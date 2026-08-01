//! Empirical capability estimation — steer from OBSERVED failure, not model
//! identity (dirge-5mtx.7).
//!
//! dirge ships ~14 behavioural thresholds (repair budgets, storm/streak
//! breakers, safe-state arming, compaction triggers, …) tuned by hand for an
//! unstated assumed model capability. That assumption is wrong often enough to
//! hurt: the tier a model deserves should derive from what the model in front
//! of us is ACTUALLY doing this run, not from its name or its provider.
//!
//! ## The finding that drives the design
//!
//! On a real reconnaissance scenario two models ran the same task:
//!
//! - glm — *reputedly* the stronger model — showed `errored_tool_calls=5`,
//!   `repair_invalid=4`, `max_failure_streak=3`: it was visibly flailing.
//! - deepseek-flash — *reputedly* the weaker model — showed `0 / 0 / 0`: it
//!   was coping perfectly.
//!
//! Reputation inverted reality. A model that is coping must be left alone
//! whichever model it is; a model that is visibly failing must get help sooner.
//! So the tier is estimated from OBSERVED FAILURE IN THIS RUN, never from model
//! identity or provider name.
//!
//! ## Design principles
//!
//! 1. **Observe, don't assume.** [`CapabilityTier`] is a pure function of the
//!    run's own failure counters ([`CapabilityCounters`]), read from the same
//!    observation-only tallies the `gate_tally` module already keeps. No model
//!    name, no provider, no static "fast/slow" flag feeds it.
//!
//! 2. **Rates, not counts.** Four failures mean different things over a 40-call
//!    run than over a 6-call run, so every signal is judged as a ratio over
//!    `tool_calls`. The signals are weighted by how damning they are:
//!    - **Strongest (weight 4)** — `repair_invalid` and `scavenged_calls`: the
//!      model could not produce a dispatchable tool call *at all* — the args
//!      were unrepairable, or it emitted tool-call-shaped text instead of a
//!      real call. The clearest "out of its depth" tell.
//!    - **Medium (weight 2)** — `hallucinated_tool_names` and
//!      `storm_suppressions`: the model tried a native dispatch but to a
//!      non-existent tool, or had to be reined in from rapid-fire repeats.
//!      Wrong, but still inside the dispatch grammar.
//!    - **Weakest (weight 1)** — `errored_tool_calls` and `repair_successful`:
//!      a call ran and failed (often environmental, often transient), or the
//!      model fumbled and then *recovered*. Friction, not stuckness.
//!
//! 3. **Hysteresis.** A single bad observation must not yank a coping run into
//!    [`CapabilityTier::Struggling`], nor must a single clean observation
//!    rescue a flailing one. [`CapabilityEstimator`] requires
//!    [`HYSTERESIS_FLIP_RUNS`] consecutive observations of a *different* tier
//!    before the published tier changes, in both directions.
//!
//! 4. **`Nominal` is a no-op.** [`CapabilityTier::scale`] returns `base`
//!    bit-identically for [`CapabilityTier::Nominal`] — until a run earns a
//!    different tier, every threshold behaves exactly as it did before this
//!    module existed.
//!
//! ## The formula (all integer arithmetic)
//!
//! Let `c_i` be a counter and `w_i` its weight (named constants below). The
//! weighted failure rate, in parts per thousand, is:
//!
//! ```text
//! rate_permille = ( sum of w_i * c_i ) * 1000 / tool_calls
//! ```
//!
//! The raw tier (`CapabilityCounters::raw_tier`) is then:
//!
//! - `tool_calls < MIN_CALLS_FOR_ESTIMATE` → [`CapabilityTier::Nominal`]
//!   (warm-up: too little data to judge).
//! - else `max_failure_streak >= STREAK_FORCE_STRUGGLING` →
//!   [`CapabilityTier::Struggling`] (an unbroken run of failures means stuck,
//!   independent of the overall rate).
//! - else `rate_permille < STRONG_MAX_PER_MILLE` → [`CapabilityTier::Strong`].
//! - else `rate_permille >= STRUGGLING_MIN_PER_MILLE` →
//!   [`CapabilityTier::Struggling`].
//! - otherwise → [`CapabilityTier::Nominal`].
//!
//! [`CapabilityTier::scale`] maps a base threshold to the tier's working value:
//!
//! ```text
//! scale(base) = base * num / den
//!   Nominal    -> (1, 1)   // bit-identical to base
//!   Strong     -> (3, 2)   // coping: more latitude before intervening
//!   Struggling -> (1, 2)   // failing: tighten limits, intervene sooner
//! ```
//!
//! Every weight, floor, threshold and ratio is a named constant below — there
//! are no magic numbers in the body. Tuning happens in one place.

