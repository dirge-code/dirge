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

/// A read-only snapshot of what the run did toward verifying its code
/// changes, derived from the same signals that drive the cheap nudge.
/// Fed to the LLM critic so it can be pickier about compile/lint/test
/// without re-deriving the signal (dirge-6q3w). The `edited_code`
/// precondition is baked in: [`VerificationStatus::NoCodeEdited`] means
/// "verification not applicable this run" so the critic adds no pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    /// No code file was edited this run — verification is N/A.
    NoCodeEdited,
    /// Code was edited and the most recent build/test command passed.
    VerifiedGreen,
    /// Code was edited and the most recent build/test command failed.
    VerifiedRed,
    /// Code was edited but no build/test/lint command was detected.
    Unverified,
}

/// Display tag prefixing both verifier nudges. The UI keys on this to attribute
/// the message to the system/critic rather than the user (it's injected as a
/// user-role message so the model responds) [dirge-i75f]. The `*_NUDGE`
/// constants below embed it literally.
pub const VERIFY_TAG: &str = "[verify-before-done]";

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
            // `edit_minified` is a real source-mutating tool (dirge-b1rr) —
            // without it here, an agent that edits only via edit_minified
            // never sets `edited_code` and the verify-before-done gate stays
            // silent on unverified changes.
            "write" | "edit" | "apply_patch" | "edit_minified" => {
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

    /// Read-only verification snapshot for the LLM critic (dirge-6q3w).
    /// Unlike [`check_before_finalize`], this never mutates the gate (it
    /// doesn't spend the one-shot nudge), so the cheap nudge and the
    /// pickier critic can both consult it in the same finalization.
    pub fn status(&self) -> VerificationStatus {
        let inner = self.inner.lock_ignore_poison();
        if !inner.edited_code {
            VerificationStatus::NoCodeEdited
        } else if inner.ran_verification && inner.verification_failed {
            VerificationStatus::VerifiedRed
        } else if inner.ran_verification {
            VerificationStatus::VerifiedGreen
        } else {
            VerificationStatus::Unverified
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
                vec![LoopMessage::User(UserMessage::text(text))]
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

/// A bash result indicates failure when the harness appended its
/// `Exit code: N` line — bash emits that line (as a standalone line)
/// ONLY on a non-zero exit (`bash.rs`). Match it anchored to the start
/// of a line and require N to parse to a non-zero integer, so a green
/// run whose own output merely contains the text `Exit code: 0` or
/// mentions `Exit code:` in prose isn't misread as red (dirge-fc40).
fn result_indicates_failure(result: &LoopToolResult) -> bool {
    result_text(result).lines().any(exit_code_line_is_failure)
}

/// True iff `line` is the harness's non-zero exit marker: it begins
/// (after trimming) with `Exit code:` and the remainder parses to a
/// non-zero integer. `Exit code: 0` and non-numeric remainders are not
/// failures.
fn exit_code_line_is_failure(line: &str) -> bool {
    line.trim()
        .strip_prefix("Exit code:")
        .and_then(|rest| rest.trim().parse::<i64>().ok())
        .is_some_and(|code| code != 0)
}

/// Heuristic: does this shell command look like a build/test/check?
/// Broad on purpose — recognizing more commands as "verification" means
/// the gate stays silent rather than nagging.
///
/// Markers are matched on whole shell WORDS, not as substrings, so
/// `git checkout` no longer matches `check` and `ls tests/` no longer
/// matches `test`. A segment carrying a non-building subcommand
/// (`npm install`, `cargo add`) is disqualified outright even though its
/// tool name is a marker (dirge-eg37). Splitting on `&& || ; |` and
/// newlines means one real build in a chain still counts.
fn is_verification_command(command: &str) -> bool {
    command
        .split(['&', '|', ';', '\n'])
        .any(segment_is_verification)
}

/// Build/test/lint tool + subcommand words. Includes linters/formatters
/// invoked by bare name (`eslint .`, `golangci-lint run`).
const WORD_MARKERS: &[&str] = &[
    "test",
    "build",
    "check",
    "lint",
    "compile",
    "cargo",
    "npm",
    "pnpm",
    "yarn",
    "pytest",
    "tox",
    "make",
    "gradle",
    "mvn",
    "ctest",
    "cmake",
    "rustc",
    "tsc",
    "jest",
    "vitest",
    "mocha",
    "clippy",
    "eslint",
    "golangci-lint",
    "prettier",
    "ruff",
    "flake8",
    "mypy",
    "shellcheck",
    "rubocop",
];

/// Subcommands that do no building/testing. Their presence disqualifies
/// the segment even when the tool name (npm/cargo/yarn) is a marker.
const NON_VERIFY: &[&str] = &["checkout", "install", "add", "remove", "uninstall"];

/// Two-word markers whose leading word isn't a marker on its own.
const PAIR_MARKERS: &[(&str, &str)] = &[("go", "vet"), ("go", "run"), ("go", "test")];

fn segment_is_verification(segment: &str) -> bool {
    let tokens: Vec<String> = segment
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .collect();
    if tokens.iter().any(|t| NON_VERIFY.contains(&t.as_str())) {
        return false;
    }
    // Whole-word markers. A `--check`-style flag is its dash-stripped
    // word, so `prettier --check .` and `cmake --build` register.
    if tokens
        .iter()
        .any(|t| WORD_MARKERS.contains(&t.trim_start_matches('-')))
    {
        return true;
    }
    if tokens
        .windows(2)
        .any(|w| PAIR_MARKERS.contains(&(w[0].as_str(), w[1].as_str())))
    {
        return true;
    }
    // The command word may be a path to a repo script: match markers
    // inside its basename, split on `-`/`_`/`.`, accepting a plural form,
    // so `./run-tests.sh` and `scripts/lint.sh` register. Only the
    // executed word gets this treatment — an argument like `ls tests/`
    // must not count (dirge-eg37).
    command_word(&tokens).is_some_and(script_name_is_verification)
}

/// First token that isn't a `VAR=value` environment prefix.
fn command_word(tokens: &[String]) -> Option<&str> {
    tokens.iter().map(|t| t.as_str()).find(|t| !t.contains('='))
}

/// True when a path-shaped command word (`./run-tests.sh`,
/// `scripts/lint.sh`) names a verification script: its basename, split on
/// `-`/`_`/`.`, carries a marker word (singular or plural).
fn script_name_is_verification(token: &str) -> bool {
    if !token.contains('/') {
        return false;
    }
    let basename = token.rsplit('/').next().unwrap_or(token);
    basename.split(['-', '_', '.']).any(|piece| {
        WORD_MARKERS.contains(&piece)
            || piece
                .strip_suffix('s')
                .is_some_and(|p| WORD_MARKERS.contains(&p))
    })
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
                LoopMessage::User(u) => u.text_joined(),
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

    /// dirge-b1rr: an `edit_minified` change to a code file must count as
    /// a code edit so the verify-before-done gate fires.
    #[test]
    fn edit_minified_counts_as_a_code_edit() {
        let g = VerifierGate::new();
        g.record_outcome(
            "edit_minified",
            &json!({"path": "src/auth.rs"}),
            &ok_result(),
            false,
        );
        let n = nudge(&g).expect("edit_minified should arm the verify nudge");
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
    fn status_reflects_run_signals() {
        let g = VerifierGate::new();
        assert_eq!(g.status(), VerificationStatus::NoCodeEdited);

        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        assert_eq!(g.status(), VerificationStatus::Unverified);

        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &failed_result(),
            false,
        );
        assert_eq!(g.status(), VerificationStatus::VerifiedRed);

        // Latest outcome wins — fix then re-run green.
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert_eq!(g.status(), VerificationStatus::VerifiedGreen);
    }

    /// `status()` must NOT spend the one-shot nudge — the cheap gate and
    /// the pickier critic both read it in the same finalization.
    #[test]
    fn status_does_not_consume_the_nudge() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        assert_eq!(g.status(), VerificationStatus::Unverified);
        // Reading status repeatedly leaves the nudge intact.
        let _ = g.status();
        assert!(nudge(&g).is_some(), "status() must not arm `fired`");
    }

    #[test]
    fn is_code_path_recognizes_common_extensions() {
        assert!(is_code_path("src/main.rs"));
        assert!(is_code_path("app/Foo.TS"));
        assert!(!is_code_path("README.md"));
        assert!(!is_code_path("Makefile"));
    }

    /// Build a bash result with an arbitrary text body.
    fn bash_result(text: &str) -> LoopToolResult {
        LoopToolResult {
            content: vec![json!({"type": "text", "text": text})],
            details: json!(null),
            terminate: None,
        }
    }

    /// dirge-fc40: a green run whose own output contains "Exit code: 0"
    /// (a wrapper, a status echo) must NOT be read as a red build. Only
    /// the harness's non-zero marker counts.
    #[test]
    fn echoed_exit_code_zero_is_not_a_failure() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "make test"}),
            &bash_result("make test\nall passed\nExit code: 0"),
            false,
        );
        assert_eq!(g.status(), VerificationStatus::VerifiedGreen);
        assert!(nudge(&g).is_none(), "echoed 'Exit code: 0' must stay green");
    }

    /// dirge-fc40: "Exit code:" in prose (not the harness's standalone
    /// non-zero marker) must not fabricate a failure.
    #[test]
    fn exit_code_in_prose_is_not_a_failure() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &bash_result("the wrapper prints 'Exit code: N' on error\ndone"),
            false,
        );
        assert_eq!(g.status(), VerificationStatus::VerifiedGreen);
    }

    /// The genuine harness marker (standalone non-zero line) is still a
    /// failure regardless of where it lands in the buffer (inline appends
    /// it last; the output relay prepends it first).
    #[test]
    fn harness_nonzero_marker_is_a_failure_anywhere() {
        for text in ["boom\nExit code: 101", "Exit code: 137\nhead\ntail"] {
            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            g.record_outcome(
                "bash",
                &json!({"command": "cargo test"}),
                &bash_result(text),
                false,
            );
            assert_eq!(
                g.status(),
                VerificationStatus::VerifiedRed,
                "non-zero marker in {text:?} should be red"
            );
        }
    }

    /// dirge-eg37: `git checkout` / `npm install` / `cargo add` / `ls
    /// tests/` must not be mistaken for a build/test because a marker
    /// appears as a substring or as the tool name of a non-building
    /// subcommand. A code edit followed only by these stays Unverified.
    #[test]
    fn non_build_subcommands_are_not_verification() {
        for cmd in [
            "git checkout main",
            "npm install",
            "cargo add serde",
            "ls tests/",
            "yarn add left-pad",
        ] {
            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            g.record_outcome("bash", &json!({"command": cmd}), &ok_result(), false);
            assert_eq!(
                g.status(),
                VerificationStatus::Unverified,
                "`{cmd}` must not count as verification"
            );
        }
    }

    /// Linters/formatters invoked by name and repo test scripts are
    /// verification even though no marker appears as a standalone word:
    /// `eslint .`, `golangci-lint run`, `prettier --check .`,
    /// `./run-tests.sh`.
    #[test]
    fn linters_and_scripts_count_as_verification() {
        for cmd in [
            "eslint .",
            "golangci-lint run",
            "prettier --check .",
            "./run-tests.sh",
            "scripts/lint.sh --fast",
        ] {
            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            g.record_outcome("bash", &json!({"command": cmd}), &ok_result(), false);
            assert_eq!(
                g.status(),
                VerificationStatus::VerifiedGreen,
                "`{cmd}` should register as verification"
            );
        }
    }

    /// dirge-eg37: real build/test/lint commands still register.
    #[test]
    fn real_build_commands_still_count() {
        for cmd in [
            "cargo test",
            "make check",
            "npm run build",
            "go vet ./...",
            "pytest -q",
            "RUST_LOG=debug cargo clippy",
        ] {
            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            g.record_outcome("bash", &json!({"command": cmd}), &ok_result(), false);
            assert_eq!(
                g.status(),
                VerificationStatus::VerifiedGreen,
                "`{cmd}` should register as verification"
            );
        }
    }
}
