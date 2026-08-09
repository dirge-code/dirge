//! Runaway-reasoning breaker (dirge-1ug5).
//!
//! dirge asks the provider for a thinking budget
//! ([`crate::provider::adapter`] maps it to Anthropic `thinking.budget_tokens`
//! / Gemini `thinking_config`), but nothing enforces one on our side, and a
//! locally-served model honours it only as far as its template does. The
//! failure mode is a model that deliberates without converging: it emits
//! reasoning steadily, so the stream-chunk timeout — which only fires on
//! *silence* — never trips, and the run hangs producing tokens that never
//! become an action.
//!
//! Two halves, and both are needed:
//!
//!   - **[`ReasoningMeter`]**, threaded into the rig stream, stops consuming
//!     once the reasoning trace crosses the budget. That bounds the current
//!     turn, which no existing guard can do — the storm breaker, the failure
//!     tracker and the progress monitor all key on completed turns.
//!   - **[`ThinkingBreaker`]**, at the turn boundary in the run loop, does the
//!     recovery: drop the thinking level to `Off` for the rest of the task and
//!     hand the model one instruction to commit to an implementation.
//!
//! The second half fires off the message itself — a `Length` stop reason plus
//! an over-budget reasoning trace — rather than off a private signal from the
//! first, so a genuine provider-side `max_tokens` hit during a long think gets
//! the same recovery, which is the right answer there too.
//!
//! Be clear about what that does *not* buy: in the ordinary case the meter is
//! what produces the `Length` it keys on, so the cap is the whole decision and
//! there is no independent second opinion. That is precisely why the cap has to
//! be derived rather than picked — see below.
//!
//! # The cap is derived, not a constant (dirge-vzsy)
//!
//! 0.21.15 shipped a flat 8192, carried over from little-coder's 4096 and
//! doubled on the reasoning that dirge drives bigger models. That was wrong by
//! inspection: [`crate::provider::adapter::budget_for_level`] grants **16384**
//! at High/Xhigh, so a high-effort turn was handed 16k of thinking and then cut
//! off at 8k by this module — the harness truncating reasoning it had itself
//! just requested, and then disabling thinking for the rest of the task over it.
//! Traces of 10-30k tokens are ordinary for frontier reasoning models on a hard
//! turn; that is not a runaway, it is the feature working.
//!
//! So the cap is now [`OVERRUN_FACTOR`] × whatever this turn's level was
//! actually granted. It means "the model blew well past its own allocation",
//! which is the real runaway signal, and it can no longer contradict the
//! request we just sent. `thinking_budget_tokens` in config.json still overrides
//! it absolutely for anyone who wants a hard number.
//!
//! # Restoring the level
//!
//! little-coder had to hold "thinking off" across a session and restore it on
//! the next genuine user input, with an `input`-event handler and a `forcedOff`
//! flag re-asserted every turn — their abort replaced the session, so a
//! deferred recovery ran against a stale handle (their issue #8, reproduced
//! twice). None of that applies here: `config` is owned per
//! [`super::run::run_agent_loop`] call, so writing `config.reasoning` lasts
//! exactly as long as the current task and the next user prompt starts from a
//! fresh config. The restore is structural, not a flag.

use super::message::{AssistantMessage, ContentBlock, StopReason};
use super::types::{ThinkingBudgets, ThinkingLevel};

/// How far past its granted allocation a model must reason before the trace is
/// treated as a runaway rather than a hard turn.
///
/// 2× is deliberately loose. A budget is a request, not a hard stop — models
/// overshoot it routinely and legitimately — so the cap has to sit well clear of
/// normal overshoot and still bite decisively on a model emitting hundreds of
/// thousands of tokens without converging. With [`MIN_DERIVED_CAP`] underneath
/// it, the default grants land at 32k for High/Xhigh and the 16k floor for
/// everything below.
pub const OVERRUN_FACTOR: usize = 2;

/// Cap used when the turn's thinking level is unknown — no level resolved, no
/// budgets to read. Matches `OVERRUN_FACTOR` × the High grant, i.e. the most
/// permissive derived value, because guessing low here is what caused
/// dirge-vzsy and a missed runaway is far cheaper than a truncated good turn.
pub const FALLBACK_BUDGET_TOKENS: usize = 32768;