// --- weights: how damning each failure signal is (see principle 2 above) ---

/// Weight for `errored_tool_calls`. Weakest: a call ran and failed, often for
/// environmental or transient reasons.
const W_ERRORED: u32 = 1;
/// Weight for `repair_successful`. Weakest: the model fumbled but then
/// recovered — friction, not stuckness.
const W_REPAIR_SUCCESS: u32 = 1;
/// Weight for `hallucinated_tool_names`. Medium: dispatched to a non-existent
/// tool, but still inside the call grammar.
const W_HALLUCINATED: u32 = 2;
/// Weight for `storm_suppressions`. Medium: had to be reined in from
/// rapid-fire repeats.
const W_STORM: u32 = 2;
/// Weight for `repair_invalid`. Strongest: the args were so malformed the
/// repair pass gave up — the model could not produce a dispatchable call.
const W_REPAIR_INVALID: u32 = 4;
/// Weight for `scavenged_calls`. Strongest: the model emitted tool-call-shaped
/// TEXT instead of a real call — never inside the dispatch grammar.
const W_SCAVENGED: u32 = 4;

// --- rate basis and tier boundaries ---

/// Fixed-point basis for the weighted failure rate: parts per thousand.
const PER_MILLE: u64 = 1_000;
/// Warm-up floor. Below this many tool calls there is too little data to judge,
/// so the run stays `Nominal`.
const MIN_CALLS_FOR_ESTIMATE: u32 = 5;
/// An unbroken streak of this many failures forces `Struggling` regardless of
/// the overall rate: a long failure streak means the model is stuck even when
/// the run as a whole looks tolerable.
const STREAK_FORCE_STRUGGLING: u32 = 3;
/// A weighted failure rate strictly below this (per-mille) earns `Strong`.
const STRONG_MAX_PER_MILLE: u64 = 50;
/// A weighted failure rate at or above this (per-mille) earns `Struggling`.
const STRUGGLING_MIN_PER_MILLE: u64 = 333;

// --- hysteresis ---

/// Consecutive observations of a *different* tier required before the
/// published tier flips. 2 means a single differing observation is absorbed.
const HYSTERESIS_FLIP_RUNS: u32 = 2;

// --- scale ratios (num, den) per tier; Nominal is (1,1) so it is bit-identical ---

/// `Strong` scales a base threshold UP: a coping model gets more latitude.
const STRONG_SCALE_NUM: u64 = 3;
const STRONG_SCALE_DEN: u64 = 2;
/// `Struggling` scales a base threshold DOWN: a failing model is tightened and
/// intervened on sooner.
const STRUGGLING_SCALE_NUM: u64 = 1;
const STRUGGLING_SCALE_DEN: u64 = 2;

/// A model's observed capability tier for the current run.
///
/// Derived purely from [`CapabilityCounters`] — never from model identity. See
/// the module docs for how it is computed and what [`CapabilityTier::scale`]
/// does to a threshold.
#[allow(dead_code)] // pending dirge-5mtx.7 loop-control wiring
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityTier {
    /// Default. Every threshold behaves exactly as it did before this module.
    Nominal,
    /// The model is coping well: thresholds are relaxed to give it more
    /// latitude before the loop intervenes.
    Strong,
    /// The model is visibly failing: thresholds are tightened so the loop
    /// intervenes and offers help sooner.
    Struggling,
}

