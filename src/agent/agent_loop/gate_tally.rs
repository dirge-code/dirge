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
//! The tally now serves TWO consumers: an A/B harness that reads it after
//! a run completes, and a capability estimator that reads it during a run
//! to adapt steering thresholds to how the model is actually performing.
//! Both read the same observation-only counters — the tally still has no
//! control-flow effect.
//!
//! The tally AGGREGATES signals, one source of truth per signal: repair
//! counts come from the existing per-run `RepairStats` on `LoopConfig`
//! (latched at run end), not a second counter. Among those,
//! `repair_invalid` is the strongest capability tell — arguments so
//! malformed the repair pass gave up means the model could not produce a
//! dispatchable call even after repair.
//!
//! It deliberately contains no rig/LLM types, so it stays unit-testable
//! without a model.

use crate::agent::agent_loop::tool_input_repair::RepairStatsSnapshot;

/// Which finalization gate produced a run's follow-up. Mirrors the
/// existing `FollowUpSource` in `run.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateSource {
    AwaitingUser,
    Hook,
    ResumeAfterFailure,
    Verifier,
    /// Deterministic claim/evidence gate (dirge-d0e5.2). No LLM call.
    ClaimGate,
    /// Deterministic artifact-scope sourcing gate (dirge-lavc GAP 1). No
    /// LLM call.
    SourceGate,
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
    /// dirge-t5dh: a run that has produced NOTHING yet crossed the prologue
    /// bound. Distinct from `ProgressStall` (produced, then stopped).
    ProgressPrologue,
    FileTouch,
    ReflectionCheckpoint,
    SafeState,
    None,
}

/// One member of a boundary co-occurrence event, in the order it was
/// recorded. A boundary is one decision point in the loop — the boundary
/// nudge poll at a turn's start, or the finalization gate poll — and every
/// gate and nudge that fires there becomes one event (dirge-1elu.6).
/// Observation only: nothing in the loop reads these back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryMember {
    Gate(GateSource),
    Nudge(BoundaryNudge),
}

impl GateSource {
    fn index(self) -> usize {
        match self {
            GateSource::AwaitingUser => 0,
            GateSource::Hook => 1,
            GateSource::ResumeAfterFailure => 2,
            GateSource::Verifier => 3,
            GateSource::ClaimGate => 4,
            GateSource::SourceGate => 5,
            GateSource::Critic => 6,
            GateSource::Goal => 7,
            GateSource::Todo => 8,
            GateSource::OpenIssues => 9,
            GateSource::None => 10,
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
            BoundaryNudge::ProgressPrologue => 4,
            BoundaryNudge::FileTouch => 5,
            BoundaryNudge::ReflectionCheckpoint => 6,
            BoundaryNudge::SafeState => 7,
            BoundaryNudge::None => 8,
        }
    }
}

/// Aggregated per-run counts of which gates and boundary nudges fired,
/// plus the capability signals the loop computes and would otherwise
/// discard.
#[derive(Clone, Debug, Default)]
pub struct GateTally {
    /// One slot per [`GateSource`] variant, indexed by `GateSource::index`.
    /// Adding a variant means growing this — the index is unchecked, so a
    /// stale length panics at runtime rather than failing to compile.
    gates: [u32; 11],
    /// One slot per [`BoundaryNudge`] variant, same contract as above.
    nudges: [u32; 9],
    turns: u32,
    tool_calls: u32,
    errored_tool_calls: u32,
    final_verification: Option<crate::agent::agent_loop::verifier::VerificationStatus>,
    repairs: Option<RepairStatsSnapshot>,
    /// dirge-5mtx.7: the capability tier the estimator settled on for this
    /// run. OBSERVATION ONLY — nothing reads it back to change behaviour. It
    /// exists so tier distributions across models and scenarios can be
    /// collected before any threshold is derived from them.
    capability_tier: Option<super::capability::CapabilityTier>,
    scavenged_calls: u32,
    hallucinated_tool_names: u32,
    storm_suppressions: u32,
    /// Peak failure streak over the run.
    max_failure_streak: u32,
    /// dirge-1elu.6: completed boundary co-occurrence events, in run order.
    /// Each event lists the gates and nudges that fired at one decision
    /// point. OBSERVATION ONLY — no loop logic reads this back.
    boundaries: Vec<Vec<BoundaryMember>>,
    /// Members recorded since [`begin_boundary`](Self::begin_boundary),
    /// flushed by [`end_boundary`](Self::end_boundary).
    open_boundary: Vec<BoundaryMember>,
    /// True between `begin_boundary` and `end_boundary`.
    in_boundary: bool,
}