/// Floor under every derived cap.
///
/// [`crate::provider::adapter::budget_for_level`] is only ever *sent* to the two
/// budget-wire providers, Anthropic and Gemini. Everything else — DeepSeek
/// (dirge's default), OpenAI, GLM, Cerebras, openrouter, ollama — takes an
/// effort *string*, so the per-level number was never communicated and is not a
/// budget the model agreed to. Deriving a tight cap from it there would repeat
/// dirge-vzsy in a quieter form: an R1-class model at `reasoning_effort: medium`
/// routinely reasons past 8k, and nothing told it not to.
///
/// So no derived cap sits below this. It costs nothing worth having — the
/// runaways this guard exists for produce hundreds of thousands of tokens, not
/// twenty — and it removes a whole class of "capped tighter than anyone was
/// told" from the design.
pub const MIN_DERIVED_CAP: usize = 16384;

/// Same chars-per-token ratio as [`crate::agent::compression`], so the budget
/// is expressed in the same currency as every other context number.
const CHARS_PER_TOKEN: usize = crate::agent::compression::CHARS_PER_TOKEN as usize;

/// Process-wide resolved budget, installed once at startup from
/// `Config::thinking_budget_tokens` — same OnceLock-set-once convention as
/// `timeout::Timeouts::init` and `context_manager::init_fold_threshold`.
/// Threading it through the stream signature would churn every
/// `wrap_streamed_assistant` caller for a value that is read and never mutated
/// after load.
static BUDGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Install the configured absolute override process-wide. Idempotent — first
/// call wins. `None` leaves the cap derived per turn; `Some(0)` disables the
/// breaker entirely.
pub fn init_budget(configured: Option<usize>) {
    if let Some(v) = configured {
        let _ = BUDGET.set(v);
    }
}

/// The user's absolute override, if they set one.
pub fn configured_override() -> Option<usize> {
    BUDGET.get().copied()
}

/// The cap for a turn at `level`, in estimated tokens. 0 disables the meter.
///
/// `Off` yields 0 on purpose. If thinking is off there is nothing to cut, and
/// nothing the breaker's recovery could do that isn't already done — which is
/// also what stops it firing twice: once it forces the level to `Off`, every
/// later turn in the task resolves to 0 here.
pub fn budget_for_turn(level: Option<ThinkingLevel>, budgets: Option<&ThinkingBudgets>) -> usize {
    if let Some(explicit) = configured_override() {
        return explicit;
    }
    let Some(level) = level else {
        return FALLBACK_BUDGET_TOKENS;
    };
    let granted = crate::provider::adapter::budget_for_level(level, budgets) as usize;
    if granted == 0 {
        return 0; // Off — nothing to meter.
    }
    granted.saturating_mul(OVERRUN_FACTOR).max(MIN_DERIVED_CAP)
}

fn estimate_tokens(chars: usize) -> usize {
    chars.div_ceil(CHARS_PER_TOKEN)
}

/// Running count of reasoning characters within one streamed turn.
///
/// Lives in the stream so the trace can be cut off *while* it is being
/// produced. Cheap by construction: one add and one compare per delta.
#[derive(Debug, Default)]
pub struct ReasoningMeter {
    chars: usize,
    budget_tokens: usize,
}

impl ReasoningMeter {
    /// `budget_tokens` of 0 disables the meter entirely.
    pub fn new(budget_tokens: usize) -> Self {
        Self {
            chars: 0,
            budget_tokens,
        }
    }

    /// Record a reasoning delta. Returns true once the accumulated trace has
    /// crossed the budget — the caller should stop consuming the stream.
    pub fn record(&mut self, delta: &str) -> bool {
        if self.budget_tokens == 0 {
            return false;
        }
        self.chars += delta.len();
        estimate_tokens(self.chars) > self.budget_tokens
    }

    pub fn exceeded(&self) -> bool {
        self.budget_tokens != 0 && estimate_tokens(self.chars) > self.budget_tokens
    }
}

/// Estimated tokens of reasoning carried by a finished assistant message.
pub fn thinking_tokens(msg: &AssistantMessage) -> usize {
    let chars: usize = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Thinking { text } => Some(text.len()),
            _ => None,
        })
        .sum();
    estimate_tokens(chars)
}

/// Harness tag on the commit nudge, so the injection is attributed to the
/// system rather than the user. Registered in [`super::intervention`].
pub const THINKING_TAG: &str = "[thinking-budget]";

/// The one instruction the model gets after its reasoning is cut off. Leads
/// with the consequence; names the action rather than the prohibition, because
/// "stop deliberating" alone tends to produce more deliberation about whether
/// to stop.
pub const COMMIT_NUDGE: &str = "Your reasoning for that turn ran past the budget and was cut \
     off, and thinking is now disabled for the rest of this task. Commit to an implementation \
     now: pick the most promising approach you already have and use your tools to make progress \
     on it. If you genuinely cannot proceed, say what is blocking you in one sentence instead of \
     reasoning further.";

