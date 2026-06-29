//! Disk-backed large-output relay for `bash` and `webfetch`
//! (and any other tool whose output can blow out the LLM
//! context). Phase 3 / part 2 of `docs/AGENTIC_LOOP_PLAN.md`.
//!
//! When a tool produces output above an inline budget we:
//!   1. write the FULL captured output (post the per-tool
//!      streaming cap — `BashTool` still tops out at 256 KiB,
//!      `WebFetchTool` at 10 MiB) to a temp file under
//!      `~/.dirge/transient/<pid>/<tool>-<unix_ts>.txt`,
//!   2. return a summary (first N lines, an ellipsis line,
//!      last M lines, line count, file path, and a hint
//!      telling the model to use the `read` tool with
//!      `offset`/`limit` to inspect the missing portion).
//!
//! Below the inline budget the relay is a no-op — output is
//! returned verbatim.
//!
//! ## Cleanup strategy
//!
//! Aged cleanup. On every write, we sweep `~/.dirge/transient/`
//! and delete files (and empty PID dirs) older than 24h. Self-
//! healing: a crashed dirge run leaves files behind, the next
//! successful write removes them. Cheap because the transient
//! tree is shallow (one dir per PID, a handful of files per
//! session).
//!
//! ## Path scheme — safe to delete
//!
//! Every file dirge writes here is named
//! `~/.dirge/transient/<pid>/<tool>-<unix_ts>.txt` and is
//! intended for one-shot LLM inspection during the same
//! session. Nothing outside the agent loop references these
//! files. Users who want to clean up immediately can
//! `rm -rf ~/.dirge/transient`; dirge will recreate it on
//! the next write.
//!
//! Files land inside `~/.dirge/` so the user's `read` tool
//! permissions (which already trust paths beneath the user's
//! cwd or `~/.dirge/`) accept reading them back.

#[cfg(test)]
#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Default inline-byte threshold (8 KiB). Output at or below
/// this size and ≤ `DEFAULT_LINE_THRESHOLD` lines is returned
/// verbatim; anything above either trips the relay.
pub const DEFAULT_INLINE_MAX_BYTES: usize = 8 * 1024;

/// Default inline-line threshold (200 lines).
pub const DEFAULT_LINE_THRESHOLD: usize = 200;

/// Number of lines kept at the head of the summary.
const HEAD_LINES: usize = 50;
/// Number of lines kept at the tail of the summary.
const TAIL_LINES: usize = 50;

/// Aged-cleanup cutoff: drop files older than 24h on each write.
const CLEANUP_MAX_AGE_SECS: u64 = 24 * 60 * 60;

/// Globally-overridable inline budgets per tool. `Config` writes
/// these once at startup via [`set_thresholds`]; the relay reads
/// them on every call. `0` means "use the default".
static BASH_INLINE_MAX: AtomicUsize = AtomicUsize::new(0);
static WEBFETCH_INLINE_MAX: AtomicUsize = AtomicUsize::new(0);
/// dirge-nmv5: inline budget for the `task` subagent tool. When a
/// subagent returns more than this many bytes, the full text is
/// relayed to `~/.dirge/transient/<pid>/task-<ts>.txt` and a
/// head/tail summary (plus a `read`-tool hint) goes back to the
/// parent agent. Prevents either silently dropping the bulk of a
/// subagent's answer OR bloating the parent context with the full
/// payload.
static TASK_INLINE_MAX: AtomicUsize = AtomicUsize::new(0);

/// Install the configured inline byte budgets. Called from the
/// agent builder once the [`Config`](crate::config::Config) is
/// loaded. `None` keeps the compiled-in default
/// ([`DEFAULT_INLINE_MAX_BYTES`]).
pub fn set_thresholds(bash: Option<usize>, webfetch: Option<usize>, task: Option<usize>) {
    if let Some(n) = bash {
        BASH_INLINE_MAX.store(n.max(1), Ordering::Relaxed);
    }
    if let Some(n) = webfetch {
        WEBFETCH_INLINE_MAX.store(n.max(1), Ordering::Relaxed);
    }
    if let Some(n) = task {
        TASK_INLINE_MAX.store(n.max(1), Ordering::Relaxed);
    }
}

/// Effective inline-byte threshold for the given tool. Looks up
/// the per-tool static override first; falls back to
/// `DEFAULT_INLINE_MAX_BYTES`.
pub fn inline_max_bytes_for(tool: &str) -> usize {
    let n = match tool {
        "bash" => BASH_INLINE_MAX.load(Ordering::Relaxed),
        "webfetch" => WEBFETCH_INLINE_MAX.load(Ordering::Relaxed),
        "task" => TASK_INLINE_MAX.load(Ordering::Relaxed),
        _ => 0,
    };
    if n == 0 { DEFAULT_INLINE_MAX_BYTES } else { n }
}

