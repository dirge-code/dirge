use std::path::Path;
#[cfg(feature = "lsp")]
use std::sync::Arc;
#[cfg(feature = "lsp")]
use std::time::{Duration, Instant};

use rig::tool::PortableTool;

use crate::agent::agent_loop::tool_input_repair::with_contract_hint;
use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{
    AskSender, PermCheck, ToolError, ToolRoot, WriteArgs, require_and_resolve_rooted,
};
#[cfg(feature = "lsp")]
use crate::lsp::diagnostic;
#[cfg(feature = "lsp")]
use crate::lsp::manager::{LspManager, TouchMode};

/// How long to wait for the LSP server to publish fresh diagnostics after
/// a write. Matches opencode's `DIAGNOSTICS_FULL_WAIT_TIMEOUT_MS`. Bounded
/// so a stuck server doesn't hold up the agent's turn.
#[cfg(feature = "lsp")]
const DIAGNOSTIC_WAIT: Duration = Duration::from_secs(10);

pub struct WriteTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    cache: Option<ToolCache>,
    root: Option<ToolRoot>,
    /// When set, the tool touches the file on the LSP server after writing
    /// and appends any resulting diagnostic block to its output. `None`
    /// reproduces the pre-LSP behaviour exactly.
    #[cfg(feature = "lsp")]
    lsp_manager: Option<Arc<LspManager>>,
}

impl WriteTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        WriteTool {
            permission,
            ask_tx,
            cache: None,
            root: None,
            #[cfg(feature = "lsp")]
            lsp_manager: None,
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        cache: ToolCache,
        #[cfg(feature = "lsp")] lsp_manager: Option<Arc<LspManager>>,
    ) -> Self {
        WriteTool {
            permission,
            ask_tx,
            cache: Some(cache),
            root: None,
            #[cfg(feature = "lsp")]
            lsp_manager,
        }
    }
    pub fn rooted(mut self, root: ToolRoot) -> Self {
        self.root = Some(root);
        self
    }
}

impl PortableTool for WriteTool {
    const NAME: &'static str = "write";

    type Error = ToolError;
    type Args = WriteArgs;
    type Output = String;