impl GateTally {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a boundary event. Subsequent [`record_gate`](Self::record_gate)
    /// / [`record_nudge`](Self::record_nudge) calls that pass a non-`None`
    /// variant are attributed to it, in order, until
    /// [`end_boundary`](Self::end_boundary) flushes it as one co-occurrence
    /// event. A duplicate open while already inside is a no-op. Observation
    /// only — the per-gate/per-nudge totals are untouched.
    pub fn begin_boundary(&mut self) {
        if !self.in_boundary {
            self.in_boundary = true;
            self.open_boundary.clear();
        }
    }

    /// Close the boundary opened by
    /// [`begin_boundary`](Self::begin_boundary). A boundary with no members
    /// is dropped. Totals are untouched.
    pub fn end_boundary(&mut self) {
        if self.in_boundary {
            self.in_boundary = false;
            if !self.open_boundary.is_empty() {
                self.boundaries
                    .push(std::mem::take(&mut self.open_boundary));
            }
        }
    }

    /// The completed boundary events, in run order (observation surface).
    ///
    /// Test-only: production scrapes the same data off the `dirge::gates`
    /// line via [`Self::boundaries_encoding`], so an ungated accessor here
    /// is dead code in a release build.
    #[cfg(test)]
    pub fn boundaries(&self) -> &[Vec<BoundaryMember>] {
        &self.boundaries
    }

