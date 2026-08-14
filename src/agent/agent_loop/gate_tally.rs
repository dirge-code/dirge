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

use crate::agent::agent_loop::tool_error_class::ErrorClass;
use crate::agent::agent_loop::tool_input_repair::RepairStatsSnapshot;
use crate::agent::agent_loop::tool_retry::RetryStatsSnapshot;

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
    /// Deterministic completeness gate (dirge-2m68): the final answer stated
    /// work the model still intended to do. No LLM call.
    CompletenessGate,
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
    /// dirge-e31n.6: a run of PERMISSION denials, which is a policy wall
    /// rather than a mechanical failure. Split from `ReflectionCheckpoint`
    /// because the two want opposite advice and, until now, were
    /// indistinguishable on the emitted line — so nothing could tell a run
    /// blocked by the user's rules from one fumbling its tool calls.
    PermissionCheckpoint,
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
    /// Every variant, in [`index`](Self::index) order.
    ///
    /// This exists because the emitter used to carry a hand-written list of
    /// `gate_*` fields, and `ClaimGate` / `SourceGate` were added to the
    /// enum without being added to it — so the two gates that exist to catch
    /// fabricated verification were themselves absent from the only surface
    /// that reports them (dirge-l8l7.1). The unit test that should have
    /// caught it iterated its own hand-written list with the same two
    /// missing, so it asserted exactly what the emitter did.
    ///
    /// Adding a variant now fails to compile in [`index`](Self::index) and
    /// [`field_name`](Self::field_name), and fails
    /// `all_gate_variants_are_indexed_contiguously` and
    /// `every_gate_variant_has_a_field_on_the_emitted_line` until it is
    /// added here and to `emit`.
    pub const ALL: [GateSource; 12] = [
        GateSource::AwaitingUser,
        GateSource::Hook,
        GateSource::ResumeAfterFailure,
        GateSource::Verifier,
        GateSource::ClaimGate,
        GateSource::SourceGate,
        GateSource::CompletenessGate,
        GateSource::Critic,
        GateSource::Goal,
        GateSource::Todo,
        GateSource::OpenIssues,
        GateSource::None,
    ];

    /// Slot in [`GateTally::gates`] — the variant's own discriminant.
    ///
    /// This was a second hand-written mapping, which left one way to break
    /// the tally that neither new test could see: add a variant, add it to
    /// this match with the next index, and forget [`ALL`](Self::ALL). Both
    /// tests iterate `ALL`, so they would have stayed green while
    /// `record_gate` wrote past the end of a `[u32; ALL.len()]` and panicked
    /// at runtime. Deleting the copy is the fix — the discriminant cannot
    /// disagree with the declaration order it comes from.
    fn index(self) -> usize {
        self as usize
    }

    /// The field name this variant carries on the `dirge::gates` line, or
    /// `None` for a variant that is deliberately not emitted. `None` is the
    /// "no gate fired" sentinel — `record_gate` treats it as a no-op, so it
    /// has no count to report.
    ///
    /// Exhaustive by design: a new variant must state which it is, and
    /// `every_gate_variant_has_a_field_on_the_emitted_line` then fails until
    /// `emit` actually carries it.
    ///
    /// Test-only, like [`Self::boundaries`]: `tracing`'s field names must be
    /// literals at the macro call, so production cannot read them from here.
    /// That is exactly why the correspondence needs asserting rather than
    /// assuming — it is the drift that lost `ClaimGate` and `SourceGate`.
    #[cfg(test)]
    pub fn field_name(self) -> Option<&'static str> {
        Some(match self {
            GateSource::AwaitingUser => "gate_awaiting_user",
            GateSource::Hook => "gate_hook",
            GateSource::ResumeAfterFailure => "gate_resume_after_failure",
            GateSource::Verifier => "gate_verifier",
            GateSource::ClaimGate => "gate_claim_gate",
            GateSource::SourceGate => "gate_source_gate",
            GateSource::CompletenessGate => "gate_completeness_gate",
            GateSource::Critic => "gate_critic",
            GateSource::Goal => "gate_goal",
            GateSource::Todo => "gate_todo",
            GateSource::OpenIssues => "gate_open_issues",
            GateSource::None => return Option::None,
        })
    }
}

