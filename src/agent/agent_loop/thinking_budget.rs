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
//! first. That means a genuine provider-side `max_tokens` hit during a long
//! think gets the same recovery, which is the right answer there too, and the
//! two halves stay independently testable.
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
use super::types::ThinkingLevel;

/// Default cap on one turn's reasoning trace, in estimated tokens.
///
/// Higher than little-coder's 4096 because dirge is not exclusively driving
/// 9B-class models: 8k is comfortably above a thorough think on a hard task and
/// well below the traces seen when a model is genuinely stuck in a loop.
pub const DEFAULT_THINKING_BUDGET_TOKENS: usize = 8192;

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

/// Install the configured budget process-wide. Idempotent — first call wins.
/// `None` keeps [`DEFAULT_THINKING_BUDGET_TOKENS`]; `Some(0)` disables the
/// breaker entirely.
pub fn init_budget(configured: Option<usize>) {
    if let Some(v) = configured {
        let _ = BUDGET.set(v);
    }
}

/// The effective per-turn reasoning budget in tokens. 0 means disabled.
pub fn budget_tokens() -> usize {
    BUDGET.get().copied().unwrap_or(DEFAULT_THINKING_BUDGET_TOKENS)
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

/// The one instruction the model gets after its reasoning is cut off. Leads
/// with the consequence; names the action rather than the prohibition, because
/// "stop deliberating" alone tends to produce more deliberation about whether
/// to stop.
pub const COMMIT_NUDGE: &str = "[thinking budget exceeded] Your reasoning for that turn ran past \
     the budget and was cut off, and thinking is now disabled for the rest of this task. Commit to \
     an implementation now: pick the most promising approach you already have and use your tools \
     to make progress on it. If you genuinely cannot proceed, say what is blocking you in one \
     sentence instead of reasoning further.";

/// Turn-boundary half of the breaker. One-shot per task.
#[derive(Debug, Default)]
pub struct ThinkingBreaker {
    budget_tokens: usize,
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
    pub fn new(budget_tokens: usize) -> Self {
        Self {
            budget_tokens,
            tripped: false,
        }
    }

    /// Judge a finished assistant message.
    ///
    /// Fires only when the turn *ran out of room* (`StopReason::Length`) with
    /// an over-budget reasoning trace. A model that thought hard and then
    /// answered or called a tool is left alone however long it thought — the
    /// trace length is not itself the problem, failing to convert it into an
    /// action is.
    pub fn inspect(&mut self, msg: &AssistantMessage) -> BreakerAction {
        if self.tripped || self.budget_tokens == 0 {
            return BreakerAction::None;
        }
        if msg.stop_reason != StopReason::Length {
            return BreakerAction::None;
        }
        if thinking_tokens(msg) <= self.budget_tokens {
            return BreakerAction::None;
        }
        self.tripped = true;
        BreakerAction::ForceOff { nudge: COMMIT_NUDGE }
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
        (DEFAULT_THINKING_BUDGET_TOKENS + 100) * CHARS_PER_TOKEN
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
        let mut b = ThinkingBreaker::new(DEFAULT_THINKING_BUDGET_TOKENS);
        let action = b.inspect(&msg(over_budget_chars(), StopReason::Length));
        assert_eq!(action, BreakerAction::ForceOff { nudge: COMMIT_NUDGE });
    }

    /// The trace length is not the problem; failing to convert it into an
    /// action is. A model that thought hard and then acted is left alone.
    #[test]
    fn breaker_ignores_a_long_think_that_produced_a_turn() {
        let mut b = ThinkingBreaker::new(DEFAULT_THINKING_BUDGET_TOKENS);
        for stop in [StopReason::Stop, StopReason::ToolUse] {
            assert_eq!(
                b.inspect(&msg(over_budget_chars(), stop)),
                BreakerAction::None
            );
        }
    }

    /// A `Length` stop with a short trace is an ordinary max_tokens hit on
    /// output, which the loop already handles.
    #[test]
    fn breaker_ignores_a_length_stop_with_little_thinking() {
        let mut b = ThinkingBreaker::new(DEFAULT_THINKING_BUDGET_TOKENS);
        assert_eq!(b.inspect(&msg(200, StopReason::Length)), BreakerAction::None);
    }

    /// One-shot: once thinking is off, a second `Length` turn must not queue
    /// another nudge on top of the first.
    #[test]
    fn breaker_is_one_shot_per_task() {
        let mut b = ThinkingBreaker::new(DEFAULT_THINKING_BUDGET_TOKENS);
        let m = msg(over_budget_chars(), StopReason::Length);
        assert_ne!(b.inspect(&m), BreakerAction::None);
        assert_eq!(b.inspect(&m), BreakerAction::None);
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
