use std::path::Path;
#[cfg(feature = "lsp")]
use std::sync::Arc;
#[cfg(feature = "lsp")]
use std::time::{Duration, Instant};

use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::agent_loop::tool_input_repair::with_contract_hint;
use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, PermCheck, ToolError, WriteArgs, require_and_resolve};
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
            #[cfg(feature = "lsp")]
            lsp_manager,
        }
    }
}

impl Tool for WriteTool {
    const NAME: &'static str = "write";

    type Error = ToolError;
    type Args = WriteArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "write".to_string(),
            description: with_contract_hint(
                "write",
                "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The absolute path to the file to write (must be absolute, not relative)" },
                    "content": { "type": "string", "description": "Content to write to the file" }
                },
                "required": ["path", "content"]
            }),
        }
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
        let resolved_path = require_and_resolve(
            &self.permission,
            &self.ask_tx,
            "write",
            &args.path,
            "the write path",
        )
        .await?;

        let path = Path::new(&resolved_path);
        // Phase-2 tree-sitter validation: refuse to write
        // syntactically-broken code so the model sees the error
        // in the SAME turn and self-corrects. dirge-p5fu: a purely
        // unclosed-delimiter imbalance is mechanically closed (parity
        // with the JSON truncation repair) instead of bounced back —
        // the fix is reported on the result so it's never silent. No-op
        // for unknown file types or when no `semantic-<lang>` feature is
        // built. See docs/AGENTIC_LOOP_PLAN.md §2.
        let (content, syntax_note) =
            crate::agent::tools::syntax_gate(path, &args.content).map_err(ToolError::Msg)?;
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
        // Repair-path rollback (dirge-p1ws): when syntax_gate had to auto-close
        // a truncation, snapshot the pre-write bytes so an LSP-rejected repair
        // can be reverted. Clean writes skip this entirely.
        #[cfg(feature = "lsp")]
        let repair_before: Option<Vec<u8>> = if syntax_note.is_some() && !was_creation {
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

        #[cfg(feature = "lsp")]
        {
            // A repaired write is verified by the language server; if the
            // close produced errors, the file is rolled back and the model
            // gets the diagnostics. A clean write keeps today's behavior:
            // surface diagnostics, never block (dirge-p1ws).
            let lsp_block = if syntax_note.is_some() {
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
}