/// True when `output` is small enough to skip the relay. Below
/// the byte threshold AND below the line threshold.
fn fits_inline(output: &str, inline_max_bytes: usize) -> bool {
    if output.len() > inline_max_bytes {
        return false;
    }
    // Count newlines + (trailing line w/o newline). Fast: no
    // intermediate vec allocation.
    let line_count = if output.is_empty() {
        0
    } else {
        let nl = output.bytes().filter(|b| *b == b'\n').count();
        if output.ends_with('\n') { nl } else { nl + 1 }
    };
    line_count <= DEFAULT_LINE_THRESHOLD
}

/// Resolve the base transient directory: `~/.dirge/transient/`.
/// Falls back to the system temp dir if the user has no HOME.
pub fn transient_base() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join(".dirge").join("transient")
    } else {
        std::env::temp_dir().join("dirge-transient")
    }
}

/// Aged cleanup. Walk every PID-dir under `~/.dirge/transient/`
/// and unlink any file whose mtime is older than 24h. Empty PID
/// dirs are removed too. Best-effort: any IO error logs at
/// `debug!` and is otherwise swallowed — a stale file is a
/// nuisance, not a failure mode worth surfacing to the agent.
fn cleanup_aged(base: &Path) {
    let cutoff = match std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(CLEANUP_MAX_AGE_SECS))
    {
        Some(t) => t,
        None => return, // clock weirdness: skip
    };
    let entries = match std::fs::read_dir(base) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let inner = match std::fs::read_dir(&path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let mut still_has_children = false;
        for f in inner.flatten() {
            let fp = f.path();
            let too_old = match f.metadata().and_then(|m| m.modified()) {
                Ok(m) => m < cutoff,
                Err(_) => false,
            };
            if too_old {
                let _ = std::fs::remove_file(&fp);
            } else {
                still_has_children = true;
            }
        }
        if !still_has_children {
            let _ = std::fs::remove_dir(&path);
        }
    }
}

/// Build a transient path:
/// `~/.dirge/transient/<pid>/<tool>-<unix_ts>-<seq>.txt`.
///
/// The `seq` suffix breaks ties between two relay calls landing in
/// the same second (multi-second-clock granularity on some
/// filesystems / fast tool loops). Monotonically increasing within
/// a process.
fn build_transient_path(tool: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let ts = crate::time_util::now_unix_secs();
    transient_base()
        .join(format!("{pid}"))
        .join(format!("{tool}-{ts}-{seq}.txt"))
}

/// Format a head+tail summary plus a `read` hint.
///
/// `header_note` is prepended outside the summary block — e.g.
/// `bash` uses it to surface the exit code. May be empty.
fn format_summary(tool: &str, full: &str, path: &Path, header_note: &str) -> String {
    let lines: Vec<&str> = full.split_inclusive('\n').collect();
    let total = lines.len();

    let head_end = HEAD_LINES.min(total);
    let head: String = lines[..head_end].concat();

    let tail_start = total.saturating_sub(TAIL_LINES).max(head_end);
    let tail: String = lines[tail_start..].concat();
    let elided = tail_start.saturating_sub(head_end);

    let path_display = path.display();
    let mut out = String::new();
    if !header_note.is_empty() {
        out.push_str(header_note);
        if !header_note.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&head);
    if !head.ends_with('\n') && !head.is_empty() {
        out.push('\n');
    }
    if elided > 0 {
        out.push_str(&format!("[… {elided} lines elided …]\n"));
    }
    out.push_str(&tail);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "\n[{tool} output relayed: {total} lines, {bytes} bytes total. \
         Full output stored at {path_display}. \
         Use the `read` tool with `offset`/`limit` to inspect specific portions.]",
        bytes = full.len(),
    ));
    out
}

/// Relay outcome — either inline output, or a head/tail summary
/// plus the path to the full content on disk.
#[derive(Debug)]
pub struct RelayOutcome {
    /// Text to feed back to the LLM (inline or summarized).
    pub text: String,
    /// `Some(path)` when the relay fired; `None` for the
    /// inline-passthrough case. Callers currently only inspect
    /// this from tests / observability; the LLM-facing path lives
    /// inside `text`.
    #[allow(dead_code)]
    pub relayed_to: Option<PathBuf>,
}

