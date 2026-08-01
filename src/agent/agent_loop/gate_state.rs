//! Finalization-gate lifecycle state, in one place [dirge-5mtx.5].
//!
//! Every finalization gate in [`super::run::poll_finalization_follow_up`] needs
//! some per-run state to stop it re-firing. These used to be nine separate
//! `&mut` parameters threaded through a signature that carried
//! `#[allow(clippy::too_many_arguments)]` to stay quiet. Collecting them here
//! is not only tidying: it puts every gate's lifecycle next to every other
//! one, which is what makes the cost-ceiling / re-fire-guard distinction below
//! visible at all.
//!
//! # Why the counters exist
//!
//! Gate predicates evaluate over `new_messages`, which ACCUMULATES across
//! re-entries within a single finalization sequence. A condition that was true
//! once therefore stays true, so a gate with no counter would fire on every
//! pass. The counters are what stop that.
//!
//! # Two kinds of bound — do not confuse them
//!
//! Each field is labelled below. The distinction matters because it decides
//! what is safe to change:
//!
//! - **Cost ceiling** — bounds spend (LLM calls, tokens, wall time). Removing
//!   or raising one costs real money on every run that hits it, and removing
//!   it entirely admits an unbounded loop. These stay regardless of how good
//!   the predicates get.
//! - **Re-fire guard** — exists only because the predicate cannot tell "this
//!   happened" from "this happened and I already reacted". A sufficiently
//!   state-derived predicate would subsume it. Relaxing one wastes a
//!   round-trip at worst.
//!
//! Note that relaxing a re-fire guard was **descoped** from dirge-5mtx.5 on
//! the evidence: run-to-run variance on the A/B scenarios is ~2x, so "this
//! budget can be relaxed without hurting anything" is not distinguishable from
//! noise at any sample size worth paying for. The labels are recorded so the
//! question can be asked properly later, not because the answer is known. See
//! `docs/verification-discipline.md` for the measurement protocol.

use super::types::GateMode;

/// Per-run lifecycle state for the finalization gates.
///
/// Constructed once per `run_loop` and passed by `&mut` to every finalization
/// poll, so the counters persist across re-entries within a run and reset
/// between runs.
#[derive(Debug, Default)]
pub struct GateStates {
    /// **Re-fire guard.** One-shot flag for the `Off`/`Advisory` unified judge
    /// [dirge-8v98]; the persistent `Blocking` path uses
    /// [`Self::code_review_reacts`] instead.
    pub critic_done: bool,

    /// **Cost ceiling.** Bounded by `code_review::MAX_REVIEW_REACT`. Each
    /// reaction is a judge LLM call over the run diff, so this is spend, not
    /// just politeness.
    pub code_review_reacts: u8,

    /// **Memo,** not a bound. Fingerprint of the diff the `Blocking` judge last
    /// reviewed, so an unchanged diff — the model declined or rebutted the
    /// finding and changed nothing — is not re-reviewed [dirge-9b2k].
    pub last_reviewed_fingerprint: Option<u64>,

    /// **Memo,** not a bound. The last reaction's rendered findings, handed to
    /// the next judge prompt so it does not blindly re-raise one the model
    /// already rebutted [dirge-9b2k R2].
    pub last_review_findings: Option<String>,

    /// **Cost ceiling.** Bounded by `goal::MAX_GOAL_REACT`. Each reaction is a
    /// goal-evaluation LLM call.
    pub goal_reacts: u8,

    /// **Re-fire guard.** Bounded by `MAX_TODO_NUDGES`. No LLM call; the cost
    /// of over-firing is a wasted round-trip and a nagged model.
    pub todo_nudges: u8,

    /// **Re-fire guard.** Bounded by `MAX_RESUME_NUDGE`.
    pub resume_nudges: u8,

    /// **Re-fire guard.** Bounded by `MAX_OPEN_ISSUES_NUDGES` [dirge-ksjl].
    /// The gate reads the issue DB, not a provider.
    pub open_issues_nudges: u8,

    /// **Re-fire guard.** Bounded by `MAX_TRACK_NUDGES`; the
    /// file-edits-without-todos advisory is one-shot per run [dirge-track].
    pub track_nudges: u8,
}

/// Read-only inputs the finalization gates consult but do not own.
///
/// Separate from [`GateStates`] because these are borrowed per-poll rather
/// than per-run: the code-review baseline is captured at run start and the
/// open-issues trio is resolved from config and the session.
#[derive(Debug, Clone, Copy)]
pub struct GateInputs<'a> {
    /// Run-start diff baseline for the code-review judge, if one was captured.
    pub code_review_baseline: Option<&'a super::code_review::RunDiff>,
    /// Open-issues gate mode [dirge-ksjl]. `Off` skips the gate entirely.
    pub open_issues_gate_mode: GateMode,
    /// Issue DB to read the session's board from; `None` skips the gate.
    pub issue_db_path: Option<&'a std::path::Path>,
    /// Session whose board to read. The gate is session-scoped, so a `None`
    /// session means the passive backlog is not nagged about.
    pub session_id: Option<&'a str>,
}

impl Default for GateInputs<'_> {
    /// The inert configuration: no baseline, gate off, no DB, no session.
    /// Every gate driven by these inputs is skipped.
    fn default() -> Self {
        Self {
            code_review_baseline: None,
            open_issues_gate_mode: GateMode::Off,
            issue_db_path: None,
            session_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of `Default` here is that a fresh run starts with every
    /// budget unspent and every memo empty — a test that builds one and only
    /// sets the field it cares about must get inert behaviour for the rest.
    #[test]
    fn default_states_are_unspent() {
        let s = GateStates::default();
        assert!(!s.critic_done);
        assert_eq!(s.code_review_reacts, 0);
        assert_eq!(s.goal_reacts, 0);
        assert_eq!(s.todo_nudges, 0);
        assert_eq!(s.resume_nudges, 0);
        assert_eq!(s.open_issues_nudges, 0);
        assert_eq!(s.track_nudges, 0);
        assert!(s.last_reviewed_fingerprint.is_none());
        assert!(s.last_review_findings.is_none());
    }

    #[test]
    fn default_inputs_disable_every_gate_they_drive() {
        let i = GateInputs::default();
        assert!(i.code_review_baseline.is_none());
        assert_eq!(i.open_issues_gate_mode, GateMode::Off);
        assert!(i.issue_db_path.is_none());
        assert!(i.session_id.is_none());
    }
}