#[allow(dead_code)] // pending dirge-5mtx.7 loop-control wiring
impl CapabilityTier {
    /// Stable lowercase wire name, for the `dirge::gates` telemetry line and
    /// anything scraping it. Kept stable across refactors — the A/B harness
    /// keys on these strings.
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilityTier::Struggling => "struggling",
            CapabilityTier::Nominal => "nominal",
            CapabilityTier::Strong => "strong",
        }
    }

    /// Scale a THRESHOLD — "intervene after N". Lower means sooner.
    ///
    /// `Nominal` returns `base` bit-identically (the no-op path). `Strong`
    /// multiplies by `3/2` (more latitude before the loop steps in);
    /// `Struggling` by `1/2` (help arrives sooner). Integer arithmetic.
    ///
    /// FLOORED AT 1 for any non-zero base. `base * 1 / 2` truncates to 0 at
    /// `base == 1`, and a threshold of 0 fires unconditionally — every
    /// boundary, forever. Several real thresholds are exactly 1, so without
    /// the floor the Struggling tier would turn them into a nudge storm
    /// aimed at the model least able to cope with one. `base == 0` means the
    /// knob is off and stays off.
    pub fn scale_threshold(self, base: u32) -> u32 {
        if base == 0 {
            return 0;
        }
        let (num, den) = match self {
            CapabilityTier::Nominal => (1u64, 1u64),
            CapabilityTier::Strong => (STRONG_SCALE_NUM, STRONG_SCALE_DEN),
            CapabilityTier::Struggling => (STRUGGLING_SCALE_NUM, STRUGGLING_SCALE_DEN),
        };
        ((base as u64 * num / den) as u32).max(1)
    }

    /// Scale a BUDGET — "at most N of these per run". Higher means more help.
    ///
    /// This is the OPPOSITE direction from [`Self::scale_threshold`] and the
    /// distinction is load-bearing. A struggling model should be helped
    /// *sooner* (lower threshold) and *more often* (higher budget). Running
    /// both through one scaler would halve the budget of the model that needs
    /// it most — turning `MAX_TRACK_NUDGES`, `MAX_VERIFY_NUDGES` and
    /// `MAX_PROLOGUE_NUDGES`, all of which are exactly 1, into 0 and
    /// disabling those nudges entirely for a failing run.
    ///
    /// `Nominal` is bit-identical; `Struggling` gets `3/2`; `Strong` gets
    /// `1/2` (a coping model needs fewer interruptions). Floored at 1 for a
    /// non-zero base for the same reason as above.
    pub fn scale_budget(self, base: u32) -> u32 {
        if base == 0 {
            return 0;
        }
        let (num, den) = match self {
            CapabilityTier::Nominal => (1u64, 1u64),
            CapabilityTier::Struggling => (STRONG_SCALE_NUM, STRONG_SCALE_DEN),
            CapabilityTier::Strong => (STRUGGLING_SCALE_NUM, STRUGGLING_SCALE_DEN),
        };
        ((base as u64 * num / den) as u32).max(1)
    }
}

/// Accumulated failure signals for one run. The loop fills these from the same
/// observation-only tallies `gate_tally` already keeps; nothing here is
/// model-identity-derived. All fields start at zero via [`Default`].
#[allow(dead_code)] // pending dirge-5mtx.7 loop-control wiring
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapabilityCounters {
    /// Total tool calls dispatched this run — the denominator for every rate.
    pub tool_calls: u32,
    /// Calls that ran and returned an error (weight [`W_ERRORED`]).
    pub errored_tool_calls: u32,
    /// Calls whose args were too malformed to repair (weight [`W_REPAIR_INVALID`]).
    pub repair_invalid: u32,
    /// Calls that were malformed but were successfully repaired (weight
    /// [`W_REPAIR_SUCCESS`]).
    pub repair_successful: u32,
    /// Dispatches to a tool name that does not exist (weight [`W_HALLUCINATED`]).
    pub hallucinated_tool_names: u32,
    /// Times a tool-call storm was suppressed (weight [`W_STORM`]).
    pub storm_suppressions: u32,
    /// Tool-call-shaped TEXT scavenged from the assistant message instead of a
    /// real call (weight [`W_SCAVENGED`]).
    pub scavenged_calls: u32,
    /// Longest unbroken run of consecutive failures seen this run.
    pub max_failure_streak: u32,
}

#[allow(dead_code)] // pending dirge-5mtx.7 loop-control wiring
impl CapabilityCounters {
    /// Pure classification of these counters into a raw [`CapabilityTier`], with
    /// no hysteresis. See the module docs for the exact formula.
    fn raw_tier(&self) -> CapabilityTier {
        // Warm-up: too few calls to judge — stay Nominal (and avoid div-by-zero).
        if self.tool_calls < MIN_CALLS_FOR_ESTIMATE {
            return CapabilityTier::Nominal;
        }
        // Stuck override: an unbroken failure streak means the model is bogged
        // down even when the overall rate looks tolerable.
        if self.max_failure_streak >= STREAK_FORCE_STRUGGLING {
            return CapabilityTier::Struggling;
        }
        // Weighted failure rate, in parts per thousand, over tool_calls.
        let weighted = self.errored_tool_calls as u64 * W_ERRORED as u64
            + self.repair_successful as u64 * W_REPAIR_SUCCESS as u64
            + self.hallucinated_tool_names as u64 * W_HALLUCINATED as u64
            + self.storm_suppressions as u64 * W_STORM as u64
            + self.repair_invalid as u64 * W_REPAIR_INVALID as u64
            + self.scavenged_calls as u64 * W_SCAVENGED as u64;
        let rate_permille = weighted * PER_MILLE / self.tool_calls as u64;
        if rate_permille < STRONG_MAX_PER_MILLE {
            CapabilityTier::Strong
        } else if rate_permille >= STRUGGLING_MIN_PER_MILLE {
            CapabilityTier::Struggling
        } else {
            CapabilityTier::Nominal
        }
    }
}

