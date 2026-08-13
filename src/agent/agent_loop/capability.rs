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
//!    - **Weakest (weight 1)** — most errored calls and `repair_successful`: a
//!      call ran and failed (often environmental, often transient), or the
//!      model fumbled and then *recovered*. Friction, not stuckness.
//!    - **One exception (weight 2)** — an errored call classified
//!      [`ErrorClass::MissingInfo`]. See below.
//!
//!    Errored calls are NOT one signal. They arrive split by
//!    [`ErrorClass`], and `MissingInfo` — the call named a file, symbol or
//!    pattern that is not there — scores double. That class is the *wandering*
//!    tell: the model is operating on a wrong picture of the tree, and it is
//!    the one failure mode no other counter here can see. A wandering run
//!    emits many DIFFERENT well-formed calls, so `storm` sees no repeat,
//!    `scavenge` and `repair` see nothing malformed, and the streak never
//!    reaches three because the occasional call does succeed. Every other
//!    class stays at weight 1 on purpose: a timeout or an `EACCES` says
//!    something about the environment, not about the model, and raising them
//!    would lower the bar for genuine friction — the exact mistake this weight
//!    is meant to avoid. `Unclassified` stays at 1 too, which is what keeps
//!    unrecognised errors byte-identical to before classification existed.
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
//! [`CapabilityTier::scale_threshold`] and [`CapabilityTier::scale_budget`]
//! map a base value to the tier's working value. Adaptation is
//! **one-directional** — the tier may add support, never remove it:
//!
//! ```text
//!               threshold ("intervene after N")   budget ("at most N")
//!   Nominal     base                              base
//!   Strong      base                              base       <- drives NOTHING
//!   Struggling  base / SUPPORT_SCALE  (sooner)    base * SUPPORT_SCALE (more)
//! ```
//!
//! `Strong` scaling identically to `Nominal` is deliberate and is the single
//! most important thing to understand here. The counters above observe
//! **tool-call mechanics only** — nothing in [`CapabilityCounters`] moves
//! based on whether the model verifies its work, checks the right gate, or
//! makes progress on the task. So a `Strong` reading is evidence about
//! argument hygiene and about nothing else, and it cannot license relaxing a
//! guard that fires on progress or verification. See [`CapabilityTier::Strong`]
//! for the two concrete failures that make the point.
//!
//! Two consequences worth knowing before wiring a new threshold here:
//!
//! - Deriving a guard whose trigger is unrelated to tool-call mechanics buys
//!   nothing, because only `Struggling` moves and `Struggling` is rare.
//! - A budget of exactly 1 cannot move at all, because `1 * 3 / 2` truncates
//!   straight back to 1. Routing a one-shot budget through the estimator
//!   looks like adaptation and changes nothing.
//!
//! Every weight, floor, threshold and ratio is a named constant below — there
//! are no magic numbers in the body. Tuning happens in one place.

use super::tool_error_class::ErrorClass;

// --- weights: how damning each failure signal is (see principle 2 above) ---

/// Weight for an errored call of any class but [`ErrorClass::MissingInfo`].
/// Weakest: a call ran and failed, often for environmental or transient
/// reasons. This is also the pre-classification weight for EVERY errored call,
/// so a run whose errors the classifier does not recognise scores exactly as it
/// did before this split existed.
const W_ERRORED: u32 = 1;
/// Weight for an errored call classified [`ErrorClass::MissingInfo`] — the
/// model asked for something that is not there.
///
/// **2 is not a guess, and it is not free to move.** It is the only integer
/// that satisfies every run whose counters are on record, and
/// [`tests::the_missing_info_weight_is_pinned_by_the_observed_runs`] is that
/// sweep, written so it fails if the constant moves in either direction:
///
/// - at **1** (i.e. no split at all) the two measured blowups — 17 and 26
///   varied, well-formed calls at 24% and 27% error rates — score 235‰ and
///   269‰, both under the 333‰ bar, and nothing tightens. That is the observed
///   failure this whole change exists to catch.
/// - at **3** the glm and qwen runs in [`CapabilityTier::Struggling`]'s table
///   cross the bar. Both COMPLETED THE TASK. Branding a run that succeeded is
///   the failure mode that gets a safety net switched off.
///
/// The bound is deliberately computed against the worst case: the grounding
/// runs predate classification, so their class mix was never recorded, and the
/// sweep scores their errors as if ALL of them were `MissingInfo`. If the mix
/// was in fact softer the real headroom is larger, never smaller.
const W_ERRORED_MISSING_INFO: u32 = 2;
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

