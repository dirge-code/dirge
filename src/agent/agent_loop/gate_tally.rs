//! Per-run gate tally.
//!
//! Pure instrumentation: `GateTally` records which finalization gate fired
//! ([`GateSource`]) and which mid-turn boundary nudges fired
//! ([`BoundaryNudge`]) during one run, then `emit`s the counts as a single
//! structured log event. It has no control-flow effect on the loop — it
//! only observes.
//!
//! It exists because `FollowUpSource` in `run.rs` is computed and then
//! dropped with nothing aggregating it, so there is no signal on which
//! gate actually drives follow-ups; and the boundary nudges are bare
//! pushes with no enum at all. This gives both a home, and makes the
//! per-variant counts scrapeable from the `dirge::gates` log target.
//!
//! It deliberately contains no rig/LLM types, so it stays unit-testable
//! without a model.

// STOPGAP: nothing in this module is reachable from production code yet —
// the run.rs wiring lands as part of bd dirge-5mtx.1. Remove this allow
// the moment that wiring ships; it must not silence anything else.
#![allow(dead_code)]

/// Which finalization gate produced a run's follow-up. Mirrors the
/// existing `FollowUpSource` in `run.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateSource {
    AwaitingUser,
    Hook,
    ResumeAfterFailure,
    Verifier,
    Critic,
    Goal,
    Todo,
    OpenIssues,
    None,
}

/// Which mid-turn boundary nudge fired. The boundary twin of `GateSource` —
/// today these are bare pushes in `run.rs` with no enum at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryNudge {
    TrackWork,
    FastVerify,
    ProgressStall,
    ProgressBudget,
    FileTouch,
    ReflectionCheckpoint,
    SafeState,
    None,
}

impl GateSource {
    fn index(self) -> usize {
        match self {
            GateSource::AwaitingUser => 0,
            GateSource::Hook => 1,
            GateSource::ResumeAfterFailure => 2,
            GateSource::Verifier => 3,
            GateSource::Critic => 4,
            GateSource::Goal => 5,
            GateSource::Todo => 6,
            GateSource::OpenIssues => 7,
            GateSource::None => 8,
        }
    }
}

impl BoundaryNudge {
    fn index(self) -> usize {
        match self {
            BoundaryNudge::TrackWork => 0,
            BoundaryNudge::FastVerify => 1,
            BoundaryNudge::ProgressStall => 2,
            BoundaryNudge::ProgressBudget => 3,
            BoundaryNudge::FileTouch => 4,
            BoundaryNudge::ReflectionCheckpoint => 5,
            BoundaryNudge::SafeState => 6,
            BoundaryNudge::None => 7,
        }
    }
}

/// Aggregated per-run counts of which gates and boundary nudges fired.
#[derive(Clone, Debug, Default)]
pub struct GateTally {
    gates: [u32; 9],
    nudges: [u32; 8],
    turns: u32,
    tool_calls: u32,
    errored_tool_calls: u32,
    final_verification: Option<crate::agent::agent_loop::verifier::VerificationStatus>,
}

impl GateTally {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a fired gate. The `None` variant ("no gate fired") is a no-op.
    pub fn record_gate(&mut self, gate: GateSource) {
        if gate == GateSource::None {
            return;
        }
        self.gates[gate.index()] += 1;
    }

    /// Record a fired boundary nudge. The `None` variant is a no-op.
    pub fn record_nudge(&mut self, nudge: BoundaryNudge) {
        if nudge == BoundaryNudge::None {
            return;
        }
        self.nudges[nudge.index()] += 1;
    }

    pub fn record_turn(&mut self) {
        self.turns += 1;
    }

    pub fn record_tool_call(&mut self, is_error: bool) {
        self.tool_calls += 1;
        if is_error {
            self.errored_tool_calls += 1;
        }
    }

    pub fn set_verification(
        &mut self,
        status: Option<crate::agent::agent_loop::verifier::VerificationStatus>,
    ) {
        self.final_verification = status;
    }

    pub fn gate_count(&self, gate: GateSource) -> u32 {
        self.gates[gate.index()]
    }

    pub fn nudge_count(&self, nudge: BoundaryNudge) -> u32 {
        self.nudges[nudge.index()]
    }

    pub fn turns(&self) -> u32 {
        self.turns
    }

    pub fn tool_calls(&self) -> u32 {
        self.tool_calls
    }

    pub fn errored_tool_calls(&self) -> u32 {
        self.errored_tool_calls
    }

