//! `beforeToolCall` and `afterToolCall` config hooks.
//!
//! Faithful port of pi's hook surface at agent-loop.ts:578-708.
//!
//! Pi's hooks are JavaScript callbacks that receive a context
//! object and may MUTATE the args in place (test pi:310). Rust
//! can't compose `&mut` cleanly with `Pin<Box<dyn Future>>`, so
//! we pass `args` by value and return the (possibly mutated)
//! args alongside the hook result. The dispatcher threads the
//! returned args forward — semantically identical to pi's
//! mutate-in-place but with explicit data flow.

use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use super::message::{AssistantMessage, LoopMessage, ToolResultMessage};
use super::result::{AfterToolCallResult, BeforeToolCallResult, LoopToolResult};
use super::types::{Context, TurnUpdate};

/// Context passed to `beforeToolCall`. Port of pi
/// `BeforeToolCallContext` (types.ts:84).
///
/// Fields are owned values (clones) so the hook closure can be
/// `Fn(Ctx) -> Future` rather than `Fn(&Ctx) -> Future` — the
/// latter is hairy with `Pin<Box<dyn Future>>` lifetimes. Pi's
/// hooks receive references to mutable JS objects; we trade a
/// small clone overhead for a clean async-fn shape.
#[derive(Debug, Clone)]
pub struct BeforeToolCallContext {
    // assistant_message + tool_call_id are carried for API completeness
    // (pi types.ts:84) but not read by current plugin hooks.
    #[allow(dead_code)]
    pub assistant_message: AssistantMessage,
    #[allow(dead_code)]
    pub tool_call_id: String,
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub tool_call_name: String,
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub args: Value,
}

/// Return value of `beforeToolCall`. Pi returns
/// `Promise<BeforeToolCallResult | undefined>` AND mutates the
/// context's `args` in place. Since Rust can't elegantly mutate
/// through a moved value, the closure returns BOTH the result
/// (possibly None) and the (possibly mutated) args.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallReturn {
    /// Pi's return value: `block?` + `reason?`. `None` means
    /// "let the call proceed unchanged".
    pub result: Option<BeforeToolCallResult>,
    /// Possibly-mutated args. Even when `result` is None, these
    /// args are what the tool executes with. Hooks that don't
    /// mutate should return the input args unchanged.
    pub args: Value,
    /// Notes for the model, prepended to this call's result.
    pub context: Vec<String>,
}

/// `beforeToolCall` hook signature. Pi (types.ts:262):
///   `(context: BeforeToolCallContext, signal?) => Promise<BeforeToolCallResult | undefined>`
pub type BeforeToolCallFn = Arc<
    dyn Fn(BeforeToolCallContext) -> Pin<Box<dyn Future<Output = BeforeToolCallReturn> + Send>>
        + Send
        + Sync,
>;

/// Context passed to `afterToolCall`. Port of pi
/// `AfterToolCallContext` (types.ts:96).
#[derive(Debug, Clone)]
pub struct AfterToolCallContext {
    // assistant_message, tool_call_id, args, is_error are carried
    // for API completeness (pi types.ts:96) but not read by current plugin hooks.
    #[allow(dead_code)]
    pub assistant_message: AssistantMessage,
    #[allow(dead_code)]
    pub tool_call_id: String,
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub tool_call_name: String,
    #[allow(dead_code)]
    pub args: Value,
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub result: LoopToolResult,
    #[allow(dead_code)]
    pub is_error: bool,
}

/// `afterToolCall` hook signature. Pi (types.ts:276):
///   `(context: AfterToolCallContext, signal?) => Promise<AfterToolCallResult | undefined>`
///
/// Returning `None` keeps the executed result verbatim; returning
/// `Some(AfterToolCallResult { … })` overrides any of the four
/// fields per pi's merge semantics (content/details/isError/
/// terminate replace in full when Some).
pub type AfterToolCallFn = Arc<
    dyn Fn(
            AfterToolCallContext,
        ) -> Pin<Box<dyn Future<Output = Option<AfterToolCallResult>> + Send>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------
// Phase 4 hooks: prepareNextTurn, shouldStopAfterTurn,
// getSteeringMessages, getFollowupMessages.
// ---------------------------------------------------------------

/// Context passed to `prepareNextTurn` AND `shouldStopAfterTurn`.
/// Pi has `PrepareNextTurnContext extends ShouldStopAfterTurnContext`
/// (types.ts:133) — same shape, two aliases. We define one struct.
///
/// Port of pi `ShouldStopAfterTurnContext` (types.ts:112):
///   `{ message, toolResults, context, newMessages }`
#[derive(Debug, Clone)]
pub struct TurnHookContext {
    // All fields are carried for API completeness (pi types.ts:112)
    // but current plugin hooks (prepare_next_turn, should_stop_after_turn)
    // use _ctx — they read from PluginManager slots instead.
    #[allow(dead_code)]
    pub message: AssistantMessage,
    #[allow(dead_code)]
    pub tool_results: Vec<ToolResultMessage>,
    #[allow(dead_code)]
    pub context: Context,
    #[allow(dead_code)]
    pub new_messages: Vec<LoopMessage>,
}