    fn description(&self) -> String {
        with_contract_hint(
            "write",
            "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"The absolute path to the file to write (must be absolute, not relative)"},
                "content": {"type":"string","description":"Content to write to the file"}
            },
            "required": ["path","content"]
        })
    }

    async fn call(&self, args: WriteArgs) -> Result<String, ToolError> {
        // Reject non-absolute paths immediately with a clear error
        // (shared guard; the schema requires an absolute path).
        // Without it the tool silently resolves "1" to "{cwd}/1" and
        // creates the file, confusing the model into thinking it wrote
        // to a real project path.
        // Audit H12: require absolute + pin file operations to the canonical
        // path the permission check ran against, so a symlink swap can't
        // redirect the write to an unauthorized target.
        // dirge-4afz: a root-anchored bare filename (`/notes.md`) is our own
        // schema's fault — it demands an absolute path, and a model with no
        // directory anchor complies by prefixing a slash. Rewrite it to cwd
        // before the permission check so the check runs on the real target.
        // Rooted runs are left alone: `ToolRoot::resolve` already refuses a
        // path outside the root, which is the correct answer there.
        let requested_path = match self.root {
            Some(_) => args.path.clone(),
            None => std::env::current_dir()
                .ok()
                .and_then(|cwd| {
                    crate::agent::tools::write_guard::rewrite_root_bare_path(&args.path, &cwd)
                })
                .unwrap_or_else(|| args.path.clone()),
        };
        let rewrote_root_bare = requested_path != args.path;

        let resolved_path = require_and_resolve_rooted(
            self.root.as_ref(),
            &self.permission,
            &self.ask_tx,
            "write",
            &requested_path,
            "the write path",
        )
        .await?;

        let path = Path::new(&resolved_path);

        // dirge-4afz: a reserved Windows device name is refused everywhere.
        // `ReservedDeviceNamePolicy` covers this for every writer including the
        // shell, but the permission checker is optional (embedded / test
        // construction passes `None`), so the tool refuses on its own too.
        if crate::agent::tools::write_guard::is_reserved_device_name(path) {
            return Err(ToolError::Msg(
                crate::agent::tools::write_guard::reserved_device_message(path),
            ));
        }

        // dirge-m8d0: read the pre-write baseline once, up front, and share it
        // with the syntax gate below.
        //
        // This supersedes dirge-ytu1's lazy read, which deferred it until the
        // gate was about to reject or repair. The shrink guard needs the
        // baseline on EVERY overwrite, so a clean overwrite now pays for one
        // read of the file it is about to replace — amortized against the write
        // itself, and against destroying content no one asked to destroy. The
        // reject/repair path costs exactly what it did before; the read is
        // shared, never doubled.
        let existing = if path.exists() {
            std::fs::read_to_string(path).ok()
        } else {
            None
        };
        if let Some(before) = existing.as_deref() {
            if let Some(msg) = crate::agent::tools::write_guard::shrink_verdict(
                &resolved_path,
                before,
                &args.content,
            ) {
                return Err(ToolError::Msg(msg));
            }
        }

        // Phase-2 tree-sitter validation: refuse to write
        // syntactically-broken code so the model sees the error
        // in the SAME turn and self-corrects. dirge-p5fu: a purely
        // unclosed-delimiter imbalance is mechanically closed (parity
        // with the JSON truncation repair) instead of bounced back —
        // the fix is reported on the result so it's never silent. No-op
        // for unknown file types or when no `semantic-<lang>` feature is
        // built. See docs/AGENTIC_LOOP_PLAN.md §2.
        let (content, syntax_note) =
            crate::agent::tools::syntax_gate(path, &args.content, || existing.clone())
                .map_err(ToolError::Msg)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = content.len();
        // Line count is useful for the LLM to confirm what it wrote
        // landed; cheap to compute on the in-memory string before
        // the write. `lines()` doesn't count a trailing empty line
        // (so "a\nb\n" is 2 lines, not 3) which matches read's
        // counting convention.
        let line_count = content.lines().count();
        let was_creation = !path.exists();
        // Only a REPAIR rewrites the model's bytes; a pre-existing-error note
        // means the text went out verbatim, so there is nothing to verify.
        #[cfg_attr(not(feature = "lsp"), allow(unused_variables))]
        let was_repaired = syntax_note
            .as_ref()
            .is_some_and(crate::agent::tools::GateNote::is_repair);
        // Repair-path rollback (dirge-p1ws): when syntax_gate had to auto-close
        // a truncation, snapshot the pre-write bytes so an LSP-rejected repair
        // can be reverted. Clean writes skip this entirely.
        #[cfg(feature = "lsp")]
        let repair_before: Option<Vec<u8>> = if was_repaired && !was_creation {
            tokio::fs::read(path).await.ok()
        } else {
            None
        };
        #[cfg(feature = "lsp")]
        let write_at = Instant::now();
        // Snapshot pre-write content (or absence) for /rewind.
        crate::agent::tools::snapshots::capture(path);
        // Atomic write: tmp + fsync + rename so a crash mid-write
        // leaves the previous file content intact, not a truncated
        // half-write. `tokio::fs::write` opens with O_TRUNC and
        // writes in-place — a corruption vector on power loss /
        // OOM-kill / SIGKILL.
        crate::fs_atomic::atomic_write(path, content.as_bytes()).await?;
        crate::agent::tools::modified::mark_modified(path);
        // File mutated → invalidate cached reads/greps/listings for this turn.
        // A wholesale write means the model now knows the on-disk content, so
        // mark it read (matches vix readTrackingTools incl. write_file) — a
        // later `edit` on this path won't be gate-blocked.
        if let Some(ref cache) = self.cache {
            cache.clear();
            cache.mark_read(path);
        }

        // Path lives in the chamber banner (`╭─ WRITE ─ "<path>" ─╮`),
        // so don't repeat it. Use the extra room to surface info the
        // LLM finds actionable: bytes, line count, and whether this
        // was a new-file creation vs overwrite. The verb up front
        // disambiguates the two — previously the LLM had to infer
        // creation by reading the surrounding context.
        let verb = if was_creation { "Created" } else { "Wrote" };
        #[allow(unused_mut)]
        let mut output = format!("{} {} bytes ({} lines)", verb, bytes, line_count);

        // dirge-4afz: the rewrite is silent to the filesystem but must not be
        // silent to the model — it asked for one path and got another, and a
        // later read of the original would come back empty.
        if rewrote_root_bare {
            output.push_str(&format!(
                "\nNote: {:?} named the filesystem root with a bare filename, \
                 which is almost never intended; wrote to {} instead.",
                args.path, resolved_path
            ));
        }

        #[cfg(feature = "lsp")]
        {
            // A repaired write is verified by the language server; if the
            // close produced errors, the file is rolled back and the model
            // gets the diagnostics. A clean write keeps today's behavior:
            // surface diagnostics, never block (dirge-p1ws).
            let lsp_block = if was_repaired {
                match verify_repaired_write_or_rollback(
                    self.lsp_manager.as_ref(),
                    path,
                    repair_before,
                    was_creation,
                    write_at,
                )
                .await
                {
                    Ok(block) => block,
                    Err(feedback) => {
                        // File reverted — drop the stale cache read-mark.
                        if let Some(ref cache) = self.cache {
                            cache.clear();
                        }
                        return Err(ToolError::Msg(feedback));
                    }
                }
            } else {
                append_lsp_block(self.lsp_manager.as_ref(), path, write_at).await
            };
            crate::agent::tools::append_repair_note(&mut output, syntax_note);
            output.push_str(&lsp_block);
        }
        #[cfg(not(feature = "lsp"))]
        crate::agent::tools::append_repair_note(&mut output, syntax_note);

        Ok(output)
    }
}