/// Stateful capability estimator. Wraps [`CapabilityCounters::raw_tier`] with
/// hysteresis so the published tier cannot flap on a single observation.
///
/// Feed it a snapshot of the run's counters whenever the loop wants to
/// re-estimate; [`CapabilityEstimator::observe`] returns the (possibly
/// unchanged) published tier.
#[allow(dead_code)] // pending dirge-5mtx.7 loop-control wiring
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityEstimator {
    /// Currently published tier.
    tier: CapabilityTier,
    /// The differing tier a run is drifting toward, if any.
    candidate: Option<CapabilityTier>,
    /// Consecutive observations matching `candidate`.
    runs: u32,
}

#[allow(dead_code)] // pending dirge-5mtx.7 loop-control wiring
impl CapabilityEstimator {
    /// New estimator, published tier [`CapabilityTier::Nominal`].
    pub fn new() -> Self {
        Self {
            tier: CapabilityTier::Nominal,
            candidate: None,
            runs: 0,
        }
    }

    /// The currently published tier.
    pub fn tier(&self) -> CapabilityTier {
        self.tier
    }

    /// Re-estimate from a snapshot of the run's counters, applying hysteresis.
    ///
    /// Returns the published tier, which changes only after
    /// [`HYSTERESIS_FLIP_RUNS`] consecutive observations of a different raw
    /// tier.
    pub fn observe(&mut self, counters: &CapabilityCounters) -> CapabilityTier {
        let computed = counters.raw_tier();
        if computed == self.tier {
            // Back in line with the published tier: forget any drift.
            self.candidate = None;
            self.runs = 0;
        } else if self.candidate == Some(computed) {
            // Sustained drift in the same direction.
            self.runs += 1;
            if self.runs >= HYSTERESIS_FLIP_RUNS {
                self.tier = computed;
                self.candidate = None;
                self.runs = 0;
            }
        } else {
            // First observation of a new differing tier — don't flip yet.
            self.candidate = Some(computed);
            self.runs = 1;
        }
        self.tier
    }
}