/// The weight one errored call carries, by what kind of failure it was.
///
/// Exhaustive on purpose: a new [`ErrorClass`] cannot be added without stating
/// what it is worth here. Defaulting a new class to the weakest weight would be
/// the safe-looking choice that silently reintroduces exactly the blindness
/// this function exists to remove.
const fn errored_weight(class: ErrorClass) -> u32 {
    match class {
        // The wandering tell — see the module docs.
        ErrorClass::MissingInfo => W_ERRORED_MISSING_INFO,
        // Environmental, not capability: a timeout or an EACCES says the world
        // pushed back, and the model changing what it does cannot help.
        ErrorClass::Transient | ErrorClass::Fatal => W_ERRORED,
        // Already counted on the repair axis: `repair_invalid` (weight 4) and
        // `repair_successful` (weight 1) both observe argument hygiene, so
        // raising this would double-count the same signal.
        ErrorClass::Misuse => W_ERRORED,
        // The non-regression floor: an error the classifier declined to name
        // must weigh exactly what every error weighed before the split.
        ErrorClass::Unclassified => W_ERRORED,
    }
}

/// The split must be a real split. If these two ever became equal the whole
/// weighting would be a no-op and every test above would still pass — so it is
/// asserted at COMPILE time rather than left to a test that could be deleted
/// with the code still building.
const _: () = assert!(
    W_ERRORED_MISSING_INFO > W_ERRORED,
    "missing-info must outweigh ordinary friction, or the class split changes nothing"
);

/// `errored_by_class` for `n` errored calls that all share one class.
///
/// Most callers — every test, and any counter snapshot built from a run that
/// only ever saw one kind of failure — want this rather than an array literal
/// whose positions have to be counted by hand.
#[allow(dead_code)] // used by tests and by callers building a single-class snapshot
pub fn errored_all(class: ErrorClass, n: u32) -> [u32; ErrorClass::ALL.len()] {
    let mut by_class = [0; ErrorClass::ALL.len()];
    by_class[class.index()] = n;
    by_class
}

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

// --- scale ratio (num, den); applied to `Struggling` ONLY ---