/// Run `touch_file` + diagnostic-report assembly. Returns the appendable
/// block (empty string when there's nothing to surface or no manager).
/// Errors during touch/wait are intentionally swallowed — diagnostic
/// surfacing is a side-effect; the write tool's primary contract is
/// "wrote the file".
#[cfg(feature = "lsp")]
pub(crate) async fn append_lsp_block(
    manager: Option<&Arc<LspManager>>,
    path: &Path,
    after: Instant,
) -> String {
    let Some(manager) = manager else {
        return String::new();
    };
    manager
        .touch_file(
            path,
            TouchMode::AwaitPush {
                after,
                timeout: DIAGNOSTIC_WAIT,
            },
        )
        .await;
    let diagnostics = manager.all_diagnostics();
    diagnostic::build_report_block(path, &diagnostics)
}

/// Max error diagnostics echoed back when a repaired write is rolled back.
#[cfg(feature = "lsp")]
const MAX_ROLLBACK_DIAGS: usize = 8;

/// Error-severity diagnostics that justify reverting a repaired write. An
/// unspecified severity counts as an error — conservative, so a server that
/// omits severity can't let a broken repair slip through.
#[cfg(feature = "lsp")]
fn error_diagnostics(diags: &[lsp_types::Diagnostic]) -> Vec<&lsp_types::Diagnostic> {
    use lsp_types::DiagnosticSeverity;
    diags
        .iter()
        .filter(|d| matches!(d.severity, Some(DiagnosticSeverity::ERROR) | None))
        .collect()
}

/// Undo a write. Returns `true` if the on-disk file was actually rolled back:
/// the original bytes were restored (`before == Some`), or a file we created
/// this call was removed (`before == None && was_creation`). Returns `false`
/// when the file existed before but we have no snapshot to restore it from
/// (`before == None && !was_creation` — e.g. the pre-write read failed): we must
/// NOT delete it, or a transient read error would destroy the user's file. In
/// that case the repaired (likely wrong) content stays on disk and the caller
/// tells the model it wasn't reverted. Best-effort — a failure here can't make
/// things worse than the broken write already on disk.
#[cfg(feature = "lsp")]
async fn revert_write(path: &Path, before: Option<&[u8]>, was_creation: bool) -> bool {
    match before {
        Some(orig) => {
            let _ = crate::fs_atomic::atomic_write(path, orig).await;
            true
        }
        None if was_creation => {
            let _ = tokio::fs::remove_file(path).await;
            true
        }
        // Existed before, but its prior content is unknown — never delete.
        None => false,
    }
}

