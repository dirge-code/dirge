//! Adapters from the hook registry onto the agent loop's hook slots, and
//! composition with hooks already installed there (plugins, background
//! follow-ups).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use super::domain::{HookEvent, HookOutcome, Submission, system_reminder};
use super::{CommandHooks, HookBinding};
use super::{dialect, policy};
use crate::agent::agent_loop::hooks::{
    AfterToolCallContext, AfterToolCallFn, BeforeToolCallContext, BeforeToolCallFn,
    BeforeToolCallReturn, GetFollowupMessagesFn,
};
use crate::agent::agent_loop::message::{LoopMessage, UserMessage};
use crate::agent::agent_loop::result::{AfterToolCallResult, BeforeToolCallResult, LoopToolResult};
use crate::permission::ask::{AskRequest, AskSender, UserDecision};

/// Consecutive `Stop` blocks tolerated before the loop is let go.
pub const MAX_CONSECUTIVE_STOP_BLOCKS: usize = 8;

fn cwd() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// `PreToolUse`: each Claude call a dirge call restates to is judged;
/// the first block refuses the call, an "ask" waits on the user's answer
/// at the permission prompt, context rides on its result, and a single
/// call's `updatedInput` is folded back onto the args.
pub fn pre_tool_hook(binding: HookBinding, session_id: Option<String>) -> BeforeToolCallFn {
    Arc::new(move |ctx: BeforeToolCallContext| {
        let hooks = binding.hooks.clone();
        let ask = binding.ask.clone();
        let session_id = session_id.clone();
        Box::pin(async move {
            let calls = dialect::claude_calls(&ctx.tool_call_name, &ctx.args, &cwd());
            let single = calls.len() == 1;
            let mut outcome = HookOutcome::default();
            let mut blocked_as = None;
            let mut asked_as = None;
            for (claude_name, input) in calls {
                let payload = hooks.payload(
                    HookEvent::PreToolUse,
                    session_id.as_deref(),
                    json!({ "tool_name": claude_name, "tool_input": input, "tool_use_id": ctx.tool_call_id }),
                );
                let targets = vec![claude_name.clone(), ctx.tool_call_name.clone()];
                let one = hooks
                    .run_async(HookEvent::PreToolUse, targets, payload)
                    .await;
                let blocks = one.block.is_some();
                if asked_as.is_none() && one.ask.is_some() {
                    asked_as = Some((claude_name.clone(), input));
                }
                outcome = outcome.combine(one);
                if blocks {
                    blocked_as = Some(claude_name);
                    break;
                }
            }
            let pending = outcome.pending_ask().map(str::to_string);
            if let (Some(reason), Some((claude_name, input))) = (pending, asked_as)
                && let Some(refusal) = confirm(ask.as_ref(), &claude_name, &input, &reason).await
            {
                outcome.block = Some(refusal);
                blocked_as = Some(claude_name);
            }
            pre_tool_return(ctx, outcome, single, blocked_as)
        })
    })
}

/// Puts a hook's "ask" to the user at the permission prompt. `None` when
/// the user allows the call, otherwise the reason it is refused. With no
/// prompt to ask (headless, tests) the call is refused, as
/// `spawn_headless_ask_responder` refuses a tool's own ask. "Allow
/// always" allows this call only: hooks do not read permission rules, so
/// the next matching call asks again.
async fn confirm(
    ask: Option<&AskSender>,
    claude_name: &str,
    input: &Value,
    reason: &str,
) -> Option<String> {
    let unavailable =
        || format!("{reason} (confirmation required, but no permission prompt is available)");
    let Some(ask) = ask else {
        return Some(unavailable());
    };
    let (reply, answer) = tokio::sync::oneshot::channel();
    let request = AskRequest {
        tool: claude_name.to_string(),
        input: ask_input(input),
        details: Some(input.to_string()),
        reason: Some(format!("{} hook: {reason}", HookEvent::PreToolUse)),
        reply,
    };
    if ask.send(request).await.is_err() {
        return Some(unavailable());
    }
    match answer.await {
        Ok(UserDecision::AllowOnce | UserDecision::AllowAlways(_)) => None,
        Ok(UserDecision::Deny { note: Some(note) }) => {
            Some(format!("{reason}; the user denied it: {note}"))
        }
        Ok(UserDecision::Deny { note: None }) => Some(format!("{reason}; the user denied it")),
        Err(_) => Some(unavailable()),
    }
}

/// The line a hook's ask shows as the call's input: the field that says
/// what the call does, else the whole input.
fn ask_input(input: &Value) -> String {
    ["command", "file_path", "url", "path", "pattern"]
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| input.to_string())
}

