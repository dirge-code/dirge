//! Pre-finalization verifier gate (F6).
//!
//! Backs the "verify before done" discipline with a *mechanism*, not just
//! prose. It watches the run for two things — did the agent edit a CODE
//! file, and did it run a build/test command and did that **pass or
//! fail** — and at the finalization boundary injects one soft nudge when
//! the work looks unverified or broken:
//!
//!   - edited code + a build/test command **failed**  → "fix the red build"
//!   - edited code + **no** build/test command ran    → "verify it works"
//!   - edited code + a build/test command **passed**  → silent (confident)
//!
//! Cheap and signal-based: no extra LLM call. Outcome is read from the
//! tool result post-execution (bash appends `Exit code: N` on non-zero
//! exit), so a failing test/build is detected without parsing semantics.
//! Bounded to fire at most once per run (can't loop). Self-contained;
//! lives behind `LoopConfig.verifier` (None = off, byte-identical).

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::sync::{Arc, Mutex};

use super::message::{LoopMessage, UserMessage};
use super::result::LoopToolResult;

/// Nudge when code was edited but no build/test command ran.
const VERIFY_NUDGE: &str = "[verify-before-done] You changed code this run but didn't run the tests or build to check it. Verify it works before reporting done — or, if there's nothing to run or you verified another way, say so briefly and finish. Don't re-edit just to look busy.";

/// Nudge when a build/test command failed after a code change.
const FAILED_NUDGE: &str = "[verify-before-done] Your last build or test command failed after you changed code. Don't report done on a red build — fix the failure. If it's pre-existing or expected, say so explicitly before finishing.";

/// Per-run verifier gate. See module docs.
#[derive(Debug)]
pub struct VerifierGate {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// A mutating file tool touched a code-extension path this run.
    edited_code: bool,
    /// A build/test command ran this run (any of them).
    ran_verification: bool,
    /// Outcome of the MOST RECENT build/test command (latest wins, so a
    /// fix-then-rerun-green sequence clears an earlier failure).
    verification_failed: bool,
    /// A nudge has already fired — never fire again (bounds the loop).
    fired: bool,
}

impl VerifierGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
        })
    }

    /// Record a finished tool call (called post-execution with the
    /// result). Flags a code edit when a mutating file tool touched a
    /// code-extension path; for a `bash` build/test command, records
    /// whether it passed or failed.
    pub fn record_outcome(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        result: &LoopToolResult,
        is_error: bool,
    ) {
        let mut inner = self.inner.lock_ignore_poison();
        match tool_name {
            "write" | "edit" | "apply_patch" => {
                if touches_code_file(args) {
                    inner.edited_code = true;
                }
            }
            "bash" => {
                let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                if is_verification_command(command) {
                    inner.ran_verification = true;
                    // Latest outcome wins.
                    inner.verification_failed = is_error || result_indicates_failure(result);
                }
            }
            _ => {}
        }
    }

    /// Finalization seam: returns a one-time nudge when code was changed
    /// and either a build/test failed or none ran. Empty when verified
    /// green (or nothing was edited). Fires at most once per run.
    pub fn check_before_finalize(&self) -> Vec<LoopMessage> {
        let mut inner = self.inner.lock_ignore_poison();
        if inner.fired || !inner.edited_code {
            return Vec::new();
        }
        let nudge = if inner.verification_failed {
            Some(FAILED_NUDGE)
        } else if !inner.ran_verification {
            Some(VERIFY_NUDGE)
        } else {
            None // ran a build/test and it passed → confident, stay silent
        };
        match nudge {
            Some(text) => {
                inner.fired = true;
                vec![LoopMessage::User(UserMessage {
                    content: text.to_string(),
                })]
            }
            None => Vec::new(),
        }
    }
}