/// Repair-path safety net (dirge-p1ws). Called ONLY after a write whose
/// content was auto-repaired (a trailing truncation closed by
/// `repair_delimiters`). Asks the language server whether the result is
/// actually sound: a close can yield structurally-valid-but-wrong code
/// tree-sitter can't flag (e.g. a `#[test]` fn nested into another fn). If
/// the server reports error-severity diagnostics, the on-disk change is
/// ROLLED BACK to `before` (or the just-created file is removed) and the
/// errors are returned so the model fixes its own un-repaired text.
///
/// Returns `Ok(report_block)` to keep the write (the block — possibly with
/// warnings/infos — is appended to the tool output, reusing the single
/// touch+wait); `Err(feedback)` means the file was reverted and `feedback`
/// is the tool error. A clean write never calls this, so WIP/multi-file
/// states that don't yet typecheck are unaffected.
#[cfg(feature = "lsp")]
pub(crate) async fn verify_repaired_write_or_rollback(
    manager: Option<&Arc<LspManager>>,
    path: &Path,
    before: Option<Vec<u8>>,
    was_creation: bool,
    after: Instant,
) -> Result<String, String> {
    let Some(manager) = manager else {
        return Ok(String::new());
    };
    manager
        .touch_file(
            path,
            TouchMode::AwaitPush {
                after,
                timeout: DIAGNOSTIC_WAIT,
            },
        )
        .await;
    let diags = manager.diagnostics_for(path).unwrap_or_default();
    let errors = error_diagnostics(&diags);
    if errors.is_empty() {
        // Repair holds up — keep it, and surface the usual report.
        return Ok(diagnostic::build_report_block(
            path,
            &manager.all_diagnostics(),
        ));
    }

    let reverted = revert_write(path, before.as_deref(), was_creation).await;
    // Re-sync the server to the on-disk content so its diagnostics don't
    // linger (best-effort; the disk rollback already happened).
    manager.touch_file(path, TouchMode::Notify).await;

    let mut msg = String::from(if reverted {
        "Auto-repair reverted: the file was restored to its previous state and NOT modified. \
         Closing the unbalanced delimiters in your text produced these language-server errors — \
         fix your original text and resend:\n"
    } else {
        "Auto-repair failed verification, but the file's prior content was unreadable so it could \
         NOT be rolled back — the repaired (and likely wrong) content is still on disk. Closing the \
         unbalanced delimiters in your text produced these language-server errors — fix and rewrite \
         the file:\n"
    });
    for d in errors.iter().take(MAX_ROLLBACK_DIAGS) {
        msg.push_str("  ");
        msg.push_str(&diagnostic::pretty(d));
        msg.push('\n');
    }
    if errors.len() > MAX_ROLLBACK_DIAGS {
        msg.push_str(&format!(
            "  …and {} more\n",
            errors.len() - MAX_ROLLBACK_DIAGS
        ));
    }
    Err(msg)
}

#[cfg(all(test, feature = "lsp"))]
mod tests {
    use super::*;
    use crate::agent::tools::cache::ToolCache;
    use crate::lsp::manager::LspManager;
    use crate::lsp::spawn::{Spawned, Spawner};
    use futures::future::BoxFuture;
    use std::path::PathBuf;

    fn tempfile_in(dir: &Path, name: &str) -> PathBuf {
        dir.join(name)
    }