    /// The events as a scrapeable string: events joined by `;`, co-firing
    /// members by `+`, each member its `Debug` name — e.g.
    /// `Verifier+Critic;Goal`. `none` when no event fired (a stable
    /// placeholder like `capability_tier`). This is exactly the value of the
    /// `boundaries=` field on the `dirge::gates` line.
    pub fn boundaries_encoding(&self) -> String {
        if self.boundaries.is_empty() {
            return "none".to_string();
        }
        self.boundaries
            .iter()
            .map(|event| {
                event
                    .iter()
                    .map(|m| match m {
                        BoundaryMember::Gate(g) => format!("{g:?}"),
                        BoundaryMember::Nudge(n) => format!("{n:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("+")
            })
            .collect::<Vec<_>>()
            .join(";")
    }

    /// Record a fired gate. The `None` variant ("no gate fired") is a no-op.
    pub fn record_gate(&mut self, gate: GateSource) {
        if gate == GateSource::None {
            return;
        }
        self.gates[gate.index()] += 1;
        // dirge-1elu.6: also attribute the fire to the open boundary, in
        // order. Observation only — the totals above are the authority.
        if self.in_boundary {
            self.open_boundary.push(BoundaryMember::Gate(gate));
        }
    }

    /// Record a fired boundary nudge. The `None` variant is a no-op.
    pub fn record_nudge(&mut self, nudge: BoundaryNudge) {
        if nudge == BoundaryNudge::None {
            return;
        }
        self.nudges[nudge.index()] += 1;
        // dirge-1elu.6: attribute the fire to the open boundary, in order.
        // Observation only — the totals above are the authority.
        if self.in_boundary {
            self.open_boundary.push(BoundaryMember::Nudge(nudge));
        }
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

    /// Latch the per-run repair snapshot. Sourced from the existing
    /// `LoopConfig::repair_stats` — one source of truth per signal, no
    /// second counter.
    /// Latch the capability tier at run end (dirge-5mtx.7). Observation only.
    pub fn set_capability_tier(&mut self, tier: Option<super::capability::CapabilityTier>) {
        self.capability_tier = tier;
    }

    pub fn set_repairs(&mut self, snapshot: Option<RepairStatsSnapshot>) {
        self.repairs = snapshot;
    }

    /// The model emitted tool-call-shaped TEXT instead of a native tool
    /// call, and it was scavenged into a real call.
    pub fn record_scavenged_call(&mut self) {
        self.scavenged_calls += 1;
    }

    /// A tool name had to be resolved by nearest-name match (suggest.rs).
    /// Not wired yet: suggest.rs is called from several sites, so its
    /// recording is scoped separately (dirge-5mtx.7). Remove this allow
    /// when that wiring lands.
    #[allow(dead_code)]
    pub fn record_hallucinated_tool_name(&mut self) {
        self.hallucinated_tool_names += 1;
    }

    /// A call was suppressed by the storm breaker as a repeat.
    pub fn record_storm_suppression(&mut self) {
        self.storm_suppressions += 1;
    }

    /// High-water mark: keeps the PEAK consecutive-errored-tool-result
    /// streak seen this run, so it never decreases.
    pub fn record_failure_streak(&mut self, current: u32) {
        self.max_failure_streak = self.max_failure_streak.max(current);
    }
}

// Read-side accessors. No consumer yet: the A/B harness (bd dirge-5mtx.1)
// and the capability estimator (dirge-5mtx.7) land in later issues.
// Remove this allow when either lands.
#[allow(dead_code)]
impl GateTally {
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

    pub fn scavenged_calls(&self) -> u32 {
        self.scavenged_calls
    }

    pub fn hallucinated_tool_names(&self) -> u32 {
        self.hallucinated_tool_names
    }

    pub fn storm_suppressions(&self) -> u32 {
        self.storm_suppressions
    }

    pub fn max_failure_streak(&self) -> u32 {
        self.max_failure_streak
    }

    /// Emit the tally as one structured event on the `dirge::gates` target,
    /// with each count as its own named field so a script can scrape it.
    pub fn emit(&self) {
        // VerificationStatus is not a tracing primitive, so render it via
        // its Debug form; use a stable placeholder when none was recorded.
        let final_verification = match self.final_verification {
            Some(status) => format!("{status:?}"),
            None => "none".to_string(),
        };
        // Repair counts come from the latched RepairStats snapshot; when it
        // is absent (never set) emit zeros so the log line keeps a stable
        // shape a script can parse.
        let repairs = &self.repairs;
        // Stable placeholder when unset, so the log line keeps one shape and
        // a scraper never has to cope with a missing field.
        let capability_tier = self.capability_tier.map_or("none", |t| t.as_str());
        let boundaries = self.boundaries_encoding();
        tracing::info!(
            target: "dirge::gates",
            capability_tier = %capability_tier,
            boundaries = %boundaries,
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
            nudge_progress_prologue = self.nudges[BoundaryNudge::ProgressPrologue.index()],
            nudge_file_touch = self.nudges[BoundaryNudge::FileTouch.index()],
            nudge_reflection_checkpoint = self.nudges[BoundaryNudge::ReflectionCheckpoint.index()],
            nudge_safe_state = self.nudges[BoundaryNudge::SafeState.index()],
            scavenged_calls = self.scavenged_calls,
            hallucinated_tool_names = self.hallucinated_tool_names,
            storm_suppressions = self.storm_suppressions,
            max_failure_streak = self.max_failure_streak,
            repair_null_stripped = repairs.as_ref().map_or(0, |s| s.null_stripped),
            repair_json_string_to_array = repairs.as_ref().map_or(0, |s| s.json_string_to_array),
            repair_object_to_array = repairs.as_ref().map_or(0, |s| s.object_to_array),
            repair_bare_string_to_array = repairs.as_ref().map_or(0, |s| s.bare_string_to_array),
            repair_md_link_unwrapped = repairs.as_ref().map_or(0, |s| s.md_link_unwrapped),
            repair_truncation_fixed = repairs.as_ref().map_or(0, |s| s.truncation_fixed),
            repair_invalid = repairs.as_ref().map_or(0, |s| s.invalid),
            repair_total_successful = repairs.as_ref().map_or(0, |s| s.total_successful()),
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
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
            BoundaryNudge::ProgressPrologue,
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
        assert_eq!(
            tally.final_verification,
            Some(VerificationStatus::VerifiedGreen)
        );

        tally.set_verification(None);
        assert!(tally.final_verification.is_none());
    }

    #[test]
    fn scavenged_calls_are_counted() {
        let mut tally = GateTally::new();
        tally.record_scavenged_call();
        tally.record_scavenged_call();
        tally.record_scavenged_call();
        assert_eq!(tally.scavenged_calls(), 3);
    }

    #[test]
    fn repair_snapshot_round_trips() {
        let mut tally = GateTally::new();
        assert!(tally.repairs.is_none());

        let snapshot = RepairStatsSnapshot {
            truncation_fixed: 2,
            invalid: 1,
            ..Default::default()
        };
        tally.set_repairs(Some(snapshot.clone()));
        assert_eq!(tally.repairs, Some(snapshot));

        tally.set_repairs(None);
        assert!(tally.repairs.is_none());
    }

    #[test]
    fn hallucinated_tool_names_are_counted() {
        let mut tally = GateTally::new();
        tally.record_hallucinated_tool_name();
        assert_eq!(tally.hallucinated_tool_names(), 1);
    }

    #[test]
    fn storm_suppressions_are_counted() {
        let mut tally = GateTally::new();
        tally.record_storm_suppression();
        tally.record_storm_suppression();
        assert_eq!(tally.storm_suppressions(), 2);
    }

    #[test]
    fn max_failure_streak_keeps_peak() {
        let mut tally = GateTally::new();
        tally.record_failure_streak(3);
        tally.record_failure_streak(5);
        tally.record_failure_streak(2);
        assert_eq!(tally.max_failure_streak(), 5);
    }
    // ---- dirge-1elu.6: boundary co-occurrence — observation only ---------

    /// dirge-1elu.6 test 1: gates that fire at the SAME boundary become ONE
    /// co-occurrence event, and the tally line says so.
    #[test]
    fn co_firing_gates_at_one_boundary_are_one_event() {
        let mut tally = GateTally::new();
        tally.begin_boundary();
        tally.record_gate(GateSource::Verifier);
        tally.record_gate(GateSource::Critic);
        tally.end_boundary();
        assert_eq!(tally.boundaries_encoding(), "Verifier+Critic");
        assert_eq!(
            tally.boundaries(),
            &[vec![
                BoundaryMember::Gate(GateSource::Verifier),
                BoundaryMember::Gate(GateSource::Critic),
            ]]
        );
        let line = capture_emit(&tally);
        assert!(
            line.contains("boundaries=Verifier+Critic"),
            "the tally line must say so: {line}"
        );
    }

    /// dirge-1elu.6 test 2: the same gates at DIFFERENT boundaries are two
    /// events — co-firing is distinguishable from mere co-presence in a run.
    #[test]
    fn same_gates_at_different_boundaries_are_distinct_events() {
        let mut tally = GateTally::new();
        tally.begin_boundary();
        tally.record_gate(GateSource::Verifier);
        tally.end_boundary();
        tally.begin_boundary();
        tally.record_gate(GateSource::Critic);
        tally.end_boundary();
        assert_eq!(tally.boundaries_encoding(), "Verifier;Critic");
        assert_ne!(tally.boundaries_encoding(), "Verifier+Critic");
    }

    /// dirge-1elu.6 test 3: boundary bookkeeping leaves the existing per-run
    /// totals byte-identical — a bracketed and an unbracketed tally emit the
    /// same line except for the new `boundaries=` field.
    #[test]
    fn boundary_bracketing_leaves_per_run_totals_identical() {
        let mut bracketed = GateTally::new();
        let mut plain = GateTally::new();
        for (gate, nudge) in [
            (GateSource::Verifier, BoundaryNudge::TrackWork),
            (GateSource::Critic, BoundaryNudge::SafeState),
            (GateSource::Todo, BoundaryNudge::FastVerify),
        ] {
            bracketed.begin_boundary();
            bracketed.record_gate(gate);
            bracketed.record_nudge(nudge);
            bracketed.end_boundary();
            plain.record_gate(gate);
            plain.record_nudge(nudge);
        }
        bracketed.record_failure_streak(4);
        plain.record_failure_streak(4);

        // Every pre-existing read surface is unchanged by the bracketing.
        assert_eq!(
            bracketed.gate_count(GateSource::Verifier),
            plain.gate_count(GateSource::Verifier)
        );
        assert_eq!(
            bracketed.nudge_count(BoundaryNudge::TrackWork),
            plain.nudge_count(BoundaryNudge::TrackWork)
        );
        assert_eq!(bracketed.max_failure_streak(), plain.max_failure_streak());
        assert_eq!(bracketed.turns(), plain.turns());
        assert_eq!(bracketed.tool_calls(), plain.tool_calls());

        // The emitted lines differ ONLY in the boundaries field.
        let b = strip_field(&capture_emit(&bracketed), "boundaries");
        let p = strip_field(&capture_emit(&plain), "boundaries");
        assert_eq!(b, p, "pre-existing fields must be byte-identical");

        assert_eq!(
            bracketed.boundaries_encoding(),
            "Verifier+TrackWork;Critic+SafeState;Todo+FastVerify"
        );
        assert_eq!(plain.boundaries_encoding(), "none");
    }

    /// dirge-1elu.6: a boundary that closes with nothing recorded drops no
    /// event, and recording outside a boundary never pollutes the events.
    #[test]
    fn empty_and_unbracketed_recording_produce_no_events() {
        let mut tally = GateTally::new();
        tally.begin_boundary();
        tally.end_boundary(); // nothing fired at this boundary
        tally.record_gate(GateSource::Verifier); // not inside a boundary
        assert_eq!(tally.boundaries_encoding(), "none");
        assert!(tally.boundaries().is_empty());
    }

    // ---- tracing capture helpers ----------------------------------------

    /// The `dirge::gates` line for a tally, rendered by a real subscriber
    /// (the fmt layer), so the tests assert on the actual emit output.
    fn capture_emit(tally: &GateTally) -> String {
        let (cap, _guard) = field_capture();
        tally.emit();
        cap.snapshot()
    }

    /// A capture writer + subscriber for the `dirge::gates` line. Shared with
    /// run_tests (dirge-1elu.6 test 4). Renders through the real fmt layer,
    /// so `%field` values appear exactly as they do in production logs.
    #[derive(Clone, Default)]
    pub(crate) struct FieldCapture {
        buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl FieldCapture {
        pub(crate) fn snapshot(&self) -> String {
            String::from_utf8_lossy(&self.buf.lock().unwrap()).to_string()
        }
    }

    impl std::io::Write for FieldCapture {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.buf.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for FieldCapture {
        type Writer = Self;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    /// A fresh capture subscriber; the guard keeps it installed for the
    /// caller's scope.
    pub(crate) fn field_capture() -> (FieldCapture, tracing::subscriber::DefaultGuard) {
        let cap = FieldCapture::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .without_time()
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(sub);
        (cap, guard)
    }

    /// Drop one key=value pair from a rendered line, for byte-comparison.
    fn strip_field(line: &str, key: &str) -> String {
        let prefix = format!("{key}=");
        line.split_whitespace()
            .filter(|tok| !tok.starts_with(&prefix))
            .collect::<Vec<_>>()
            .join(" ")
    }
}
