//! What a tool call actually LANDED (dirge-e31n.5).
//!
//! # The gap
//!
//! A tool result is success-or-error text. That answers "did the tool report a
//! problem", which is not the question the next turn needs answered after a
//! turn is cut short. `recovery.rs` classifies transport errors for retry and
//! `heal.rs` repairs unpaired tool calls on load, but neither tells the MODEL
//! what reached the disk — so it either re-runs an effect that already
//! committed, or assumes work that never happened.
//!
//! The two failures are not symmetric. Assuming work that never happened
//! produces a wrong report. Re-running a committed effect produces a second
//! `git push`, a doubled append, a duplicate issue. So where this cannot tell,
//! it says [`SideEffect::Unknown`] and lets the model go look.
//!
//! # Why the classification is central, not per-tool
//!
//! The obvious design puts a `side_effect` on each tool's result, since each
//! tool knows itself best. That is ~30 implementations of a rule with no
//! shared enforcement, which is the shape that drifted the storm breaker's
//! mutating-tool list ([`crate::permission::engine::tool_operation`] gained
//! `edit_minified` only after minified edits had been treated as non-mutating
//! for a release — dirge-b1rr).
//!
//! Instead the answer is a total function of two things the dispatcher already
//! has: the tool's [`Operation`], and whether the call RAN TO COMPLETION.
//! Both come from existing single sources of truth — `tool_operation` and
//! [`super::tool_error_class`] — so there is no third list to drift.
//!
//! # Why a failed call is `Unknown`, not `NoEffect`
//!
//! Tempting: a `write` that errored did not write, so it had no effect. True
//! for `write`, and false in general. `bash` exiting non-zero ran the command
//! first and may have written half a file. `apply_patch` returns `Err` for a
//! batch that failed partway, having already applied the earlier files
//! (dirge-tc9l). Getting this right per-tool means the exception list this
//! module exists to avoid, and getting it wrong means telling the model
//! "nothing happened" about a file that changed.
//!
//! So: an error means the tool reported failure, NOT that nothing happened.
//! One rule, no exceptions, never claims more than it knows. The distinction
//! that carries the information is `NoEffect` vs `Unknown` vs `Committed`, and
//! all three stay reachable.

use crate::permission::engine::tool_operation;
use crate::permission::engine::types::Operation;

use super::tool_error_class::{ErrorClass, classify};

/// Whether a tool call's effect on the world reached the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SideEffect {
    /// The call ran to completion and its operation can change things. Treat
    /// the change as real: re-running it will apply it a second time.
    Committed,
    /// The call could have changed things and was cut short, or reported an
    /// error after work that may have partly landed. **Nothing here is
    /// guaranteed either way** — this is the state that requires looking.
    Unknown,
    /// The call cannot change anything, whatever happened to it. A read that
    /// timed out still read nothing.
    NoEffect,
}

impl SideEffect {
    /// Stable lowercase wire name for the rendered handoff and telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            SideEffect::Committed => "committed",
            SideEffect::Unknown => "unknown",
            SideEffect::NoEffect => "no_effect",
        }
    }
}

/// How far a tool call got, independent of whether it liked the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// Ran and reported success.
    Ok,
    /// Ran and reported an error. The call COMPLETED — the tool had its turn
    /// and said no.
    Failed,
    /// Did not finish: cancelled mid-flight, timed out, or lost its
    /// connection. Whatever it had done by then, it did.
    CutOff,
}

/// Sentinel emitted by the dispatcher when the abort signal wins the race
/// against a tool's own future. Lives here rather than as a bare string at the
/// two sites that care, and `tools.rs` builds its abort result FROM this
/// constant, so the producer and the reader cannot drift.
///
/// Same pattern and same reason as
/// [`super::tools::SYNTAX_CHECK_PREFIX`]: the dispatcher needs to recognise a
/// condition in result TEXT without every tool having to carry a richer
/// result type.
pub const ABORTED_SENTINEL: &str = "tool execution aborted by cancellation signal";