/// Concatenate the text blocks of a tool result for failure scanning.
fn result_text(result: &LoopToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A bash result indicates failure when the harness appended an
/// `Exit code: N` line — bash only adds it on a non-zero exit
/// (`bash.rs`), so its presence is a reliable failure signal.
fn result_indicates_failure(result: &LoopToolResult) -> bool {
    result_text(result).contains("Exit code:")
}

/// Heuristic: does this shell command look like a build/test/check?
/// Broad on purpose — recognizing more commands as "verification" means
/// the gate stays silent rather than nagging, while the precise failure
/// signal (exit code) still catches a red build.
fn is_verification_command(command: &str) -> bool {
    const MARKERS: &[&str] = &[
        "test", "build", "check", "lint", "compile", "cargo", "npm", "pnpm", "yarn", "pytest",
        "tox", "make", "gradle", "mvn", "ctest", "cmake", "rustc", "tsc", "jest", "vitest",
        "mocha", "clippy", "go vet", "go run",
    ];
    let lower = command.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

/// True if any path argument names a source-code file (by extension).
/// Looks at top-level `path` / `file_path` / `file` and `apply_patch`'s
/// `operations[].path`.
fn touches_code_file(args: &serde_json::Value) -> bool {
    let Some(obj) = args.as_object() else {
        return false;
    };
    let mut paths: Vec<&str> = Vec::new();
    for key in ["path", "file_path", "file"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            paths.push(s);
        }
    }
    if let Some(ops) = obj.get("operations").and_then(|v| v.as_array()) {
        for op in ops {
            if let Some(s) = op.get("path").and_then(|v| v.as_str()) {
                paths.push(s);
            }
        }
    }
    paths.iter().any(|p| is_code_path(p))
}

/// Source-code file extensions. A change to one of these is "editing
/// code"; docs/config (md, txt, json, toml, …) deliberately don't count,
/// so a doc-only edit never triggers the verify nudge.
const CODE_EXTS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "rb", "java", "kt", "kts", "c", "h",
    "cc", "cpp", "hpp", "cxx", "cs", "swift", "php", "scala", "clj", "cljs", "cljc", "ex", "exs",
    "sh", "bash", "lua", "pl", "hs", "ml", "sql", "vue", "svelte",
];

fn is_code_path(path: &str) -> bool {
    match path.rsplit_once('.') {
        Some((_, ext)) => CODE_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok_result() -> LoopToolResult {
        LoopToolResult {
            content: vec![json!({"type": "text", "text": "ok"})],
            details: json!(null),
            terminate: None,
        }
    }

    fn failed_result() -> LoopToolResult {
        // Mirrors bash's non-zero-exit output: the harness appends an
        // "Exit code: N" line.
        LoopToolResult {
            content: vec![json!({"type": "text", "text": "test failed\nExit code: 101"})],
            details: json!(null),
            terminate: None,
        }
    }

    fn nudge(gate: &VerifierGate) -> Option<String> {
        gate.check_before_finalize()
            .into_iter()
            .next()
            .map(|m| match m {
                LoopMessage::User(u) => u.content,
                _ => panic!("expected user message"),
            })
    }

    #[test]
    fn edited_code_without_running_nudges_to_verify() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        let n = nudge(&g).expect("should nudge");
        assert!(n.contains("didn't run the tests"), "verify nudge: {n}");
    }

    #[test]
    fn edited_code_then_passing_test_is_silent() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert!(
            nudge(&g).is_none(),
            "passing verification should stay silent"
        );
    }

    #[test]
    fn edited_code_then_failing_test_nudges_to_fix() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &failed_result(),
            false,
        );
        let n = nudge(&g).expect("should nudge on red build");
        assert!(n.contains("failed"), "fix-it nudge: {n}");
        assert!(
            n.contains("red build"),
            "should mention not finishing on red: {n}"
        );
    }

    #[test]
    fn rerun_green_after_failure_clears_the_nudge() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &failed_result(),
            false,
        );
        // Fix, re-run, now green — latest outcome wins.
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert!(
            nudge(&g).is_none(),
            "a subsequent green run should clear the failure"
        );
    }

    #[test]
    fn non_verification_command_does_not_count_as_verified() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        // `ls` is not a build/test command → still unverified.
        g.record_outcome("bash", &json!({"command": "ls -la"}), &ok_result(), false);
        let n = nudge(&g).expect("ls is not verification");
        assert!(n.contains("didn't run the tests"));
    }

    #[test]
    fn tool_execution_error_counts_as_failure() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        // is_error=true (tool blew up) on a verification command → failed.
        g.record_outcome("bash", &json!({"command": "make test"}), &ok_result(), true);
        let n = nudge(&g).expect("errored verification is a failure");
        assert!(n.contains("failed"));
    }

    #[test]
    fn doc_only_edit_never_nudges() {
        let g = VerifierGate::new();
        g.record_outcome("write", &json!({"path": "README.md"}), &ok_result(), false);
        assert!(nudge(&g).is_none());
    }

    #[test]
    fn no_edits_never_nudges() {
        let g = VerifierGate::new();
        g.record_outcome("read", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        assert!(nudge(&g).is_none());
    }

    #[test]
    fn nudge_fires_at_most_once() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        assert!(nudge(&g).is_some());
        assert!(nudge(&g).is_none(), "bounded to once per run");
    }

    #[test]
    fn apply_patch_with_code_operation_counts_as_edit() {
        let g = VerifierGate::new();
        g.record_outcome(
            "apply_patch",
            &json!({"operations": [{"type": "update", "path": "src/lib.rs"}]}),
            &ok_result(),
            false,
        );
        assert!(nudge(&g).is_some());
    }

    #[test]
    fn is_code_path_recognizes_common_extensions() {
        assert!(is_code_path("src/main.rs"));
        assert!(is_code_path("app/Foo.TS"));
        assert!(!is_code_path("README.md"));
        assert!(!is_code_path("Makefile"));
    }
}