impl BoundaryNudge {
    /// Every variant, in [`index`](Self::index) order. Same contract as
    /// [`GateSource::ALL`] — see its doc for why this exists.
    pub const ALL: [BoundaryNudge; 10] = [
        BoundaryNudge::TrackWork,
        BoundaryNudge::FastVerify,
        BoundaryNudge::ProgressStall,
        BoundaryNudge::ProgressBudget,
        BoundaryNudge::ProgressPrologue,
        BoundaryNudge::FileTouch,
        BoundaryNudge::ReflectionCheckpoint,
        BoundaryNudge::PermissionCheckpoint,
        BoundaryNudge::SafeState,
        BoundaryNudge::None,
    ];

    /// Slot in [`GateTally::nudges`] — the discriminant. See
    /// [`GateSource::index`] for why this is not a second mapping.
    fn index(self) -> usize {
        self as usize
    }

    /// The field name this variant carries on the `dirge::gates` line, or
    /// `None` for the "no nudge fired" sentinel. Test-only for the same
    /// reason — see [`GateSource::field_name`].
    #[cfg(test)]
    pub fn field_name(self) -> Option<&'static str> {
        Some(match self {
            BoundaryNudge::TrackWork => "nudge_track_work",
            BoundaryNudge::FastVerify => "nudge_fast_verify",
            BoundaryNudge::ProgressStall => "nudge_progress_stall",
            BoundaryNudge::ProgressBudget => "nudge_progress_budget",
            BoundaryNudge::ProgressPrologue => "nudge_progress_prologue",
            BoundaryNudge::FileTouch => "nudge_file_touch",
            BoundaryNudge::ReflectionCheckpoint => "nudge_reflection_checkpoint",
            BoundaryNudge::PermissionCheckpoint => "nudge_permission_checkpoint",
            BoundaryNudge::SafeState => "nudge_safe_state",
            BoundaryNudge::None => return Option::None,
        })
    }
}

/// Aggregated per-run counts of which gates and boundary nudges fired,
/// plus the capability signals the loop computes and would otherwise
/// discard.
#[derive(Clone, Debug, Default)]
pub struct GateTally {
    /// One slot per [`GateSource`] variant, indexed by `GateSource::index`.
    /// Sized off [`GateSource::ALL`] so the two cannot disagree.
    gates: [u32; GateSource::ALL.len()],
    /// One slot per [`BoundaryNudge`] variant, same contract as above.
    nudges: [u32; BoundaryNudge::ALL.len()],
    turns: u32,
    tool_calls: u32,
    /// Errored calls split by recovery class, indexed by
    /// [`ErrorClass::index`]. Sized off [`ErrorClass::ALL`] so the two cannot
    /// disagree.
    ///
    /// There is deliberately NO separate `errored_tool_calls` field:
    /// [`errored_tool_calls`](Self::errored_tool_calls) sums this instead. A
    /// total kept alongside its own parts is a duplicate that drifts the
    /// moment one call site increments one and not the other, which is the
    /// failure this module's own history is made of (dirge-l8l7.1).
    errored_by_class: [u32; ErrorClass::ALL.len()],
    final_verification: Option<crate::agent::agent_loop::verifier::VerificationStatus>,
    repairs: Option<RepairStatsSnapshot>,
    /// dirge-61sv: per-run transient-tool-retry counts, latched at run end
    /// from `LoopConfig::retry_stats` — the dispatch cannot reach the tally,
    /// exactly as with `repairs`.
    retries: Option<RetryStatsSnapshot>,
    /// dirge-5mtx.7: the capability tier the estimator settled on for this
    /// run. OBSERVATION ONLY — nothing reads it back to change behaviour. It
    /// exists so tier distributions across models and scenarios can be
    /// collected before any threshold is derived from them.
    capability_tier: Option<super::capability::CapabilityTier>,
    scavenged_calls: u32,
    hallucinated_tool_names: u32,
    /// dirge-e31n.8: calls written in EXPLICIT call syntax inside the model's
    /// text whose tool name matched nothing, so the scavenger dropped them.
    ///
    /// Sibling of `hallucinated_tool_names`, and the reason both are needed:
    /// that one counts a miss the model was TOLD about ("Tool X not found",
    /// with a nearest-name hint), this one counts a miss nothing reported to
    /// anyone. Dropping silently is deliberate — dirge-knt8 established that
    /// erroring on scavenged text re-forces a continuation turn — so the
    /// counter is how the cost of that choice becomes visible at all.
    ///
    /// OBSERVATION ONLY: not fed to the capability estimator. What one of
    /// these is worth against an errored call is exactly what there is no
    /// data on yet, and guessing a weight would bake the guess into the
    /// tier before the first measurement.
    dropped_unknown_names: u32,
    storm_suppressions: u32,
    /// Peak failure streak over the run.
    max_failure_streak: u32,
    /// dirge-e31n.5: tool calls whose effect could not be confirmed. The
    /// MECHANISM GATE for the unresolved-effect handoff: the handoff renders
    /// only when this is non-zero, so an A/B reading zero in both arms
    /// measured nothing however healthy the rest of the report looks.
    unresolved_effects: u32,
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