    /// Emit the tally as one structured event on the `dirge::gates` target,
    /// with each count as its own named field so a script can scrape it.
    pub fn emit(&self) {
        // VerificationStatus is not a tracing primitive, so render it via
        // its Debug form; use a stable placeholder when none was recorded.
        let final_verification = self
            .final_verification
            .map_or_else(|| "none".to_string(), |status| format!("{status:?}"));
        tracing::info!(
            target: "dirge::gates",
            turns = self.turns,
            tool_calls = self.tool_calls,
            errored_tool_calls = self.errored_tool_calls,
            final_verification = %final_verification,
            gate_awaiting_user = self.gates[GateSource::AwaitingUser.index()],
            gate_hook = self.gates[GateSource::Hook.index()],
            gate_resume_after_failure = self.gates[GateSource::ResumeAfterFailure.index()],
            gate_verifier = self.gates[GateSource::Verifier.index()],
            gate_critic = self.gates[GateSource::Critic.index()],
            gate_goal = self.gates[GateSource::Goal.index()],
            gate_todo = self.gates[GateSource::Todo.index()],
            gate_open_issues = self.gates[GateSource::OpenIssues.index()],
            nudge_track_work = self.nudges[BoundaryNudge::TrackWork.index()],
            nudge_fast_verify = self.nudges[BoundaryNudge::FastVerify.index()],
            nudge_progress_stall = self.nudges[BoundaryNudge::ProgressStall.index()],
            nudge_progress_budget = self.nudges[BoundaryNudge::ProgressBudget.index()],
            nudge_file_touch = self.nudges[BoundaryNudge::FileTouch.index()],
            nudge_reflection_checkpoint = self.nudges[BoundaryNudge::ReflectionCheckpoint.index()],
            nudge_safe_state = self.nudges[BoundaryNudge::SafeState.index()],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::verifier::VerificationStatus;

    #[test]
    fn counts_each_gate_variant_separately() {
        let mut tally = GateTally::new();
        for gate in [
            GateSource::AwaitingUser,
            GateSource::Hook,
            GateSource::ResumeAfterFailure,
            GateSource::Verifier,
            GateSource::Critic,
            GateSource::Goal,
            GateSource::Todo,
            GateSource::OpenIssues,
        ] {
            tally.record_gate(gate);
        }
        tally.record_gate(GateSource::Verifier);

        assert_eq!(tally.gate_count(GateSource::AwaitingUser), 1);
        assert_eq!(tally.gate_count(GateSource::Hook), 1);
        assert_eq!(tally.gate_count(GateSource::ResumeAfterFailure), 1);
        assert_eq!(tally.gate_count(GateSource::Verifier), 2);
        assert_eq!(tally.gate_count(GateSource::Critic), 1);
        assert_eq!(tally.gate_count(GateSource::Goal), 1);
        assert_eq!(tally.gate_count(GateSource::Todo), 1);
        assert_eq!(tally.gate_count(GateSource::OpenIssues), 1);
        assert_eq!(tally.gate_count(GateSource::None), 0);
    }

    #[test]
    fn counts_each_nudge_variant_separately() {
        let mut tally = GateTally::new();
        for nudge in [
            BoundaryNudge::TrackWork,
            BoundaryNudge::FastVerify,
            BoundaryNudge::ProgressStall,
            BoundaryNudge::ProgressBudget,
            BoundaryNudge::FileTouch,
            BoundaryNudge::ReflectionCheckpoint,
            BoundaryNudge::SafeState,
        ] {
            tally.record_nudge(nudge);
        }
        tally.record_nudge(BoundaryNudge::ProgressStall);

        assert_eq!(tally.nudge_count(BoundaryNudge::TrackWork), 1);
        assert_eq!(tally.nudge_count(BoundaryNudge::FastVerify), 1);
        assert_eq!(tally.nudge_count(BoundaryNudge::ProgressStall), 2);
        assert_eq!(tally.nudge_count(BoundaryNudge::ProgressBudget), 1);
        assert_eq!(tally.nudge_count(BoundaryNudge::FileTouch), 1);
        assert_eq!(tally.nudge_count(BoundaryNudge::ReflectionCheckpoint), 1);
        assert_eq!(tally.nudge_count(BoundaryNudge::SafeState), 1);
        assert_eq!(tally.nudge_count(BoundaryNudge::None), 0);
    }

    #[test]
    fn none_gate_is_a_noop() {
        let mut tally = GateTally::new();
        tally.record_gate(GateSource::None);
        tally.record_gate(GateSource::None);
        assert_eq!(tally.gate_count(GateSource::None), 0);
        assert_eq!(tally.gate_count(GateSource::Goal), 0);
    }

    #[test]
    fn none_nudge_is_a_noop() {
        let mut tally = GateTally::new();
        tally.record_nudge(BoundaryNudge::None);
        tally.record_nudge(BoundaryNudge::None);
        assert_eq!(tally.nudge_count(BoundaryNudge::None), 0);
        assert_eq!(tally.nudge_count(BoundaryNudge::FileTouch), 0);
    }

    #[test]
    fn turns_are_counted() {
        let mut tally = GateTally::new();
        tally.record_turn();
        tally.record_turn();
        tally.record_turn();
        assert_eq!(tally.turns(), 3);
    }

    #[test]
    fn tool_call_errors_are_counted_separately() {
        let mut tally = GateTally::new();
        tally.record_tool_call(false);
        tally.record_tool_call(false);
        tally.record_tool_call(true);
        assert_eq!(tally.tool_calls(), 3);
        assert_eq!(tally.errored_tool_calls(), 1);
    }

    #[test]
    fn verification_status_round_trips() {
        let mut tally = GateTally::new();
        assert!(tally.final_verification.is_none());

        tally.set_verification(Some(VerificationStatus::VerifiedGreen));
        assert_eq!(tally.final_verification, Some(VerificationStatus::VerifiedGreen));

        tally.set_verification(None);
        assert!(tally.final_verification.is_none());
    }
}