/// The single adaptation ratio. Applied as `1/2` to a threshold (intervene at
/// half the base count) and as `3/2` to a budget (half again as many nudges
/// allowed) — see [`CapabilityTier::scale_threshold`] and
/// [`CapabilityTier::scale_budget`].
///
/// `Nominal` and `Strong` are BOTH `(1,1)`, i.e. bit-identical to the
/// pre-estimator constants. Adaptation is deliberately **one-directional**:
/// the tier may add support, never remove it. See
/// [`CapabilityTier::Strong`] for why.
const SUPPORT_SCALE_NUM: u64 = 3;
const SUPPORT_SCALE_DEN: u64 = 2;

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
    /// The model is coping well on tool-call mechanics: no malformed
    /// arguments, no invented tool names, few or no errored calls.
    ///
    /// # This tier deliberately drives NOTHING
    ///
    /// It scales identically to [`Self::Nominal`], and that is not an
    /// oversight. Adaptation here is one-directional — the tier may add
    /// support, never remove it — for a reason that is structural rather than
    /// statistical.
    ///
    /// [`CapabilityCounters`] observes tool-call mechanics *only*: errored
    /// calls, repaired arguments, invented names, scavenged text, storms,
    /// failure streaks. Not one of those fields changes based on whether the
    /// model verifies its work, makes progress on the task, or checks the
    /// right gate. So a `Strong` reading is evidence about **argument
    /// hygiene** and about nothing else, and cannot support relaxing a guard
    /// that fires on progress or verification.
    ///
    /// The epic's own record makes the point concretely. Both failures worth
    /// having a guard for came from models this estimator reads as `Strong`:
    ///
    /// - the 60-turn reconnaissance thrash (dirge-t5dh) was deepseek-flash
    ///   with a 0% tool-call error rate — flawless mechanics, zero files
    ///   written
    /// - the wrong-gate verification failure (dirge-w2de) came from the same
    ///   tier, which is what `docs/verification-discipline.md` is about
    ///
    /// An earlier cut of this work scaled `FAST_VERIFY_EDIT_THRESHOLD` up for
    /// `Strong`, on the argument that extra latitude for a model with no
    /// observed failures could not cause a nudge storm. True, but it inverts
    /// the risk: it relaxes verification pressure on precisely the class of
    /// model that produced the only verification failures on record. Removed.
    ///
    /// Keep observing it — it is the distinction the estimator discriminates
    /// most cleanly (deepseek 6/6 `Strong`, glm 3/6 `Nominal`, tracking
    /// failure exactly), which makes it useful telemetry and useful to the A/B
    /// harness. Just do not let it take a guard away.
    Strong,
    /// The model is visibly failing: thresholds are tightened so the loop
    /// intervenes and offers help sooner.
    ///
    /// # This tier is a SAFETY NET, and not firing is the normal outcome
    ///
    /// Measured across the supported capability range on the `recon-real`
    /// scenario, it never fired once:
    ///
    /// | model                | calls | errored | max streak | rep_invalid | tier    |
    /// |----------------------|-------|---------|------------|-------------|---------|
    /// | deepseek-flash       | ~22   | 0%      | 0          | 0           | strong  |
    /// | glm                  | ~19   | ~4%     | 1          | 1           | nominal |
    /// | Qwen3.6-27B-Q8 local | 20    | 15%     | 2          | 0           | nominal |
    ///
    /// Qwen3.6-27B is the agreed LOW BOUND of supported models — whatever
    /// works with it is good enough generally. So `Nominal` is the bottom of
    /// the supported range, and this tier sits BELOW that range: it is for a
    /// model doing materially worse than the low bound, or for a very
    /// difficult long-horizon task where even a capable model degrades.
    ///
    /// **Do not tune the weights or the streak override to make this fire.**
    /// It is supposed to be quiet in normal operation, the same way
    /// [`super::progress`]'s prologue bound is. Making it fire would start
    /// nudging models that are coping: qwen completed the task at a 15% error
    /// rate, and a run that succeeds is not one to intervene on.
    ///
    /// Equally, do not delete it as dead code. Reaching it requires a run
    /// worse than the low bound, which is exactly the case worth having a net
    /// for, and the observation wiring costs nothing when it stays quiet.
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
    /// `Nominal` and `Strong` both return `base` bit-identically; only
    /// `Struggling` moves it, dividing by `SUPPORT_SCALE` so help arrives
    /// sooner. Integer arithmetic. See [`CapabilityTier::Strong`] for why
    /// `Strong` does not relax anything.
    ///
    /// FLOORED AT 1 for any non-zero base. `base / 2` truncates to 0 at
    /// `base == 1`, and a threshold of 0 fires unconditionally — every
    /// boundary, forever. Several real thresholds are exactly 1, so without
    /// the floor the Struggling tier would turn them into a nudge storm
    /// aimed at the model least able to cope with one. `base == 0` means the
    /// knob is off and stays off.
    pub fn scale_threshold(self, base: u32) -> u32 {
        if base == 0 {
            return 0;
        }
        match self {
            CapabilityTier::Nominal | CapabilityTier::Strong => base,
            CapabilityTier::Struggling => {
                ((base as u64 * SUPPORT_SCALE_DEN / SUPPORT_SCALE_NUM) as u32).max(1)
            }
        }
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
    /// `Nominal` and `Strong` are both bit-identical; only `Struggling` moves,
    /// multiplying by `SUPPORT_SCALE` for more nudges. Floored at 1 for a
    /// non-zero base for the same reason as above.
    ///
    /// Note the floor makes this a **no-op for any budget of exactly 1**:
    /// `1 * 3 / 2` truncates back to 1. `MAX_TRACK_NUDGES`,
    /// `MAX_VERIFY_NUDGES` and `MAX_PROLOGUE_NUDGES` are all 1, so routing
    /// them through here would look like adaptation and change nothing. Give
    /// a one-shot budget a larger base before deriving it, or leave it alone.
    pub fn scale_budget(self, base: u32) -> u32 {
        if base == 0 {
            return 0;
        }
        match self {
            CapabilityTier::Nominal | CapabilityTier::Strong => base,
            CapabilityTier::Struggling => {
                ((base as u64 * SUPPORT_SCALE_NUM / SUPPORT_SCALE_DEN) as u32).max(1)
            }
        }
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
    /// Calls that ran and returned an error, SPLIT BY [`ErrorClass`] and
    /// indexed by [`ErrorClass::index`] — build it with [`errored_all`] rather
    /// than by counting array positions.
    ///
    /// Split rather than totalled because the weights differ: see
    /// [`errored_weight`]. The total is still available as
    /// [`CapabilityCounters::errored_tool_calls`], derived from this.
    pub errored_by_class: [u32; ErrorClass::ALL.len()],
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
    /// Total errored calls this run — DERIVED from the per-class split, never
    /// stored beside it, so the two cannot disagree.
    pub fn errored_tool_calls(&self) -> u32 {
        self.errored_by_class.iter().sum()
    }

    /// Pure classification of these counters into a raw [`CapabilityTier`], with
    /// no hysteresis. See the module docs for the exact formula.
    fn raw_tier(&self) -> CapabilityTier {
        self.raw_tier_at(W_ERRORED_MISSING_INFO)
    }

    /// [`Self::raw_tier`] with the missing-info weight supplied, so the weight
    /// sweep can ask "what would this run classify as at weight N?" against the
    /// SAME implementation production uses.
    ///
    /// A sweep that re-implemented the formula would be testing its own copy —
    /// it would stay green while the real boundaries moved underneath it, which
    /// is how a test written from buggy output becomes the contract.
    fn raw_tier_at(&self, w_missing_info: u32) -> CapabilityTier {
        // Warm-up: too few calls to judge — stay Nominal (and avoid div-by-zero).
        if self.tool_calls < MIN_CALLS_FOR_ESTIMATE {
            return CapabilityTier::Nominal;
        }
        // Stuck override: an unbroken failure streak means the model is bogged
        // down even when the overall rate looks tolerable.
        if self.max_failure_streak >= STREAK_FORCE_STRUGGLING {
            return CapabilityTier::Struggling;
        }
        // Errored calls carry a per-class weight; everything else is flat.
        let errored: u64 = ErrorClass::ALL
            .into_iter()
            .map(|class| {
                let weight = if class == ErrorClass::MissingInfo {
                    w_missing_info
                } else {
                    errored_weight(class)
                };
                self.errored_by_class[class.index()] as u64 * weight as u64
            })
            .sum();
        // Weighted failure rate, in parts per thousand, over tool_calls.
        let weighted = errored
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
            errored_by_class: errored_all(ErrorClass::Unclassified, 4),
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

    /// An errored call of an unrecognised class must weigh exactly what every
    /// errored call weighed before the split existed. This is the
    /// non-regression criterion for the whole change, so it is asserted
    /// against the constant rather than against a hard-coded 1.
    #[test]
    fn every_class_but_missing_info_keeps_the_pre_split_weight() {
        for class in ErrorClass::ALL {
            let expected = if class == ErrorClass::MissingInfo {
                W_ERRORED_MISSING_INFO
            } else {
                W_ERRORED
            };
            assert_eq!(
                errored_weight(class),
                expected,
                "{class:?} weight drifted from the documented split"
            );
        }
        // That the split is a REAL split — the two weights differing — is
        // asserted at compile time next to the constants, not here: a runtime
        // assertion on two consts is one clippy silences and a maintainer
        // deletes.
    }

    /// The same run classifies differently by what its failures WERE, not just
    /// how many there were. This is the discrimination the split buys, stated
    /// on one pair of counter sets identical in every other respect.
    #[test]
    fn identical_error_counts_rank_differently_by_class() {
        let wandering = CapabilityCounters {
            tool_calls: 26,
            errored_by_class: errored_all(ErrorClass::MissingInfo, 7),
            ..Default::default()
        };
        let friction = CapabilityCounters {
            tool_calls: 26,
            errored_by_class: errored_all(ErrorClass::Transient, 7),
            ..Default::default()
        };
        assert_eq!(
            wandering.errored_tool_calls(),
            friction.errored_tool_calls()
        );
        assert_eq!(
            wandering.raw_tier(),
            CapabilityTier::Struggling,
            "7 of 26 calls naming things that aren't there is a run operating on a wrong map"
        );
        assert_ne!(
            friction.raw_tier(),
            CapabilityTier::Struggling,
            "the same count of timeouts is the environment pushing back, not the model failing"
        );
    }

    // ---------------------------------------------------------------------
    // dirge-s9ry: the missing-info weight, swept rather than guessed.
    //
    // Every run below is one whose counters are actually on record — the two
    // measured blowups, and the three grounding runs in `CapabilityTier`'s
    // table. The sweep asks each candidate weight to satisfy all five.
    // ---------------------------------------------------------------------

    /// One recorded run and what the estimator must say about it.
    struct Recorded {
        what: &'static str,
        counters: CapabilityCounters,
        must_struggle: bool,
    }

    /// The observed runs, scored at their WORST CASE: every one of these
    /// predates classification, so the class mix was never recorded and each
    /// run's errors are counted as if all of them were missing-info. That is
    /// the conservative direction — it makes the upper bound on the weight as
    /// tight as it can be, so a weight that passes here passes on the real
    /// mix too.
    fn recorded_runs() -> Vec<Recorded> {
        vec![
            // The two blowups this change exists to catch (dirge-e31n). Both
            // hit the turn cap having produced nothing: varied, well-formed
            // calls that storm, scavenge and repair all correctly ignored,
            // with a streak that never reached the STREAK_FORCE_STRUGGLING
            // override. Nothing but the class of the errors distinguishes
            // these from ordinary friction.
            Recorded {
                what: "blowup A: 17 calls, 24% errored, streak 2",
                counters: CapabilityCounters {
                    tool_calls: 17,
                    errored_by_class: errored_all(ErrorClass::MissingInfo, 4),
                    max_failure_streak: 2,
                    ..Default::default()
                },
                must_struggle: true,
            },
            Recorded {
                what: "blowup B: 26 calls, 27% errored, streak 2",
                counters: CapabilityCounters {
                    tool_calls: 26,
                    errored_by_class: errored_all(ErrorClass::MissingInfo, 7),
                    max_failure_streak: 2,
                    ..Default::default()
                },
                must_struggle: true,
            },
            // The grounding runs. All three COMPLETED THE TASK, and qwen is
            // the agreed low bound of supported models — branding any of them
            // is how a safety net gets switched off.
            Recorded {
                what: "deepseek-flash recon-real: 35 calls, clean",
                counters: CapabilityCounters {
                    tool_calls: 35,
                    ..Default::default()
                },
                must_struggle: false,
            },
            Recorded {
                what: "glm recon-real (good run): 20 calls, 1 errored, 1 unrepairable",
                counters: CapabilityCounters {
                    tool_calls: 20,
                    errored_by_class: errored_all(ErrorClass::MissingInfo, 1),
                    repair_invalid: 1,
                    max_failure_streak: 1,
                    ..Default::default()
                },
                must_struggle: false,
            },
            Recorded {
                what: "Qwen3.6-27B-Q8 local: 20 calls, 15% errored, streak 2",
                counters: CapabilityCounters {
                    tool_calls: 20,
                    errored_by_class: errored_all(ErrorClass::MissingInfo, 3),
                    max_failure_streak: 2,
                    ..Default::default()
                },
                must_struggle: false,
            },
        ]
    }

    /// Which recorded runs a candidate weight gets wrong.
    fn misclassified_at(weight: u32) -> Vec<&'static str> {
        recorded_runs()
            .into_iter()
            .filter(|r| {
                let struggling = r.counters.raw_tier_at(weight) == CapabilityTier::Struggling;
                struggling != r.must_struggle
            })
            .map(|r| r.what)
            .collect()
    }

    /// The shipped weight satisfies every recorded run — and the neighbours on
    /// both sides do not. Without the second half this would pass for any
    /// weight in a range and pin nothing.
    #[test]
    fn the_missing_info_weight_is_pinned_by_the_observed_runs() {
        assert!(
            misclassified_at(W_ERRORED_MISSING_INFO).is_empty(),
            "weight {W_ERRORED_MISSING_INFO} misclassifies: {:?}",
            misclassified_at(W_ERRORED_MISSING_INFO)
        );

        // Too low — this IS the pre-split behaviour, and it is exactly the
        // blowups it lets through.
        let too_low = misclassified_at(W_ERRORED_MISSING_INFO - 1);
        assert!(
            !too_low.is_empty(),
            "weight {} classifies everything correctly too, so {W_ERRORED_MISSING_INFO} is not pinned from below",
            W_ERRORED_MISSING_INFO - 1
        );
        assert!(
            too_low.iter().all(|w| w.starts_with("blowup")),
            "under-weighting should miss the blowups, not brand a healthy run: {too_low:?}"
        );

        // Too high — and what it breaks is a run that finished the job.
        let too_high = misclassified_at(W_ERRORED_MISSING_INFO + 1);
        assert!(
            !too_high.is_empty(),
            "weight {} classifies everything correctly too, so {W_ERRORED_MISSING_INFO} is not pinned from above",
            W_ERRORED_MISSING_INFO + 1
        );
        assert!(
            too_high.iter().all(|w| !w.starts_with("blowup")),
            "over-weighting should brand healthy runs, not miss blowups: {too_high:?}"
        );
    }

    /// The sweep must be reading the shipped constant, not a copy. If
    /// `raw_tier` stopped routing through `raw_tier_at` the sweep above would
    /// keep passing while production used something else entirely.
    #[test]
    fn raw_tier_uses_the_swept_weight() {
        for r in recorded_runs() {
            assert_eq!(
                r.counters.raw_tier(),
                r.counters.raw_tier_at(W_ERRORED_MISSING_INFO),
                "{}: raw_tier diverged from the swept implementation",
                r.what
            );
        }
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

    /// Adaptation is ONE-DIRECTIONAL: the tier may add support, never remove
    /// it. `Nominal` and `Strong` are both the bit-identical no-op; only
    /// `Struggling` moves a threshold. See `CapabilityTier::Strong` for why —
    /// the counters observe tool-call mechanics only, so a `Strong` reading
    /// cannot justify relaxing a progress or verification guard.
    #[test]
    fn scale_is_identity_for_nominal_and_strong() {
        for base in [0u32, 1, 7, 42, 1_000, u32::MAX] {
            assert_eq!(
                CapabilityTier::Nominal.scale_threshold(base),
                base,
                "Nominal must return base bit-identically"
            );
            assert_eq!(
                CapabilityTier::Strong.scale_threshold(base),
                base,
                "Strong must NOT relax a threshold — it drives nothing"
            );
        }
        // Only Struggling moves, and only toward earlier intervention.
        assert_eq!(CapabilityTier::Struggling.scale_threshold(10), 6);
        assert!(CapabilityTier::Struggling.scale_threshold(10) < 10);
    }

    #[test]
    fn rates_not_counts() {
        // Same absolute failure count (4 errored calls), different denominators.
        // 4 failures over 40 calls is a 10% rate -> Nominal.
        let diluted = CapabilityCounters {
            tool_calls: 40,
            errored_by_class: errored_all(ErrorClass::Unclassified, 4),
            ..Default::default()
        };
        // 4 failures over 6 calls is a ~67% rate -> Struggling.
        let concentrated = CapabilityCounters {
            tool_calls: 6,
            errored_by_class: errored_all(ErrorClass::Unclassified, 4),
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
            CapabilityTier::Struggling.scale_threshold(base) < base,
            "...and gets it sooner — the two axes move opposite ways"
        );
        // Nominal and Strong are the bit-identical no-op on both axes.
        for tier in [CapabilityTier::Nominal, CapabilityTier::Strong] {
            assert_eq!(tier.scale_budget(base), base);
            assert_eq!(tier.scale_threshold(base), base);
        }
        // And budgets floor at 1 too.
        for b in 1..=4u32 {
            assert!(CapabilityTier::Struggling.scale_budget(b) >= 1);
        }
    }

    /// A budget of exactly 1 cannot be adapted, because `1 * 3 / 2` truncates
    /// straight back to 1. Several real budgets are 1 (`MAX_TRACK_NUDGES`,
    /// `MAX_VERIFY_NUDGES`, `MAX_PROLOGUE_NUDGES`), so wiring them through
    /// the estimator would look like adaptation and do nothing. Pinned so the
    /// no-op is a documented property rather than a surprise.
    #[test]
    fn a_budget_of_one_is_a_structural_no_op() {
        for tier in [
            CapabilityTier::Struggling,
            CapabilityTier::Nominal,
            CapabilityTier::Strong,
        ] {
            assert_eq!(
                tier.scale_budget(1),
                1,
                "{tier:?} must leave a one-shot budget alone"
            );
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
            errored_by_class: errored_all(ErrorClass::Unclassified, 5),
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
            errored_by_class: errored_all(ErrorClass::Unclassified, 5),
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
            errored_by_class: errored_all(ErrorClass::Unclassified, 1),
            repair_invalid: 1,
            max_failure_streak: 1,
            ..Default::default()
        };
        assert_ne!(settled(&good), CapabilityTier::Struggling);
    }
}
