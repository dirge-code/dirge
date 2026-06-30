//! Shared tool-activity classification and the [`LoopGuards`] facade.
//!
//! Two loop-protection engines watch the model's tool calls:
//!
//! - [`super::storm::StormBreaker`] — pre-dispatch circuit breaker that
//!   SUPPRESSES a call repeated with identical args (the model is stuck
//!   re-running the same thing).
//! - [`super::failure_tracker::FailureTracker`] — post-result feedback
//!   that NUDGES (a reflection checkpoint) when errors pile up, even when
//!   each one differs.
//!
//! They genuinely do different jobs at different points and keep their
//! own state (their reset lifecycles differ: storm resets per user turn,
//! the failure tracker resets on any success). What they used to lack was
//! a shared notion of *cost*: a command that ran out its entire time
//! budget (a timeout) counted exactly the same as one that failed in a
//! millisecond. So a model "stuck doing the same thing till it times out"
//! produced the weakest possible signal — one cheap-looking error per
//! 120s — and neither engine escalated.
//!
//! This module fixes that. [`Outcome`] classifies each result once
//! (Ok / Error / Timeout), and [`LoopGuards`] fans that single
//! classification into both engines so a timeout escalates faster in
//! each: the failure tracker counts it double toward its nudge, and the
//! storm breaker drops its threshold for that exact call so a hanging
//! command isn't allowed to burn the budget three times before the loop
//! is broken.

use super::failure_tracker::FailureTracker;
use super::message::LoopMessage;
use super::storm::{StormBreaker, StormReport};
use super::tools::ToolCall;
use std::sync::Arc;

/// Outcome of a dispatched tool call, classified once from its result.
///
/// `Timeout` is called out separately from `Error` because a command
/// that ran to its timeout is a far stronger "stuck" signal than one
/// that failed fast: it consumed the whole budget and retrying it
/// identically will just consume it again. The guards weight it more
/// heavily than an ordinary error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Error,
    Timeout,
    /// A permission/approval refusal (dirge-c7sd). Split out from `Error`
    /// because it is *not* mechanically recoverable: the model cannot fix
    /// it by retrying, rephrasing, or "a different approach" — only the
    /// user can, via `/allow` or a prompt. The failure tracker must not
    /// fold it into the recovery-checkpoint streak (that nudge tells the
    /// model to route around the wall, which is exactly wrong here).
    Denied,
}

impl Outcome {
    /// Classify from a tool result's error flag and text excerpt.
    ///
    /// A timeout surfaces as the [`super::super::tools::bash::exec`]
    /// messages — `run_with_timeout`'s "Command timed out after Ns" and
    /// the background auto-kill's "auto-killed after Ns". Matching the
    /// message keeps us from threading millisecond costs up through every
    /// tool's dispatch path: the only cost that signals a loop is the one
    /// that hit the ceiling, and that one already announces itself.
    pub fn classify(is_error: bool, excerpt: &str) -> Self {
        if !is_error {
            return Outcome::Ok;
        }
        // Policy refusal first: a denied call is not made retryable by a
        // bigger timeout budget, so it outranks the timeout marker even
        // when the reason text happens to mention one.
        if crate::agent::tools::is_permission_denial(excerpt) {
            Outcome::Denied
        } else if excerpt.contains("timed out after") || excerpt.contains("auto-killed after") {
            Outcome::Timeout
        } else {
            Outcome::Error
        }
    }
}

/// Single integration surface over the two loop-protection engines.
///
/// run.rs holds one `LoopGuards` instead of a separate storm breaker and
/// failure tracker. Every result is classified once here and fanned out
/// to both, so the cost (timeout) signal can't reach one engine and miss
/// the other.
pub struct LoopGuards {
    storm: StormBreaker,
    failures: Arc<FailureTracker>,
}

impl LoopGuards {
    pub fn new(storm: StormBreaker, failures: Arc<FailureTracker>) -> Self {
        Self { storm, failures }
    }

    /// Fresh repeat-intent for a new user turn (storm only — the failure
    /// tracker is deliberately cross-turn, reset by success not by turn).
    pub fn reset_turn(&mut self) {
        self.storm.reset();
    }

    /// Pre-dispatch: filter a batch of calls through the storm breaker.
    pub fn inspect_calls(&mut self, calls: &[ToolCall]) -> (Vec<ToolCall>, StormReport) {
        self.storm.filter_calls(calls)
    }

    /// Post-dispatch: classify the result once and feed both engines.
    /// `call` is the originating call so the storm breaker can tie a
    /// timeout to the exact signature it will see again on a retry.
    pub fn record_result(&mut self, call: &ToolCall, is_error: bool, excerpt: &str) {
        let outcome = Outcome::classify(is_error, excerpt);
        self.storm.note_outcome(call, outcome);
        self.failures.record(outcome, &call.name, excerpt);
    }

    /// Turn-boundary poll for the failure tracker's recovery checkpoint.
    pub fn poll_reflection(&self) -> Vec<LoopMessage> {
        self.failures.poll_reflection()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_success_error_and_timeout() {
        assert_eq!(Outcome::classify(false, "all good"), Outcome::Ok);
        assert_eq!(
            Outcome::classify(true, "old_string not found"),
            Outcome::Error
        );
        assert_eq!(
            Outcome::classify(true, "Command timed out after 120s"),
            Outcome::Timeout
        );
        assert_eq!(
            Outcome::classify(true, "background shell auto-killed after 600s timeout"),
            Outcome::Timeout
        );
    }

    #[test]
    fn timeout_text_on_a_success_is_still_ok() {
        // The marker only matters when the call actually errored.
        assert_eq!(
            Outcome::classify(false, "note: a prior run timed out after 5s"),
            Outcome::Ok
        );
    }

    #[test]
    fn classify_maps_permission_denials_to_denied() {
        // Every denial form the enforce layer emits classifies as Denied,
        // distinct from a mechanical Error the model could retry around.
        for text in [
            "Permission denied: writes outside project",
            "Permission denied by user",
            "Permission denied (non-interactive mode)",
            "Auto-approval denied by approval_provider: file is outside the project directory",
        ] {
            assert_eq!(Outcome::classify(true, text), Outcome::Denied, "{text}");
        }
    }

    #[test]
    fn denial_text_on_a_success_is_still_ok() {
        // Same guard as timeouts: the marker only matters on an error.
        assert_eq!(
            Outcome::classify(false, "Permission denied: (quoting a past run)"),
            Outcome::Ok
        );
    }

    #[test]
    fn a_denial_that_also_timed_out_is_classified_denied() {
        // Policy refusal takes precedence: a denied call is not something
        // a longer timeout budget would fix.
        assert_eq!(
            Outcome::classify(true, "Permission denied: command timed out after 1s"),
            Outcome::Denied
        );
    }
}