/// Turn-boundary half of the breaker. One-shot per task.
///
/// Holds no budget of its own: the cap belongs to the turn, and
/// `config.reasoning` can change mid-run (the `prepare_next_turn` hook swaps
/// the thinking level). Caching a cap at construction would let the breaker
/// judge a turn against a level that turn was not run at.
#[derive(Debug, Default)]
pub struct ThinkingBreaker {
    tripped: bool,
}

/// What the run loop should do about the turn that just finished.
#[derive(Debug, PartialEq, Eq)]
pub enum BreakerAction {
    /// Nothing to do.
    None,
    /// Force `reasoning` to this level and deliver [`COMMIT_NUDGE`] as a user
    /// message, then take one more turn.
    ForceOff { nudge: &'static str },
}

impl ThinkingBreaker {
    pub fn new() -> Self {
        Self { tripped: false }
    }

    /// Judge a finished assistant message against the cap the turn ran under.
    ///
    /// Fires only when the turn *ran out of room* (`StopReason::Length`) with
    /// an over-budget reasoning trace. A model that thought hard and then
    /// answered or called a tool is left alone however long it thought — the
    /// trace length is not itself the problem, failing to convert it into an
    /// action is.
    pub fn inspect(&mut self, msg: &AssistantMessage, budget_tokens: usize) -> BreakerAction {
        if self.tripped || budget_tokens == 0 {
            return BreakerAction::None;
        }
        if msg.stop_reason != StopReason::Length {
            return BreakerAction::None;
        }
        if thinking_tokens(msg) <= budget_tokens {
            return BreakerAction::None;
        }
        self.tripped = true;
        BreakerAction::ForceOff {
            nudge: COMMIT_NUDGE,
        }
    }