/// Apply the relay policy.
///
/// If `output` fits within the configured inline budget, return
/// it unchanged with `relayed_to = None`. Otherwise:
///   1. Write the full output to a transient file under
///      `~/.dirge/transient/<pid>/`.
///   2. Sweep the transient tree for files >24h old.
///   3. Return a head/tail summary with a `read`-tool hint and
///      the transient path.
///
/// `header_note` is prepended to the summary outside the head
/// block. `bash` uses it for `Exit code: N`. Pass `""` if
/// nothing to prepend.
///
/// Errors during the disk write degrade gracefully: we still
/// return a summary (with no path), the LLM just can't recover
/// the missing portion. We never fail the tool call over relay
/// IO — dropping output is strictly better than dropping the
/// whole turn.
pub fn relay_if_large(tool: &str, output: String, header_note: &str) -> RelayOutcome {
    let inline_max = inline_max_bytes_for(tool);
    if fits_inline(&output, inline_max) {
        let mut text = output;
        if !header_note.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(header_note);
        }
        return RelayOutcome {
            text,
            relayed_to: None,
        };
    }

    let path = build_transient_path(tool);
    let mut wrote_ok = false;
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_ok()
    {
        wrote_ok = std::fs::write(&path, output.as_bytes()).is_ok();
    }

    // Best-effort cleanup of aged transient files. Cheap (one
    // shallow walk of `~/.dirge/transient`) and self-healing.
    cleanup_aged(&transient_base());

    let summary_path: &Path = &path;
    let text = format_summary(tool, &output, summary_path, header_note);
    RelayOutcome {
        text,
        relayed_to: if wrote_ok { Some(path) } else { None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that read or mutate the shared `BASH_INLINE_MAX`
    /// global so a transient override in one test can't be observed
    /// mid-computation by another running in parallel (dirge-zk60).
    static THRESHOLD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Short output (well under both byte + line thresholds)
    /// passes through unchanged.
    #[test]
    fn short_output_passes_through() {
        let out = "hello\nworld\n".to_string();
        let outcome = relay_if_large("bash", out.clone(), "");
        assert!(outcome.relayed_to.is_none(), "expected no relay");
        assert_eq!(outcome.text, out);
    }

    /// Header note is appended inline for short outputs.
    #[test]
    fn short_output_appends_header_note() {
        let outcome = relay_if_large("bash", "ok\n".to_string(), "Exit code: 0");
        assert!(outcome.relayed_to.is_none());
        assert!(outcome.text.contains("Exit code: 0"));
        assert!(outcome.text.contains("ok"));
    }

    /// Output at the byte threshold (exactly `inline_max_bytes`)
    /// still passes through — `<=` not `<`.
    ///
    /// Uses an unknown tool name so the parallel
    /// `config_override_changes_threshold` test (which mutates
    /// the `bash` global) can't race us. Unknown tools always
    /// resolve to `DEFAULT_INLINE_MAX_BYTES`.
    #[test]
    fn at_byte_threshold_passes_through() {
        let inline_max = inline_max_bytes_for("nonexistent-tool");
        let payload: String = "x".repeat(inline_max);
        let outcome = relay_if_large("nonexistent-tool", payload, "");
        assert!(
            outcome.relayed_to.is_none(),
            "exact-threshold output must not relay; got relay at {:?}",
            outcome.relayed_to,
        );
    }

    /// One byte over the threshold trips the relay.
    ///
    /// Same race-avoidance rationale as
    /// `at_byte_threshold_passes_through`: use an unknown tool
    /// name so `BASH_INLINE_MAX` overrides in parallel tests
    /// can't shift the threshold under us.
    #[test]
    fn one_byte_over_threshold_relays() {
        let inline_max = inline_max_bytes_for("nonexistent-tool");
        let payload: String = "x".repeat(inline_max + 1);
        let outcome = relay_if_large("nonexistent-tool", payload, "");
        assert!(
            outcome.relayed_to.is_some(),
            "over-threshold output must relay",
        );
        let path = outcome.relayed_to.unwrap();
        assert!(path.exists(), "transient file must exist on disk");
        let meta = std::fs::metadata(&path).expect("read meta");
        assert_eq!(
            meta.len() as usize,
            inline_max + 1,
            "transient file should hold the full payload",
        );
        // Cleanup so this test doesn't leave debris.
        let _ = std::fs::remove_file(&path);
    }

    /// Output above the line threshold trips the relay even
    /// when the byte total is small.
    #[test]
    fn line_threshold_trips_relay() {
        let lines: Vec<String> = (0..DEFAULT_LINE_THRESHOLD + 5)
            .map(|i| format!("line {i}"))
            .collect();
        let payload = lines.join("\n");
        let outcome = relay_if_large("bash", payload.clone(), "");
        assert!(
            outcome.relayed_to.is_some(),
            "line-threshold should trip relay even at low byte count",
        );
        // Summary contains hint + line count.
        assert!(
            outcome.text.contains("read"),
            "summary should mention `read` tool"
        );
        assert!(
            outcome.text.contains("elided"),
            "summary should mention elided lines",
        );
        if let Some(p) = outcome.relayed_to {
            let _ = std::fs::remove_file(&p);
        }
    }

    /// Summary contains head + tail + hint pointing at the full file.
    #[test]
    fn summary_contains_head_tail_and_hint() {
        let mut lines: Vec<String> = Vec::new();
        for i in 0..500 {
            lines.push(format!("LINE{i}"));
        }
        let payload = lines.join("\n");
        let outcome = relay_if_large("bash", payload, "");
        let text = &outcome.text;
        // Head should include LINE0.
        assert!(text.contains("LINE0"), "summary missing head line");
        // Tail should include LINE499.
        assert!(text.contains("LINE499"), "summary missing tail line");
        // Middle line (LINE250) should be elided.
        assert!(!text.contains("LINE250"), "middle line should be elided");
        // Hint must mention `read`.
        assert!(text.contains("`read`"), "hint missing `read` reference");
        // Path hint must mention the transient path.
        assert!(
            text.contains("~/.dirge")
                || text.contains(".dirge/transient")
                || text.contains("/transient/")
                || text.contains("dirge-transient"),
            "hint should reference the transient directory: {text}",
        );
        if let Some(p) = outcome.relayed_to {
            let _ = std::fs::remove_file(&p);
        }
    }

    /// Header note (bash exit code) renders at the top of the
    /// summary block.
    #[test]
    fn relay_prepends_header_note() {
        let _guard = THRESHOLD_LOCK.lock_ignore_poison();
        let payload: String = "x".repeat(inline_max_bytes_for("bash") + 1);
        let outcome = relay_if_large("bash", payload, "Exit code: 137");
        assert!(outcome.text.starts_with("Exit code: 137"));
        if let Some(p) = outcome.relayed_to {
            let _ = std::fs::remove_file(&p);
        }
    }

    /// Config-driven override: tightening the bash threshold
    /// makes shorter outputs trip the relay.
    #[test]
    fn config_override_changes_threshold() {
        let _guard = THRESHOLD_LOCK.lock_ignore_poison();
        // Snapshot whatever the global was so we can restore
        // it for other parallel tests.
        let prev = BASH_INLINE_MAX.load(Ordering::Relaxed);
        BASH_INLINE_MAX.store(16, Ordering::Relaxed);

        let payload = "x".repeat(32); // 32 bytes > 16
        let outcome = relay_if_large("bash", payload, "");
        assert!(
            outcome.relayed_to.is_some(),
            "16-byte threshold should relay 32-byte payload",
        );
        if let Some(p) = outcome.relayed_to {
            let _ = std::fs::remove_file(&p);
        }

        // Restore — other tests may rely on the default.
        BASH_INLINE_MAX.store(prev, Ordering::Relaxed);
    }

    /// Aged-cleanup: files in `~/.dirge/transient/<pid>/` whose
    /// mtime is >24h old get removed on the next write. Empty
    /// PID dirs are also removed.
    #[test]
    fn aged_cleanup_removes_stale_files() {
        let base = transient_base();
        let pid_dir = base.join("test-aged-cleanup-pid-9999");
        let _ = std::fs::remove_dir_all(&pid_dir);
        std::fs::create_dir_all(&pid_dir).expect("mkdir");
        let stale = pid_dir.join("bash-1-1.txt");
        std::fs::write(&stale, b"old").expect("write");
        // Set mtime to 48h ago via filetime if available, else
        // fall back to skipping the assertion.
        let two_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 60 * 60);
        // Use libc/utimensat indirectly via std::fs::set_file_times (1.75+).
        if std::fs::File::open(&stale)
            .and_then(|f| {
                f.set_modified(two_days_ago)?;
                Ok(())
            })
            .is_err()
        {
            // Best-effort: skip if mtime setting unsupported.
            let _ = std::fs::remove_dir_all(&pid_dir);
            return;
        }

        cleanup_aged(&base);
        assert!(
            !stale.exists(),
            "stale file should have been removed by cleanup_aged",
        );
        // PID dir was empty after cleanup → also removed.
        assert!(
            !pid_dir.exists(),
            "empty pid dir should have been removed by cleanup_aged",
        );
    }
}