/// How far a call got, from what the dispatcher can see.
///
/// `CutOff` is deliberately keyed on [`ErrorClass::Transient`] rather than a
/// second list of timeout wordings. Transient is exactly "did not complete for
/// reasons unrelated to its inputs" — a timeout, a reset connection, an
/// `EAGAIN` — which is the same set. A second list would be a copy that
/// drifts.
pub fn completion_of(tool_name: &str, is_error: bool, excerpt: &str) -> Completion {
    if !is_error {
        return Completion::Ok;
    }
    if excerpt.contains(ABORTED_SENTINEL) {
        return Completion::CutOff;
    }
    if classify(tool_name, excerpt) == ErrorClass::Transient {
        return Completion::CutOff;
    }
    Completion::Failed
}

/// Whether an operation can change anything outside the process at all.
///
/// Exhaustive, so a new [`Operation`] must state its answer. The safe default
/// is the one it should have to argue against — claiming `NoEffect` for
/// something that mutates is how the model gets told nothing happened about a
/// file that changed.
const fn can_mutate(op: Operation) -> bool {
    match op {
        // Pure reads: read, grep, glob, list_dir, repo_overview, the LSP
        // family. Cannot change anything, however they end.
        Operation::Read => false,
        Operation::Edit
        | Operation::Execute
        | Operation::Network
        | Operation::Mcp
        | Operation::Plugin
        | Operation::Agent
        | Operation::Memory
        | Operation::Skill
        // Mostly bookkeeping, but `issue` creates rows and `write_todo_list`
        // replaces a list. Both are things a model must not silently redo.
        | Operation::Meta
        // Unknown tool. The one case where the default MUST be "it might".
        | Operation::Other => true,
    }
}

/// Classify one tool result.
pub fn classify_effect(tool_name: &str, completion: Completion) -> SideEffect {
    if !can_mutate(tool_operation(tool_name)) {
        return SideEffect::NoEffect;
    }
    match completion {
        Completion::Ok => SideEffect::Committed,
        // See the module docs: an error means the tool reported failure, not
        // that nothing happened.
        Completion::Failed | Completion::CutOff => SideEffect::Unknown,
    }
}