fn pre_tool_return(
    ctx: BeforeToolCallContext,
    outcome: HookOutcome,
    single: bool,
    blocked_as: Option<String>,
) -> BeforeToolCallReturn {
    let args = match (&outcome.updated_input, single) {
        (Some(updated), true) => {
            dialect::dirge_args_from_claude(&ctx.tool_call_name, &ctx.args, updated)
        }
        _ => ctx.args,
    };
    let result = outcome.block.as_ref().map(|reason| BeforeToolCallResult {
        block: Some(true),
        reason: Some(format!(
            "PreToolUse:{} hook error: {reason}",
            blocked_as.as_deref().unwrap_or("tool")
        )),
    });
    BeforeToolCallReturn {
        result,
        args,
        context: outcome
            .context_text()
            .map(|t| system_reminder(HookEvent::PreToolUse, &t))
            .into_iter()
            .collect(),
    }
}

/// `PostToolUse`: a block or context is appended to the result the model
/// sees.
pub fn post_tool_hook(binding: HookBinding, session_id: Option<String>) -> AfterToolCallFn {
    Arc::new(move |ctx: AfterToolCallContext| {
        let hooks = binding.hooks.clone();
        let session_id = session_id.clone();
        Box::pin(async move {
            let claude_name = dialect::claude_tool_name(&ctx.tool_call_name);
            let payload = hooks.payload(
                HookEvent::PostToolUse,
                session_id.as_deref(),
                json!({
                    "tool_name": claude_name,
                    "tool_input": dialect::claude_input(&ctx.tool_call_name, &ctx.args, &cwd()),
                    "tool_use_id": ctx.tool_call_id,
                    "tool_response": dialect::result_text(&ctx.result.content),
                }),
            );
            let targets = vec![claude_name, ctx.tool_call_name.clone()];
            let outcome = hooks
                .run_async(HookEvent::PostToolUse, targets, payload)
                .await;
            post_tool_override(&ctx.result, &outcome)
        })
    })
}

fn post_tool_override(
    result: &LoopToolResult,
    outcome: &HookOutcome,
) -> Option<AfterToolCallResult> {
    let mut notes: Vec<String> = Vec::new();
    if let Some(reason) = &outcome.block {
        notes.push(format!(
            "<system-reminder>\nPostToolUse hook feedback: {reason}\n</system-reminder>"
        ));
    }
    if let Some(text) = outcome.context_text() {
        notes.push(system_reminder(HookEvent::PostToolUse, &text));
    }
    if notes.is_empty() {
        return None;
    }
    let mut content = result.content.clone();
    content.push(json!({ "type": "text", "text": notes.join("\n") }));
    Some(AfterToolCallResult {
        content: Some(content),
        ..AfterToolCallResult::default()
    })
}

/// `Stop` / `SubagentStop` at the outer-loop boundary: a block hands its
/// reason back to the model as a user message and the loop continues.
pub fn stop_followup(binding: HookBinding, session_id: Option<String>) -> GetFollowupMessagesFn {
    let consecutive = Arc::new(AtomicUsize::new(0));
    Arc::new(move || {
        let hooks = binding.hooks.clone();
        let event = binding.stop_event;
        let session_id = session_id.clone();
        let consecutive = consecutive.clone();
        Box::pin(async move {
            let streak = consecutive.load(Ordering::SeqCst);
            if streak >= MAX_CONSECUTIVE_STOP_BLOCKS {
                consecutive.store(0, Ordering::SeqCst);
                return Vec::new();
            }
            let payload = hooks.payload(
                event,
                session_id.as_deref(),
                json!({ "stop_hook_active": streak > 0 }),
            );
            let outcome = hooks.run_async(event, Vec::new(), payload).await;
            match outcome.block {
                Some(reason) => {
                    consecutive.fetch_add(1, Ordering::SeqCst);
                    vec![LoopMessage::User(UserMessage::text(format!(
                        "<system-reminder>\n{event} hook feedback:\n{reason}\n</system-reminder>"
                    )))]
                }
                None => {
                    consecutive.store(0, Ordering::SeqCst);
                    Vec::new()
                }
            }
        })
    })
}

/// `first` then `second`: a block from `first` short-circuits, args flow
/// from one to the next, contexts concatenate.
pub fn compose_before(
    first: Option<BeforeToolCallFn>,
    second: BeforeToolCallFn,
) -> BeforeToolCallFn {
    let Some(first) = first else {
        return second;
    };
    Arc::new(move |ctx: BeforeToolCallContext| {
        let first = first.clone();
        let second = second.clone();
        Box::pin(async move {
            let a = first(ctx.clone()).await;
            if a.result.as_ref().and_then(|r| r.block).unwrap_or(false) {
                return a;
            }
            let b = second(BeforeToolCallContext {
                args: a.args,
                ..ctx
            })
            .await;
            BeforeToolCallReturn {
                context: a.context.into_iter().chain(b.context).collect(),
                ..b
            }
        })
    })
}