    /// Record one dispatched tool call. `error` is `None` for a success and
    /// `Some(class)` for a failure, so the total and the per-class split are
    /// written from ONE input at ONE site and cannot disagree — an errored
    /// call with no class is unrepresentable rather than merely discouraged.
    /// A failure the classifier declined to name is
    /// [`ErrorClass::Unclassified`], which is a class like any other.
    pub fn record_tool_call(&mut self, error: Option<ErrorClass>) {
        self.tool_calls += 1;
        if let Some(class) = error {
            self.errored_by_class[class.index()] += 1;
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

    /// Latch the per-run transient-retry snapshot (dirge-61sv). Observation
    /// only, like every other counter here.
    pub fn set_retries(&mut self, snapshot: Option<RetryStatsSnapshot>) {
        self.retries = snapshot;
    }

    /// The model emitted tool-call-shaped TEXT instead of a native tool
    /// call, and it was scavenged into a real call.
    pub fn record_scavenged_call(&mut self) {
        self.scavenged_calls += 1;
    }

    /// A dispatched call named a tool the run does not have. Recorded by
    /// `run::record_tool_result_signals`, which re-derives the miss from the
    /// batch's tool set — see its docs for why the classification lives
    /// there rather than at the rejection site.
    pub fn record_hallucinated_tool_name(&mut self) {
        self.hallucinated_tool_names += 1;
    }

    /// A call written in explicit call syntax inside the model's text named a
    /// tool the run does not have, so it was dropped without dispatch.
    pub fn record_dropped_unknown_name(&mut self) {
        self.dropped_unknown_names += 1;
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

    /// A tool call's effect could not be confirmed (dirge-e31n.5).
    pub fn record_unresolved_effect(&mut self) {
        self.unresolved_effects += 1;
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

    /// Total errored calls — DERIVED from the per-class split, never stored
    /// beside it. See [`GateTally::errored_by_class`].
    pub fn errored_tool_calls(&self) -> u32 {
        self.errored_by_class.iter().sum()
    }

    /// Errored calls split by recovery class, indexed by
    /// [`ErrorClass::index`]. Feeds [`super::capability::CapabilityCounters`],
    /// which weights `MissingInfo` above the rest.
    pub fn errored_by_class(&self) -> [u32; ErrorClass::ALL.len()] {
        self.errored_by_class
    }

    pub fn scavenged_calls(&self) -> u32 {
        self.scavenged_calls
    }

    pub fn hallucinated_tool_names(&self) -> u32 {
        self.hallucinated_tool_names
    }

    pub fn dropped_unknown_names(&self) -> u32 {
        self.dropped_unknown_names
    }

    pub fn storm_suppressions(&self) -> u32 {
        self.storm_suppressions
    }

    pub fn unresolved_effects(&self) -> u32 {
        self.unresolved_effects
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
        let retries = &self.retries;
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
            errored_tool_calls = self.errored_tool_calls(),
            // dirge-s9ry: the class split, not just the total. The total alone
            // is what let two runs at 24% and 27% error rates read as ordinary
            // friction — the harness could see THAT calls failed but not that
            // every one of them named something that wasn't there. Field names
            // come from `ErrorClass::field_name` and are asserted against
            // `ErrorClass::ALL` by
            // `every_error_class_has_a_field_on_the_emitted_line`.
            errored_misuse = self.errored_by_class[ErrorClass::Misuse.index()],
            errored_missing_info = self.errored_by_class[ErrorClass::MissingInfo.index()],
            errored_transient = self.errored_by_class[ErrorClass::Transient.index()],
            errored_fatal = self.errored_by_class[ErrorClass::Fatal.index()],
            errored_unclassified = self.errored_by_class[ErrorClass::Unclassified.index()],
            final_verification = %final_verification,
            gate_awaiting_user = self.gates[GateSource::AwaitingUser.index()],
            gate_hook = self.gates[GateSource::Hook.index()],
            gate_resume_after_failure = self.gates[GateSource::ResumeAfterFailure.index()],
            gate_verifier = self.gates[GateSource::Verifier.index()],
            // dirge-l8l7.1: these two were recorded by `record_gate` (run.rs
            // maps them through an exhaustive `From<FollowUpSource>`) and
            // then dropped here, so the two gates that exist to catch
            // fabricated verification were the two whose own firing could
            // not be observed. Field names come from `GateSource::field_name`
            // and are asserted against `GateSource::ALL` by
            // `every_gate_variant_has_a_field_on_the_emitted_line`.
            gate_claim_gate = self.gates[GateSource::ClaimGate.index()],
            gate_source_gate = self.gates[GateSource::SourceGate.index()],
            gate_completeness_gate = self.gates[GateSource::CompletenessGate.index()],
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
            nudge_permission_checkpoint = self.nudges[BoundaryNudge::PermissionCheckpoint.index()],
            nudge_safe_state = self.nudges[BoundaryNudge::SafeState.index()],
            scavenged_calls = self.scavenged_calls,
            hallucinated_tool_names = self.hallucinated_tool_names,
            dropped_unknown_names = self.dropped_unknown_names,
            storm_suppressions = self.storm_suppressions,
            max_failure_streak = self.max_failure_streak,
            unresolved_effects = self.unresolved_effects,
            repair_null_stripped = repairs.as_ref().map_or(0, |s| s.null_stripped),
            repair_json_string_to_array = repairs.as_ref().map_or(0, |s| s.json_string_to_array),
            repair_object_to_array = repairs.as_ref().map_or(0, |s| s.object_to_array),
            repair_bare_string_to_array = repairs.as_ref().map_or(0, |s| s.bare_string_to_array),
            repair_md_link_unwrapped = repairs.as_ref().map_or(0, |s| s.md_link_unwrapped),
            repair_truncation_fixed = repairs.as_ref().map_or(0, |s| s.truncation_fixed),
            repair_invalid = repairs.as_ref().map_or(0, |s| s.invalid),
            repair_total_successful = repairs.as_ref().map_or(0, |s| s.total_successful()),
            // dirge-61sv. `attempted` is the mechanism gate: zero means no
            // transient read failure ever occurred, so any comparison of runs
            // with and without the retry measured nothing. `recovered` is
            // whether it earned the latency it spent.
            tool_retries_attempted = retries.as_ref().map_or(0, |s| s.attempted),
            tool_retries_recovered = retries.as_ref().map_or(0, |s| s.recovered),
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

    // ---- dirge-l8l7.1: the emitted line must cover every variant --------
    //
    // These four replace the hand-written variant lists that let `ClaimGate`
    // and `SourceGate` be added to the enum, recorded by `record_gate`, and
    // then dropped by `emit` — with `counts_each_gate_variant_separately`
    // green throughout, because it iterated a list with the same two
    // missing. A test that enumerates the thing it is checking cannot fail.

    /// Field names on a rendered `dirge::gates` line, parsed exactly. The
    /// line is space-separated `key=value`, so this splits rather than
    /// substring-matching: `contains("gate_todo=")` would also be satisfied
    /// by a longer field ending in that name, which is precisely the class
    /// of bug `loop-ab.sh`'s `get_field` hit when `tool_calls` silently read
    /// `errored_tool_calls`.
    fn emitted_field_names(line: &str) -> std::collections::HashSet<&str> {
        line.split_whitespace()
            .filter_map(|tok| tok.split_once('='))
            .map(|(k, _)| k)
            .collect()
    }

    /// The one way left to break the tally: add a variant ahead of the `None`
    /// sentinel and not extend `ALL`. `index` is the discriminant now, so the
    /// new variant would write past the end of a `[u32; ALL.len()]` and panic
    /// inside `record_gate` — and every test that iterates `ALL` would stay
    /// green, because `ALL` is what is missing it. Anchoring on the sentinel's
    /// discriminant is what makes that visible.
    ///
    /// `None` must stay declared last in both enums for this to hold.
    #[test]
    fn all_ends_at_the_sentinel_so_a_new_variant_cannot_hide() {
        assert_eq!(
            GateSource::ALL.len(),
            GateSource::None as usize + 1,
            "a GateSource variant was added without extending ALL"
        );
        assert_eq!(
            BoundaryNudge::ALL.len(),
            BoundaryNudge::None as usize + 1,
            "a BoundaryNudge variant was added without extending ALL"
        );
    }

    #[test]
    fn all_gate_variants_are_indexed_contiguously() {
        let mut seen = vec![false; GateSource::ALL.len()];
        for gate in GateSource::ALL {
            let i = gate.index();
            assert!(
                i < seen.len(),
                "{gate:?} indexes {i}, past the end of ALL ({})",
                seen.len()
            );
            assert!(!seen[i], "index {i} is claimed twice; {gate:?} collides");
            seen[i] = true;
        }
        assert!(
            seen.iter().all(|s| *s),
            "GateSource::ALL is missing a variant: index(es) {:?} unclaimed",
            seen.iter()
                .enumerate()
                .filter(|(_, s)| !**s)
                .map(|(i, _)| i)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn all_nudge_variants_are_indexed_contiguously() {
        let mut seen = vec![false; BoundaryNudge::ALL.len()];
        for nudge in BoundaryNudge::ALL {
            let i = nudge.index();
            assert!(
                i < seen.len(),
                "{nudge:?} indexes {i}, past the end of ALL ({})",
                seen.len()
            );
            assert!(!seen[i], "index {i} is claimed twice; {nudge:?} collides");
            seen[i] = true;
        }
        assert!(
            seen.iter().all(|s| *s),
            "BoundaryNudge::ALL is missing a variant: index(es) {:?} unclaimed",
            seen.iter()
                .enumerate()
                .filter(|(_, s)| !**s)
                .map(|(i, _)| i)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_gate_variant_has_a_field_on_the_emitted_line() {
        let line = capture_emit(&GateTally::new());
        let present = emitted_field_names(&line);
        let missing: Vec<&str> = GateSource::ALL
            .into_iter()
            .filter_map(GateSource::field_name)
            .filter(|name| !present.contains(name))
            .collect();
        assert!(
            missing.is_empty(),
            "GateSource variants recorded but never emitted: {missing:?}\nline: {line}"
        );
    }

    #[test]
    fn every_nudge_variant_has_a_field_on_the_emitted_line() {
        let line = capture_emit(&GateTally::new());
        let present = emitted_field_names(&line);
        let missing: Vec<&str> = BoundaryNudge::ALL
            .into_iter()
            .filter_map(BoundaryNudge::field_name)
            .filter(|name| !present.contains(name))
            .collect();
        assert!(
            missing.is_empty(),
            "BoundaryNudge variants recorded but never emitted: {missing:?}\nline: {line}"
        );
    }

    /// The other side of the two tests above: a name they check for must be
    /// one the emitter could actually have got wrong. If `field_name` ever
    /// returned something no `emit` field could match, both would be
    /// vacuous — so pin the count and the sentinel exclusion.
    #[test]
    fn only_the_none_sentinels_are_exempt_from_emission() {
        assert_eq!(GateSource::None.field_name(), Option::None);
        assert_eq!(BoundaryNudge::None.field_name(), Option::None);
        assert_eq!(
            GateSource::ALL
                .into_iter()
                .filter_map(GateSource::field_name)
                .count(),
            GateSource::ALL.len() - 1
        );
        assert_eq!(
            BoundaryNudge::ALL
                .into_iter()
                .filter_map(BoundaryNudge::field_name)
                .count(),
            BoundaryNudge::ALL.len() - 1
        );
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
        tally.record_tool_call(None);
        tally.record_tool_call(None);
        tally.record_tool_call(Some(ErrorClass::Unclassified));
        assert_eq!(tally.tool_calls(), 3);
        assert_eq!(tally.errored_tool_calls(), 1);
    }

    /// dirge-s9ry: the total is the SUM of the split, so no sequence of calls
    /// can make the two disagree. `errored_tool_calls` used to be its own
    /// field, which is the shape that drifts.
    #[test]
    fn the_errored_total_is_exactly_the_class_split() {
        let mut tally = GateTally::new();
        for class in ErrorClass::ALL {
            tally.record_tool_call(Some(class));
        }
        tally.record_tool_call(Some(ErrorClass::MissingInfo));
        tally.record_tool_call(None);

        let split = tally.errored_by_class();
        assert_eq!(
            tally.errored_tool_calls(),
            split.iter().sum::<u32>(),
            "total and split disagree"
        );
        assert_eq!(tally.errored_tool_calls(), ErrorClass::ALL.len() as u32 + 1);
        assert_eq!(split[ErrorClass::MissingInfo.index()], 2);
        assert_eq!(split[ErrorClass::Fatal.index()], 1);
        assert_eq!(
            tally.tool_calls(),
            ErrorClass::ALL.len() as u32 + 2,
            "the success counts toward the denominator and nothing else"
        );
    }

    /// The emitted line must carry every class, for the same reason the gate
    /// and nudge lines must: a counter that is recorded and never reported is
    /// a signal nobody can act on (dirge-l8l7.1).
    #[test]
    fn every_error_class_has_a_field_on_the_emitted_line() {
        let line = capture_emit(&GateTally::new());
        let present = emitted_field_names(&line);
        let missing: Vec<&str> = ErrorClass::ALL
            .into_iter()
            .map(ErrorClass::field_name)
            .filter(|name| !present.contains(name))
            .collect();
        assert!(
            missing.is_empty(),
            "ErrorClass variants recorded but never emitted: {missing:?}\nline: {line}"
        );
    }

    /// ...and each field carries ITS OWN class's count. All five reading the
    /// same slot would satisfy the presence test above and report nothing.
    #[test]
    fn each_class_field_carries_its_own_count() {
        let mut tally = GateTally::new();
        // Distinct counts, so a field wired to the wrong slot shows up as a
        // wrong number rather than coincidentally matching.
        for (i, class) in ErrorClass::ALL.into_iter().enumerate() {
            for _ in 0..=i {
                tally.record_tool_call(Some(class));
            }
        }
        let line = capture_emit(&tally);
        for (i, class) in ErrorClass::ALL.into_iter().enumerate() {
            let want = format!("{}={}", class.field_name(), i + 1);
            assert!(
                line.split_whitespace().any(|tok| tok == want),
                "expected `{want}` on the line: {line}"
            );
        }
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

    /// dirge-e31n.8. Both tool-name miss counters must reach the line, and
    /// they must reach it SEPARATELY: they measure failures that behave
    /// nothing alike — one the model was told about, one nobody was told
    /// about — and a report that merged them could not tell an alias table
    /// working from a model simply retrying.
    #[test]
    fn both_tool_name_miss_counters_reach_the_emitted_line() {
        let mut tally = GateTally::new();
        tally.record_hallucinated_tool_name();
        tally.record_dropped_unknown_name();
        tally.record_dropped_unknown_name();
        let line = capture_emit(&tally);
        let present = emitted_field_names(&line);
        for field in ["hallucinated_tool_names", "dropped_unknown_names"] {
            assert!(present.contains(field), "{field} not emitted\nline: {line}");
        }
        // Distinct values, so a line that emitted one counter twice — the
        // copy-paste this whole family of bugs is made of — fails here.
        assert!(
            line.contains("hallucinated_tool_names=1") && line.contains("dropped_unknown_names=2"),
            "counters crossed or miscounted\nline: {line}"
        );
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