/// Classify straight from what the dispatcher sees.
pub fn classify_result(tool_name: &str, is_error: bool, excerpt: &str) -> SideEffect {
    classify_effect(tool_name, completion_of(tool_name, is_error, excerpt))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the bead's acceptance criteria, named as it named them ----

    #[test]
    fn write_tool_reports_committed() {
        assert_eq!(
            classify_result("write", false, "wrote 12 lines"),
            SideEffect::Committed
        );
        for tool in ["edit", "apply_patch", "edit_minified"] {
            assert_eq!(classify_result(tool, false, "ok"), SideEffect::Committed);
        }
    }

    #[test]
    fn successful_bash_reports_committed() {
        assert_eq!(
            classify_result("bash", false, "3 files changed"),
            SideEffect::Committed
        );
    }

    #[test]
    fn aborted_bash_reports_unknown() {
        assert_eq!(
            classify_result("bash", true, ABORTED_SENTINEL),
            SideEffect::Unknown
        );
    }

    #[test]
    fn timed_out_bash_reports_unknown() {
        assert_eq!(
            classify_result("bash", true, "Command timed out after 120s"),
            SideEffect::Unknown
        );
    }

    #[test]
    fn read_tool_reports_no_effect() {
        for tool in ["read", "grep", "glob", "list_dir", "find_callers"] {
            for (is_error, text) in [
                (false, "contents"),
                (true, "No such file or directory"),
                (true, "timed out"),
                (true, ABORTED_SENTINEL),
            ] {
                assert_eq!(
                    classify_result(tool, is_error, text),
                    SideEffect::NoEffect,
                    "{tool} / is_error={is_error} must be NoEffect however it ended"
                );
            }
        }
    }

    // ---- the distinctions that make the taxonomy worth carrying ----

    /// All three states must be reachable. A taxonomy that collapses to one
    /// answer carries no information and would pass every test above that
    /// only checks its own case.
    #[test]
    fn all_three_states_are_reachable() {
        let states = [
            classify_result("write", false, "ok"),
            classify_result("bash", true, "Command timed out"),
            classify_result("read", false, "data"),
        ];
        assert_eq!(states[0], SideEffect::Committed);
        assert_eq!(states[1], SideEffect::Unknown);
        assert_eq!(states[2], SideEffect::NoEffect);
        let distinct: std::collections::HashSet<_> = states.iter().collect();
        assert_eq!(distinct.len(), 3, "the three states collapsed: {states:?}");
    }

    /// A failed mutating call is `Unknown`, NOT `NoEffect`. This is the
    /// judgement call the module docs argue for, pinned so it cannot be
    /// "simplified" back into claiming nothing happened.
    #[test]
    fn a_failed_mutating_call_is_unknown_not_no_effect() {
        // apply_patch is the concrete case: a batch can fail having already
        // applied earlier files (dirge-tc9l).
        assert_eq!(
            classify_result("apply_patch", true, "FAILED at hunk 3"),
            SideEffect::Unknown
        );
        // bash exiting non-zero ran the command first.
        assert_eq!(
            classify_result("bash", true, "make: *** [all] Error 1"),
            SideEffect::Unknown
        );
    }

    /// An unknown tool might do anything, so it must not be reported as safe.
    #[test]
    fn an_unknown_tool_is_never_no_effect() {
        assert_eq!(tool_operation("some_mcp_thing"), Operation::Other);
        assert_eq!(
            classify_result("some_mcp_thing", false, "done"),
            SideEffect::Committed
        );
        assert_eq!(
            classify_result("some_mcp_thing", true, "timed out"),
            SideEffect::Unknown
        );
    }

    /// `issue` creates rows — a retried create makes two. It rides
    /// `Operation::Meta` alongside genuinely inert tools, so `Meta` as a whole
    /// has to count as mutating.
    #[test]
    fn meta_tools_that_write_are_not_no_effect() {
        assert_eq!(
            classify_result("issue", false, "created drg-1234"),
            SideEffect::Committed
        );
        assert_eq!(
            classify_result("write_todo_list", false, "ok"),
            SideEffect::Committed
        );
    }

    #[test]
    fn every_operation_states_whether_it_mutates() {
        // Read is the ONLY non-mutating operation; asserted as a set so
        // adding a permissive variant has to change this test deliberately.
        assert!(!can_mutate(Operation::Read));
        for op in [
            Operation::Edit,
            Operation::Execute,
            Operation::Network,
            Operation::Mcp,
            Operation::Plugin,
            Operation::Agent,
            Operation::Memory,
            Operation::Skill,
            Operation::Meta,
            Operation::Other,
        ] {
            assert!(can_mutate(op), "{op:?} claimed it cannot mutate");
        }
    }

    // ---- completion_of ----

    #[test]
    fn completion_distinguishes_ran_from_cut_off() {
        assert_eq!(completion_of("bash", false, "ok"), Completion::Ok);
        assert_eq!(
            completion_of("bash", true, "make: *** Error 1"),
            Completion::Failed
        );
        assert_eq!(
            completion_of("bash", true, "Command timed out after 120s"),
            Completion::CutOff
        );
        assert_eq!(
            completion_of("bash", true, ABORTED_SENTINEL),
            Completion::CutOff
        );
        // A reset connection is a cut-off too: the remote's state is unknown.
        assert_eq!(
            completion_of("mcp_tool", true, "connection reset by peer"),
            Completion::CutOff
        );
    }

    /// The abort sentinel must win even when the surrounding text would
    /// otherwise classify. A cancelled call is cut off regardless.
    #[test]
    fn the_abort_sentinel_outranks_the_error_wording() {
        let mixed = format!("{ABORTED_SENTINEL} (no such file or directory)");
        assert_eq!(completion_of("bash", true, &mixed), Completion::CutOff);
    }
}