/// `first` then `second`, `second` seeing `first`'s overrides; each field
/// `second` sets wins.
pub fn compose_after(first: Option<AfterToolCallFn>, second: AfterToolCallFn) -> AfterToolCallFn {
    let Some(first) = first else {
        return second;
    };
    Arc::new(move |ctx: AfterToolCallContext| {
        let first = first.clone();
        let second = second.clone();
        Box::pin(async move {
            let a = first(ctx.clone()).await;
            let seen = apply_after(ctx, a.as_ref());
            let b = second(seen).await;
            merge_after(a, b)
        })
    })
}

fn apply_after(
    mut ctx: AfterToolCallContext,
    over: Option<&AfterToolCallResult>,
) -> AfterToolCallContext {
    if let Some(over) = over {
        if let Some(content) = &over.content {
            ctx.result.content = content.clone();
        }
        if let Some(details) = &over.details {
            ctx.result.details = details.clone();
        }
        if let Some(is_error) = over.is_error {
            ctx.is_error = is_error;
        }
        if over.terminate.is_some() {
            ctx.result.terminate = over.terminate;
        }
    }
    ctx
}

fn merge_after(
    a: Option<AfterToolCallResult>,
    b: Option<AfterToolCallResult>,
) -> Option<AfterToolCallResult> {
    match (a, b) {
        (None, b) => b,
        (a, None) => a,
        (Some(a), Some(b)) => Some(AfterToolCallResult {
            content: b.content.or(a.content),
            details: b.details.or(a.details),
            is_error: b.is_error.or(a.is_error),
            terminate: b.terminate.or(a.terminate),
        }),
    }
}

/// `first`'s follow-ups when it has any; otherwise the loop really is
/// about to stop, so `stop` is consulted.
pub fn compose_followup(
    first: Option<GetFollowupMessagesFn>,
    stop: GetFollowupMessagesFn,
) -> GetFollowupMessagesFn {
    let Some(first) = first else {
        return stop;
    };
    Arc::new(move || {
        let first = first.clone();
        let stop = stop.clone();
        Box::pin(async move {
            let pending = first().await;
            if pending.is_empty() {
                stop().await
            } else {
                pending
            }
        })
    })
}

/// `system_prompt` with the session's `SessionStart` context appended.
pub fn with_session_context(
    hooks: &CommandHooks,
    system_prompt: String,
    session_id: Option<&str>,
    resumed: bool,
) -> String {
    append_context(
        system_prompt,
        HookEvent::SessionStart,
        hooks.session_start_context(session_id, resumed),
    )
}

/// A child's `system_prompt` with its `SubagentStart` context appended.
pub fn with_subagent_context(
    hooks: &CommandHooks,
    system_prompt: String,
    agent_id: &str,
) -> String {
    append_context(
        system_prompt,
        HookEvent::SubagentStart,
        hooks.subagent_start_context(agent_id, SUBAGENT_TYPE),
    )
}

/// `UserPromptSubmit` for `prompt`: the text the model receives, or the
/// reason it must not be called at all. See [`policy::submission`].
pub fn submitted_prompt(
    hooks: &CommandHooks,
    session_id: Option<&str>,
    prompt: String,
) -> Submission {
    let outcome = hooks.user_prompt_submit(session_id, &prompt);
    policy::submission(outcome, prompt)
}

/// `agent_type` reported for dirge's `task` subagents.
pub const SUBAGENT_TYPE: &str = "task";

fn append_context(base: String, event: HookEvent, context: Option<String>) -> String {
    match context {
        Some(text) => format!("{base}\n\n{}", system_reminder(event, &text)),
        None => base,
    }
}

/// Installs a binding's hooks on a loop config, composing with what is
/// already there.
pub fn install(
    config: &mut crate::agent::agent_loop::types::LoopConfig,
    binding: &HookBinding,
    session_id: Option<String>,
) {
    let hooks = &binding.hooks;
    if hooks.has(HookEvent::PreToolUse) {
        config.before_tool_call = Some(compose_before(
            config.before_tool_call.take(),
            pre_tool_hook(binding.clone(), session_id.clone()),
        ));
    }
    if hooks.has(HookEvent::PostToolUse) {
        config.after_tool_call = Some(compose_after(
            config.after_tool_call.take(),
            post_tool_hook(binding.clone(), session_id.clone()),
        ));
    }
    if hooks.has(binding.stop_event) {
        config.get_followup_messages = Some(compose_followup(
            config.get_followup_messages.take(),
            stop_followup(binding.clone(), session_id),
        ));
    }
}