    /// Synthetic spawner — never actually invoked because the write paths
    /// we test don't have an extension the manager would claim.
    struct NopSpawner;
    impl Spawner for NopSpawner {
        fn spawn<'a>(
            &'a self,
            _server_id: &'a str,
            _root: &'a Path,
        ) -> BoxFuture<'a, std::io::Result<Spawned>> {
            Box::pin(async { Err(std::io::Error::other("not used")) })
        }
    }

    // Regression: when no LSP manager is provided, the tool's output must
    // be exactly what it was pre-LSP (just "Written N bytes to PATH").
    // The diagnostic-append code path must not perturb the no-manager case.
    #[tokio::test]
    async fn regression_no_manager_preserves_existing_output() {
        let dir = std::env::temp_dir().join(format!("dirge-write-no-mgr-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = tempfile_in(&dir, "no-mgr.txt");

        let tool = WriteTool::with_cache(None, None, ToolCache::new(), None);
        let out = tool
            .call(WriteArgs {
                path: path.to_string_lossy().into_owned(),
                content: "hello".into(),
            })
            .await
            .unwrap();
        // Path is in the chamber banner; body starts with the verb +
        // bytes + line count. Use `Created` since the test path
        // didn't exist beforehand. Single-line "hello" content → 1 line.
        assert_eq!(
            out, "Created 5 bytes (1 lines)",
            "unexpected write summary: {out}",
        );
        assert!(!out.contains("LSP errors"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // When a manager IS provided but has no diagnostics (mock spawner that
    // never gets called for the extension), the tool's output still starts
    // with the write confirmation and contains no diagnostic block.
    #[tokio::test]
    async fn manager_with_no_diagnostics_appends_nothing() {
        let dir = std::env::temp_dir().join(format!("dirge-write-with-mgr-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = tempfile_in(&dir, "with-mgr.unknown_ext");

        let manager = Arc::new(LspManager::new(Arc::new(NopSpawner), dir.clone()));
        let tool = WriteTool::with_cache(None, None, ToolCache::new(), Some(manager));

        let out = tool
            .call(WriteArgs {
                path: path.to_string_lossy().into_owned(),
                content: "hi".into(),
            })
            .await
            .unwrap();
        assert!(
            out.starts_with("Created 2 bytes") || out.starts_with("Wrote 2 bytes"),
            "expected `Created`/`Wrote 2 bytes` prefix; got: {out}",
        );
        assert!(!out.contains("LSP errors"), "got: {out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// dirge-p5fu: a write whose content has a purely unclosed-delimiter
    /// imbalance (e.g. a truncated form) is mechanically closed and the
    /// BALANCED content lands on disk, with the fix reported — instead of
    /// the write being rejected and bounced back to the model.
    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn auto_repairs_truncated_delimiters_on_write() {
        let dir = std::env::temp_dir().join(format!("dirge-write-repair-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = tempfile_in(&dir, "trunc.janet");

        let tool = WriteTool::with_cache(None, None, ToolCache::new(), None);
        let out = tool
            .call(WriteArgs {
                path: path.to_string_lossy().into_owned(),
                content: "(defn f [x]\n  (+ x 1".into(),
            })
            .await
            .unwrap();
        assert!(
            out.contains("[auto-repair]"),
            "the result must report the repair: {out}"
        );
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            on_disk, "(defn f [x]\n  (+ x 1))",
            "the balanced (repaired) content must be what got written"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Non-absolute paths (like "1", "file.txt") must be rejected
    /// immediately with a clear error. Without this guard the tool
    /// silently resolves "1" → "{cwd}/1" and creates the file, which
    /// confuses the model into retrying the same nonsense write.
    // dirge-p1ws: repair-path LSP verify + rollback.

    #[test]
    fn error_diagnostics_keeps_errors_and_unspecified() {
        use lsp_types::{Diagnostic, DiagnosticSeverity};
        let d = |sev: Option<DiagnosticSeverity>| Diagnostic {
            severity: sev,
            message: "m".into(),
            ..Default::default()
        };
        let diags = vec![
            d(Some(DiagnosticSeverity::ERROR)),
            d(Some(DiagnosticSeverity::WARNING)),
            d(Some(DiagnosticSeverity::INFORMATION)),
            d(Some(DiagnosticSeverity::HINT)),
            d(None), // unspecified severity → treated as an error
        ];
        let errs = error_diagnostics(&diags);
        assert_eq!(
            errs.len(),
            2,
            "ERROR and unspecified are kept; warning/info/hint are dropped",
        );
    }

    #[tokio::test]
    async fn revert_restores_overwrite_and_removes_new_file() {
        let dir = std::env::temp_dir().join(format!("dirge-revert-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        // Overwrite case: a rejected repair restores the original bytes.
        let p = dir.join("existing.rs");
        std::fs::write(&p, b"original").unwrap();
        std::fs::write(&p, b"broken repair").unwrap();
        assert!(revert_write(&p, Some(b"original"), false).await);
        assert_eq!(std::fs::read(&p).unwrap(), b"original");

        // Creation case: a file that didn't exist before is removed.
        let np = dir.join("new.rs");
        std::fs::write(&np, b"broken new file").unwrap();
        assert!(revert_write(&np, None, true).await);
        assert!(!np.exists(), "a newly-created file is removed on revert");

        // Unsnapshotted-overwrite case: the file existed but we have no prior
        // bytes (read failed). It must NOT be deleted — losing the user's file
        // is worse than leaving the broken repair for them to fix.
        let up = dir.join("unreadable.rs");
        std::fs::write(&up, b"repaired but wrong").unwrap();
        assert!(
            !revert_write(&up, None, false).await,
            "returns false: not reverted",
        );
        assert!(up.exists(), "an existing file is never deleted on revert");
        assert_eq!(std::fs::read(&up).unwrap(), b"repaired but wrong");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_non_absolute_path() {
        let tool = WriteTool::with_cache(None, None, ToolCache::new(), None);
        for path in ["1", "file.txt", "src/main.rs"] {
            let err = tool
                .call(WriteArgs {
                    path: path.into(),
                    content: "hello".into(),
                })
                .await
                .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("absolute path"),
                "path {path:?}: expected absolute-path rejection; got: {msg}",
            );
        }
    }

    // ── dirge-m8d0 / dirge-4afz: content and path guards, end to end ──

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dirge-write-guard-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The failure the tree-sitter gate cannot see: well-formed content that
    /// happens to be most of the file missing.
    #[tokio::test]
    async fn refuses_a_write_that_drops_most_of_an_existing_file() {
        let dir = tmp_dir("shrink");
        let path = dir.join("lib.rs");
        std::fs::write(&path, "// line\n".repeat(400)).unwrap();

        let tool = WriteTool::with_cache(None, None, ToolCache::new(), None);
        let err = tool
            .call(WriteArgs {
                path: path.to_string_lossy().into_owned(),
                content: "// line\n".repeat(50),
            })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("discarding 350"), "{msg}");
        assert!(msg.contains("\"name\": \"edit\""), "expected the edit recipe: {msg}");

        // The refusal must be a refusal — the file is untouched.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.lines().count(), 400, "file was modified anyway");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Creating a new file is never shrink-guarded, whatever its size.
    #[tokio::test]
    async fn creating_a_new_file_is_unaffected() {
        let dir = tmp_dir("create");
        let path = dir.join("new.rs");
        let tool = WriteTool::with_cache(None, None, ToolCache::new(), None);
        let out = tool
            .call(WriteArgs {
                path: path.to_string_lossy().into_owned(),
                content: "fn main() {}\n".into(),
            })
            .await
            .unwrap();
        assert!(out.starts_with("Created"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refuses_reserved_device_names() {
        let dir = tmp_dir("device");
        let tool = WriteTool::with_cache(None, None, ToolCache::new(), None);
        for name in ["nul", "COM1.txt"] {
            let err = tool
                .call(WriteArgs {
                    path: dir.join(name).to_string_lossy().into_owned(),
                    content: "x".into(),
                })
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("reserved device name"),
                "{name}: {err}"
            );
            assert!(!dir.join(name).exists(), "{name} was created anyway");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/foo.md` lands in cwd, and the result says so — the model asked for a
    /// different path than it got.
    #[tokio::test]
    async fn root_bare_path_lands_in_cwd_and_is_reported() {
        let dir = tmp_dir("rootbare");
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();

        let tool = WriteTool::with_cache(None, None, ToolCache::new(), None);
        let result = tool
            .call(WriteArgs {
                path: "/scratch-note.md".into(),
                content: "hi\n".into(),
            })
            .await;

        std::env::set_current_dir(&prev).unwrap();
        let out = result.unwrap();
        assert!(out.contains("named the filesystem root"), "{out}");
        assert!(dir.join("scratch-note.md").exists(), "not written under cwd");
        assert!(
            !std::path::Path::new("/scratch-note.md").exists(),
            "wrote to the actual filesystem root"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