    /// The level to force. Separate from `inspect` so the run loop's
    /// assignment site reads as a single obvious statement.
    pub fn forced_level() -> ThinkingLevel {
        ThinkingLevel::Off
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(thinking_chars: usize, stop: StopReason) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Thinking {
                text: "x".repeat(thinking_chars),
            }],
            stop_reason: stop,
            error_message: None,
        }
    }

    fn over_budget_chars() -> usize {
        (FALLBACK_BUDGET_TOKENS + 100) * CHARS_PER_TOKEN
    }

    /// The bug this module shipped with: the cap must never sit below the
    /// allocation dirge's own request granted, or the harness truncates
    /// reasoning it just asked for and then disables thinking over it.
    #[test]
    fn the_cap_is_never_below_what_the_level_was_granted() {
        for level in [
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
        ] {
            let granted = crate::provider::adapter::budget_for_level(level, None) as usize;
            let cap = budget_for_turn(Some(level), None);
            assert!(
                cap >= granted,
                "{level:?}: cap {cap} is under the granted {granted} — the harness would \
                 cut off reasoning the same request asked for"
            );
        }
    }

    /// Concretely: high effort is granted 16384, so the cap must clear it.
    /// 0.21.15 shipped 8192 here.
    #[test]
    fn high_effort_gets_a_cap_well_clear_of_its_grant() {
        assert_eq!(budget_for_turn(Some(ThinkingLevel::High), None), 32768);
        assert_eq!(budget_for_turn(Some(ThinkingLevel::Xhigh), None), 32768);
    }

    /// The lower levels sit on the floor, not on 2× a number the provider was
    /// never told. DeepSeek and OpenAI take an effort string; `budget_for_level`
    /// never reaches them.
    #[test]
    fn levels_below_high_rest_on_the_floor() {
        for level in [
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
        ] {
            assert_eq!(
                budget_for_turn(Some(level), None),
                MIN_DERIVED_CAP,
                "{level:?} should not be capped below the floor"
            );
        }
    }

    /// Custom budgets flow through — a user who raises `high` raises the cap
    /// with it rather than being cut at a stale constant.
    #[test]
    fn a_configured_grant_scales_the_cap() {
        let budgets = ThinkingBudgets {
            minimal: None,
            low: None,
            medium: None,
            high: Some(60_000),
        };
        assert_eq!(
            budget_for_turn(Some(ThinkingLevel::High), Some(&budgets)),
            120_000
        );
    }

    /// Thinking off means nothing to meter — and it is what stops the breaker
    /// firing twice, since it forces the level to Off.
    #[test]
    fn thinking_off_disables_the_meter() {
        assert_eq!(budget_for_turn(Some(ThinkingLevel::Off), None), 0);
        assert!(!ReasoningMeter::new(0).record(&"x".repeat(1_000_000)));
    }

    /// An unknown level takes the most permissive derived value, never a guess
    /// that could land under the grant.
    #[test]
    fn an_unknown_level_falls_back_to_the_widest_cap() {
        assert_eq!(budget_for_turn(None, None), FALLBACK_BUDGET_TOKENS);
        assert_eq!(
            FALLBACK_BUDGET_TOKENS,
            budget_for_turn(Some(ThinkingLevel::High), None),
            "the fallback must match the widest derived cap"
        );
    }

    #[test]
    fn meter_trips_only_after_the_budget_is_crossed() {
        let mut m = ReasoningMeter::new(10);
        // 10 tokens' worth is at the budget, not over it.
        assert!(!m.record(&"x".repeat(10 * CHARS_PER_TOKEN)));
        assert!(m.record("more reasoning text pushing it over"));
        assert!(m.exceeded());
    }

    #[test]
    fn a_zero_budget_disables_the_meter() {
        let mut m = ReasoningMeter::new(0);
        assert!(!m.record(&"x".repeat(1_000_000)));
        assert!(!m.exceeded());
    }

    #[test]
    fn breaker_fires_on_a_truncated_over_budget_think() {
        let mut b = ThinkingBreaker::new();
        let action = b.inspect(
            &msg(over_budget_chars(), StopReason::Length),
            FALLBACK_BUDGET_TOKENS,
        );
        assert_eq!(
            action,
            BreakerAction::ForceOff {
                nudge: COMMIT_NUDGE
            }
        );
    }

    /// The trace length is not the problem; failing to convert it into an
    /// action is. A model that thought hard and then acted is left alone.
    #[test]
    fn breaker_ignores_a_long_think_that_produced_a_turn() {
        let mut b = ThinkingBreaker::new();
        for stop in [StopReason::Stop, StopReason::ToolUse] {
            assert_eq!(
                b.inspect(&msg(over_budget_chars(), stop), FALLBACK_BUDGET_TOKENS),
                BreakerAction::None
            );
        }
    }

    /// A `Length` stop with a short trace is an ordinary max_tokens hit on
    /// output, which the loop already handles.
    #[test]
    fn breaker_ignores_a_length_stop_with_little_thinking() {
        let mut b = ThinkingBreaker::new();
        assert_eq!(
            b.inspect(&msg(200, StopReason::Length), FALLBACK_BUDGET_TOKENS),
            BreakerAction::None
        );
    }

    /// One-shot: once thinking is off, a second `Length` turn must not queue
    /// another nudge on top of the first.
    #[test]
    fn breaker_is_one_shot_per_task() {
        let mut b = ThinkingBreaker::new();
        let m = msg(over_budget_chars(), StopReason::Length);
        assert_ne!(b.inspect(&m, FALLBACK_BUDGET_TOKENS), BreakerAction::None);
        assert_eq!(b.inspect(&m, FALLBACK_BUDGET_TOKENS), BreakerAction::None);
    }

    /// The breaker holds no cap of its own, so a level swapped mid-run by
    /// `prepare_next_turn` can't leave it judging a turn against a level that
    /// turn never ran at.
    #[test]
    fn the_breaker_judges_against_the_cap_it_is_handed() {
        let m = msg(over_budget_chars(), StopReason::Length);

        // Under a cap wide enough for the trace: nothing to do.
        let mut b = ThinkingBreaker::new();
        assert_eq!(
            b.inspect(&m, usize::MAX),
            BreakerAction::None,
            "a trace inside its cap is not a runaway"
        );

        // Same message, same breaker, a tighter cap: now it fires. The verdict
        // follows the cap it is given, not one captured at construction.
        assert_ne!(b.inspect(&m, 1024), BreakerAction::None);
    }

    #[test]
    fn thinking_tokens_sums_every_thinking_block() {
        let m = AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    text: "a".repeat(CHARS_PER_TOKEN * 3),
                },
                ContentBlock::Text {
                    text: "ignored".repeat(100),
                },
                ContentBlock::Thinking {
                    text: "b".repeat(CHARS_PER_TOKEN * 2),
                },
            ],
            stop_reason: StopReason::Stop,
            error_message: None,
        };
        assert_eq!(thinking_tokens(&m), 5);
    }

    #[test]
    fn the_forced_level_is_off() {
        assert_eq!(ThinkingBreaker::forced_level(), ThinkingLevel::Off);
    }
}