impl Default for CapabilityEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean run (all signals zero) past the warm-up floor.
    fn clean(calls: u32) -> CapabilityCounters {
        CapabilityCounters {
            tool_calls: calls,
            ..Default::default()
        }
    }

    #[test]
    fn warm_up_returns_nominal_below_floor() {
        // Below MIN_CALLS_FOR_ESTIMATE even a failure-heavy snapshot is Nominal:
        // too little data to judge, and this avoids div-by-zero.
        let c = CapabilityCounters {
            tool_calls: MIN_CALLS_FOR_ESTIMATE - 1,
            errored_tool_calls: 4,
            repair_invalid: 4,
            max_failure_streak: 4,
            ..Default::default()
        };
        assert_eq!(c.raw_tier(), CapabilityTier::Nominal);
    }

    #[test]
    fn clean_stream_reaches_strong() {
        // A clean stream past the floor is Strong — the model is coping.
        assert_eq!(clean(10).raw_tier(), CapabilityTier::Strong);
        assert_ne!(clean(10).raw_tier(), CapabilityTier::Struggling);

        // Through the estimator it stays Nominal during the first observation
        // (hysteresis) and reaches Strong after a second — never Struggling.
        let mut est = CapabilityEstimator::new();
        let snapshot = clean(10);
        est.observe(&snapshot);
        assert_ne!(est.tier(), CapabilityTier::Struggling);
        est.observe(&snapshot);
        assert_eq!(est.tier(), CapabilityTier::Strong);
    }

    #[test]
    fn heavy_repair_and_scavenge_struggles() {
        // repair_invalid and scavenged_calls are the strongest tells; a run
        // heavy in them is Struggling even at a modest absolute count.
        let c = CapabilityCounters {
            tool_calls: 12,
            repair_invalid: 3,
            scavenged_calls: 2,
            ..Default::default()
        };
        assert_eq!(c.raw_tier(), CapabilityTier::Struggling);

        // The same picture through the estimator flips to Struggling after the
        // hysteresis window.
        let mut est = CapabilityEstimator::new();
        est.observe(&c);
        assert_eq!(est.tier(), CapabilityTier::Nominal);
        est.observe(&c);
        assert_eq!(est.tier(), CapabilityTier::Struggling);
    }

    #[test]
    fn hysteresis_blocks_single_observation_flip_both_directions() {
        // --- Strong held against one Struggling blip ---
        let mut est = CapabilityEstimator::new();
        let good = clean(20);
        let bad = CapabilityCounters {
            tool_calls: 6,
            repair_invalid: 4,
            scavenged_calls: 2,
            ..Default::default()
        };
        // Establish Strong.
        est.observe(&good);
        est.observe(&good);
        assert_eq!(est.tier(), CapabilityTier::Strong);
        // A single bad observation must NOT flip it.
        est.observe(&bad);
        assert_eq!(
            est.tier(),
            CapabilityTier::Strong,
            "single bad observation must not flip Strong -> Struggling"
        );
        // Back to clean: still Strong.
        est.observe(&good);
        assert_eq!(est.tier(), CapabilityTier::Strong);

        // --- Struggling held against one clean blip ---
        let mut est = CapabilityEstimator::new();
        est.observe(&bad);
        est.observe(&bad);
        assert_eq!(est.tier(), CapabilityTier::Struggling);
        // A single clean observation must NOT rescue it.
        est.observe(&good);
        assert_eq!(
            est.tier(),
            CapabilityTier::Struggling,
            "single clean observation must not flip Struggling -> Strong"
        );
        est.observe(&bad);
        assert_eq!(est.tier(), CapabilityTier::Struggling);
    }

    #[test]
    fn scale_is_identity_for_nominal_and_directional_otherwise() {
        // Nominal is bit-identical: the no-op path.
        for base in [0u32, 1, 7, 42, 1_000, u32::MAX] {
            assert_eq!(
                CapabilityTier::Nominal.scale_threshold(base),
                base,
                "Nominal must return base bit-identically"
            );
        }
        // Strong scales up, Struggling scales down.
        assert_eq!(CapabilityTier::Strong.scale_threshold(10), 15);
        assert_eq!(CapabilityTier::Struggling.scale_threshold(10), 5);
        assert!(CapabilityTier::Strong.scale_threshold(10) > 10);
        assert!(CapabilityTier::Struggling.scale_threshold(10) < 10);
    }

    #[test]
    fn rates_not_counts() {
        // Same absolute failure count (4 errored calls), different denominators.
        // 4 failures over 40 calls is a 10% rate -> Nominal.
        let diluted = CapabilityCounters {
            tool_calls: 40,
            errored_tool_calls: 4,
            ..Default::default()
        };
        // 4 failures over 6 calls is a ~67% rate -> Struggling.
        let concentrated = CapabilityCounters {
            tool_calls: 6,
            errored_tool_calls: 4,
            ..Default::default()
        };
        let diluted_tier = diluted.raw_tier();
        let concentrated_tier = concentrated.raw_tier();
        assert_eq!(diluted_tier, CapabilityTier::Nominal);
        assert_eq!(concentrated_tier, CapabilityTier::Struggling);
        // The diluted run ranks strictly better (less help needed).
        fn rank(t: CapabilityTier) -> u8 {
            match t {
                CapabilityTier::Strong => 0,
                CapabilityTier::Nominal => 1,
                CapabilityTier::Struggling => 2,
            }
        }
        assert!(rank(diluted_tier) < rank(concentrated_tier));
    }

    /// A threshold of 1 must never scale to 0. `base * 1 / 2` truncates to
    /// zero, and a zero threshold fires unconditionally — on EVERY boundary,
    /// at the model least able to absorb a nudge storm. MAX_TRACK_NUDGES,
    /// MAX_VERIFY_NUDGES and MAX_PROLOGUE_NUDGES are all exactly 1, so this
    /// is the common case, not an edge case.
    #[test]
    fn struggling_threshold_never_truncates_to_zero() {
        for base in 1..=4u32 {
            assert!(
                CapabilityTier::Struggling.scale_threshold(base) >= 1,
                "scale_threshold({base}) must not zero out the gate"
            );
        }
        // An explicitly-off knob stays off.
        assert_eq!(CapabilityTier::Struggling.scale_threshold(0), 0);
        assert_eq!(CapabilityTier::Strong.scale_threshold(0), 0);
    }

    /// Budgets scale the OPPOSITE way from thresholds. A struggling model
    /// should be helped sooner (lower threshold) AND more often (higher
    /// budget). Running both through one scaler would halve the budget of the
    /// run that needs it most.
    #[test]
    fn budget_scales_opposite_to_threshold() {
        let base = 4;
        assert!(
            CapabilityTier::Struggling.scale_budget(base) > base,
            "a failing run gets MORE help, not less"
        );
        assert!(
            CapabilityTier::Strong.scale_budget(base) < base,
            "a coping run gets fewer interruptions"
        );
        // Nominal is the bit-identical no-op on both axes.
        assert_eq!(CapabilityTier::Nominal.scale_budget(base), base);
        assert_eq!(CapabilityTier::Nominal.scale_threshold(base), base);
        // And budgets floor at 1 too.
        for b in 1..=4u32 {
            assert!(CapabilityTier::Strong.scale_budget(b) >= 1);
        }
    }

    // ---------------------------------------------------------------------
    // Grounding tests: the estimator run against counters ACTUALLY OBSERVED
    // on the recon-real scenario (an extract of this repo's own agent loop),
    // not synthetic streams.
    //
    // These matter because run-to-run variance on that scenario is ~2x on
    // turns and tool calls, so nothing about steering can be validated by
    // comparing means at affordable sample sizes (dirge-5mtx.6, FM-5). What
    // CAN be checked is that the estimator's classification of a run matches
    // what a human reading that run's counters would say. That is a
    // structural claim, and it holds at n=1.
    //
    // The headline observation: on the SAME task, the stronger model (glm)
    // was the one visibly failing while the weaker one (deepseek-flash) was
    // clean. Any estimator keyed on model identity gets this exactly
    // backwards, which is why tier is derived from observed failure only.
    // ---------------------------------------------------------------------

    /// Settle the estimator on a steady stream (hysteresis needs agreement).
    fn settled(counters: &CapabilityCounters) -> CapabilityTier {
        let mut est = CapabilityEstimator::new();
        for _ in 0..3 {
            est.observe(counters);
        }
        est.tier()
    }

    #[test]
    fn observed_glm_thrash_run_reads_as_struggling() {
        // glm, recon-real: turns=36 tools=40 err=5 streak=3 rep_inv=4.
        let obs = CapabilityCounters {
            tool_calls: 40,
            errored_tool_calls: 5,
            repair_invalid: 4,
            max_failure_streak: 3,
            ..Default::default()
        };
        assert_eq!(
            settled(&obs),
            CapabilityTier::Struggling,
            "4 undispatchable calls and a 3-long failure streak is a run in trouble"
        );
    }

    #[test]
    fn observed_deepseek_clean_run_is_not_struggling() {
        // deepseek-flash, recon-real: turns=33 tools=35, zero failures of
        // every kind. The WEAKER model, coping fine on the same task.
        let obs = CapabilityCounters {
            tool_calls: 35,
            ..Default::default()
        };
        assert_ne!(
            settled(&obs),
            CapabilityTier::Struggling,
            "a clean run must never be branded struggling because the model is small"
        );
    }

    /// The pair, stated as one invariant: identical task, and the estimator
    /// must rank them by how the run went rather than by which model it was.
    #[test]
    fn tier_tracks_the_run_not_the_model() {
        let strong_model_bad_run = CapabilityCounters {
            tool_calls: 40,
            errored_tool_calls: 5,
            repair_invalid: 4,
            max_failure_streak: 3,
            ..Default::default()
        };
        let weak_model_good_run = CapabilityCounters {
            tool_calls: 35,
            ..Default::default()
        };
        assert_eq!(settled(&strong_model_bad_run), CapabilityTier::Struggling);
        assert_ne!(settled(&weak_model_good_run), CapabilityTier::Struggling);
    }

    /// glm's OTHER recon-real run — same model, same task, 20 calls with one
    /// error and a 1-long streak. It must not be branded Struggling: the
    /// difference between this and the run above is the run, not the model,
    /// and that is precisely the discrimination being asked for.
    #[test]
    fn same_model_clean_run_ranks_above_its_own_bad_run() {
        let good = CapabilityCounters {
            tool_calls: 20,
            errored_tool_calls: 1,
            repair_invalid: 1,
            max_failure_streak: 1,
            ..Default::default()
        };
        assert_ne!(settled(&good), CapabilityTier::Struggling);
    }
}