/// `prepareNextTurn` hook signature. Port of pi
/// `prepareNextTurn?` (types.ts:215):
///   `(context) => AgentLoopTurnUpdate | undefined | Promise<...>`
///
/// `None` means "no changes — continue with current state". The
/// returned `TurnUpdate`'s `Some` fields replace the
/// corresponding loop config / context for the next turn.
pub type PrepareNextTurnFn = Arc<
    dyn Fn(TurnHookContext) -> Pin<Box<dyn Future<Output = Option<TurnUpdate>> + Send>>
        + Send
        + Sync,
>;

/// `shouldStopAfterTurn` hook signature. Port of pi
/// `shouldStopAfterTurn?` (types.ts:208):
///   `(context) => boolean | Promise<boolean>`
///
/// Returning `true` requests a graceful stop after the current
/// turn — the loop emits `agent_end` and exits without polling
/// steering or follow-up queues.
pub type ShouldStopAfterTurnFn =
    Arc<dyn Fn(TurnHookContext) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// `getSteeringMessages` hook signature. Port of pi
/// `getSteeringMessages?` (types.ts:230):
///   `() => Promise<AgentMessage[]>`
///
/// Polled at: (a) loop entry, (b) after each `turn_end` /
/// `prepareNextTurn` / `shouldStopAfterTurn` cycle. Returned
/// messages inject BEFORE the next assistant response.
pub type GetSteeringMessagesFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<LoopMessage>> + Send>> + Send + Sync>;

/// `getFollowupMessages` hook signature. Port of pi
/// `getFollowUpMessages?` (types.ts:243):
///   `() => Promise<AgentMessage[]>`
///
/// Polled at the OUTER-loop boundary — when the inner loop has
/// no more tool calls AND no pending steering. Non-empty return
/// triggers the outer loop to re-enter the inner loop with these
/// messages as pending.
pub type GetFollowupMessagesFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<LoopMessage>> + Send>> + Send + Sync>;

/// Polled at the finalization boundary. Returns `true` when the run should
/// defer finalization — e.g. coordinated subagent work is still running, so the
/// parent turn should end WITHOUT the critic (and lower gates) firing and be
/// re-woken when the batch becomes deliverable.
pub type ShouldDeferFinalizationFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// What a run opens with: its system prompt, the prompt that starts it, and
/// the reminders its first user turn leads with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOpening {
    pub system_prompt: String,
    pub prompt: String,
    pub reminders: Vec<String>,
}

impl RunOpening {
    /// The text of the first user turn: the reminders, then the prompt.
    pub fn first_turn_text(&self) -> String {
        if self.reminders.is_empty() {
            self.prompt.clone()
        } else {
            format!("{}\n\n{}", self.reminders.join("\n"), self.prompt)
        }
    }
}

/// Amends a run's opening before its first model call. Runs inside the
/// run's task, on the agent runtime.
pub type OpenRunFn =
    Arc<dyn Fn(RunOpening) -> Pin<Box<dyn Future<Output = RunOpening> + Send>> + Send + Sync>;

/// `first`, then `second` on what `first` answered.
pub fn compose_open_run(first: Option<OpenRunFn>, second: Option<OpenRunFn>) -> Option<OpenRunFn> {
    match (first, second) {
        (Some(first), Some(second)) => Some(Arc::new(move |opening: RunOpening| {
            let (first, second) = (first.clone(), second.clone());
            Box::pin(async move { second(first(opening).await).await })
        })),
        (first, second) => first.or(second),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `BeforeToolCallReturn::default()` is the no-op outcome —
    /// result=None, args=Null. Hooks that "did nothing" return
    /// effectively this shape (with the input args instead of
    /// Null).
    #[test]
    fn before_return_default() {
        let r = BeforeToolCallReturn::default();
        assert!(r.result.is_none());
        assert_eq!(r.args, Value::Null);
    }

    fn appending(text: &'static str) -> OpenRunFn {
        Arc::new(move |mut opening: RunOpening| {
            Box::pin(async move {
                opening.system_prompt.push_str(text);
                opening
            })
        })
    }

    #[tokio::test]
    async fn open_run_steps_run_in_order() {
        assert!(compose_open_run(None, None).is_none());
        let both = compose_open_run(Some(appending("a")), Some(appending("b"))).unwrap();
        assert_eq!(both(RunOpening::default()).await.system_prompt, "ab");
        let second = compose_open_run(None, Some(appending("b"))).unwrap();
        assert_eq!(second(RunOpening::default()).await.system_prompt, "b");
    }

    #[test]
    fn the_first_turn_leads_with_its_reminders() {
        let mut opening = RunOpening {
            prompt: "do it".into(),
            ..RunOpening::default()
        };
        assert_eq!(opening.first_turn_text(), "do it");
        opening.reminders = vec!["<r1>".into(), "<r2>".into()];
        assert_eq!(opening.first_turn_text(), "<r1>\n<r2>\n\ndo it");
    }
}
