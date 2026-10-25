//! Adapters from the addon host onto dirge's hook points: the agent loop's
//! before/after tool-call slots, and the main session's system prompt and
//! submitted prompt.
//!
//! Composition reuses the command-hooks combinators, so addon hooks chain
//! after Janet plugin and command hooks with one semantics: a block from an
//! earlier hook short-circuits, args flow forward, contexts concatenate.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::domain::{BeforeOutcome, HookPoint};
use super::host::AddonHost;
use super::policy;
use crate::agent::addon_hooks::AddonHooks;
use crate::agent::agent_loop::LoopTool;
use crate::agent::agent_loop::hooks::{
    AfterToolCallContext, AfterToolCallFn, BeforeToolCallContext, BeforeToolCallFn,
    BeforeToolCallReturn, OpenRunFn, RunOpening,
};
use crate::agent::agent_loop::result::{AfterToolCallResult, BeforeToolCallResult, LoopToolResult};
use crate::agent::agent_loop::types::LoopConfig;
use crate::agent::command_hooks::loop_hooks::{compose_after, compose_before};
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::runtime::blocking_within;

/// `:dirge/before-tool-call`, adapted onto the loop's slot.
pub fn before_hook(host: Arc<AddonHost>) -> BeforeToolCallFn {
    Arc::new(move |ctx: BeforeToolCallContext| {
        let host = host.clone();
        Box::pin(async move {
            let payload = json!({
                "tool": ctx.tool_call_name,
                "args": ctx.args,
                "tool-call-id": ctx.tool_call_id,
            });
            let outcome = tokio::task::spawn_blocking(move || host.before_tool_call(&payload))
                .await
                .unwrap_or_default();
            before_return(ctx, outcome)
        })
    })
}

/// The loop's answer for a folded addon outcome.
fn before_return(ctx: BeforeToolCallContext, outcome: BeforeOutcome) -> BeforeToolCallReturn {
    BeforeToolCallReturn {
        result: outcome.block.map(|(addon, reason)| BeforeToolCallResult {
            block: Some(true),
            reason: Some(format!("blocked by addon {addon}: {reason}")),
        }),
        args: outcome.args.unwrap_or(ctx.args),
        context: outcome
            .context
            .iter()
            .map(|text| policy::reminder(HookPoint::BeforeToolCall, text))
            .collect(),
    }
}

/// `:dirge/after-tool-call`: addon context is appended to the result the
/// model sees.
pub fn after_hook(host: Arc<AddonHost>) -> AfterToolCallFn {
    Arc::new(move |ctx: AfterToolCallContext| {
        let host = host.clone();
        Box::pin(async move {
            let payload = json!({
                "tool": ctx.tool_call_name,
                "args": ctx.args,
                "result": policy::content_text(&ctx.result.content),
                "error?": ctx.is_error,
            });
            let texts =
                tokio::task::spawn_blocking(move || host.texts(HookPoint::AfterToolCall, &payload))
                    .await
                    .unwrap_or_default();
            after_override(&ctx.result, &texts)
        })
    })
}

fn after_override(result: &LoopToolResult, texts: &[String]) -> Option<AfterToolCallResult> {
    if texts.is_empty() {
        return None;
    }
    let notes: Vec<String> = texts
        .iter()
        .map(|t| policy::reminder(HookPoint::AfterToolCall, t))
        .collect();
    let mut content = result.content.clone();
    content.push(json!({ "type": "text", "text": notes.join("\n") }));
    Some(AfterToolCallResult {
        content: Some(content),
        ..AfterToolCallResult::default()
    })
}

/// Install the host's tool-call hooks on `config`, after whatever is there.
/// Points no addon listens on install nothing.
pub fn install(config: &mut LoopConfig, host: &Arc<AddonHost>) {
    if host.listens(HookPoint::BeforeToolCall) {
        config.before_tool_call = Some(compose_before(
            config.before_tool_call.take(),
            before_hook(host.clone()),
        ));
    }
    if host.listens(HookPoint::AfterToolCall) {
        config.after_tool_call = Some(compose_after(
            config.after_tool_call.take(),
            after_hook(host.clone()),
        ));
    }
}

/// `system_prompt` with `:dirge/system-prompt` contributions appended.
pub fn with_system_prompt(
    host: &AddonHost,
    system_prompt: String,
    session_id: Option<&str>,
) -> String {
    let ctx = json!({ "cwd": cwd(), "session-id": session_id });
    append(
        system_prompt,
        host.texts(HookPoint::SystemPrompt, &ctx),
        "\n\n",
    )
}

/// `:dirge/on-prompt` contributions for `prompt`, as reminders.
pub fn prompt_reminders(
    host: &AddonHost,
    prompt: &str,
    session_id: Option<&str>,
    first_prompt: bool,
) -> Vec<String> {
    let ctx = json!({ "prompt": prompt, "session-id": session_id, "first-prompt?": first_prompt });
    host.texts(HookPoint::OnPrompt, &ctx)
        .iter()
        .map(|t| policy::reminder(HookPoint::OnPrompt, t))
        .collect()
}

/// Longest `:dirge/system-prompt` and `:dirge/on-prompt` may take, together,
/// before a run opens without them.
pub const PROMPT_HOOKS_BUDGET: Duration = Duration::from_secs(30);

/// The step that runs `:dirge/system-prompt` and `:dirge/on-prompt` as a run
/// of `session_id` opens, on a blocking thread of the agent runtime, where
/// addon code may reach MCP. `None` when no addon listens on either.
pub fn open_run(
    host: Arc<AddonHost>,
    session_id: Option<String>,
    first_prompt: bool,
) -> Option<OpenRunFn> {
    if !host.listens(HookPoint::SystemPrompt) && !host.listens(HookPoint::OnPrompt) {
        return None;
    }
    Some(Arc::new(move |opening: RunOpening| {
        let (host, session_id) = (host.clone(), session_id.clone());
        Box::pin(async move {
            let unchanged = opening.clone();
            let amended = blocking_within(PROMPT_HOOKS_BUDGET, move || {
                amend_opening(&host, opening, session_id.as_deref(), first_prompt)
            })
            .await;
            amended.unwrap_or_else(|why| {
                tracing::warn!(target: "dirge::addon", %why, "addon prompt hooks skipped");
                unchanged
            })
        })
    }))
}

/// `opening` with the system-prompt texts appended and the on-prompt
/// reminders added.
fn amend_opening(
    host: &AddonHost,
    mut opening: RunOpening,
    session_id: Option<&str>,
    first_prompt: bool,
) -> RunOpening {
    opening.system_prompt = with_system_prompt(host, opening.system_prompt, session_id);
    let reminders = prompt_reminders(host, &opening.prompt, session_id, first_prompt);
    opening.reminders.extend(reminders);
    opening
}

fn append(base: String, texts: Vec<String>, sep: &str) -> String {
    if texts.is_empty() {
        base
    } else {
        format!("{base}{sep}{}", texts.join(sep))
    }
}

fn cwd() -> Value {
    std::env::current_dir()
        .map(|p| Value::String(p.display().to_string()))
        .unwrap_or(Value::Null)
}

/// The process-wide addon host as the agent's [`AddonHooks`], looked up on
/// every call: `/addons reload` may start it after boot.
pub struct LiveAddonHooks;

impl AddonHooks for LiveAddonHooks {
    fn loop_tools(
        &self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Vec<Arc<dyn LoopTool>> {
        super::global()
            .map(|host| {
                super::tool::loop_tools(&host, permission, ask_tx)
                    .into_iter()
                    .map(|tool| Arc::new(tool) as Arc<dyn LoopTool>)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn install_tool_hooks(&self, config: &mut LoopConfig) {
        if let Some(host) = super::global() {
            install(config, &host);
        }
    }

    fn open_run(&self, session_id: Option<String>, first_prompt: bool) -> Option<OpenRunFn> {
        super::global().and_then(|host| open_run(host, session_id, first_prompt))
    }

    fn observe(&self, event: &crate::event::AgentEvent) {
        let Some(host) = super::global() else {
            return;
        };
        if host.listens_key(super::events::EVENT_KEY)
            && let Some(ctx) = super::events::project(event)
        {
            host.post(super::events::EVENT_KEY, &ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addons::domain::HookReply;
    use crate::addons::host::tests::{ScriptedRuntime, summary};
    use crate::agent::agent_loop::message::{AssistantMessage, StopReason};

    fn host_with(points: &[HookPoint], answers: Vec<HookReply>) -> Arc<AddonHost> {
        let rt = Arc::new(ScriptedRuntime {
            hook_answers: answers,
            ..Default::default()
        });
        Arc::new(AddonHost::new(
            rt,
            vec![summary("hd", &[], points)],
            Vec::new(),
        ))
    }

    fn reply(v: Value) -> HookReply {
        HookReply {
            addon_id: "hd".into(),
            result: Ok(v),
        }
    }

    fn ctx() -> BeforeToolCallContext {
        BeforeToolCallContext {
            assistant_message: AssistantMessage::new(Vec::new(), StopReason::ToolUse),
            tool_call_name: "bash".into(),
            tool_call_id: "t1".into(),
            args: json!({"command": "ls"}),
        }
    }

    #[test]
    fn a_block_becomes_a_refusal_naming_the_addon() {
        let out = before_return(
            ctx(),
            BeforeOutcome {
                block: Some(("hd".into(), "no rm".into())),
                ..BeforeOutcome::default()
            },
        );
        let result = out.result.expect("blocked");
        assert_eq!(result.block, Some(true));
        assert_eq!(result.reason.as_deref(), Some("blocked by addon hd: no rm"));
        assert_eq!(out.args, json!({"command": "ls"}));
    }

    #[test]
    fn replacement_args_and_context_flow_through() {
        let out = before_return(
            ctx(),
            BeforeOutcome {
                args: Some(json!({"command": "ls -la"})),
                context: vec!["mind the cwd".into()],
                ..BeforeOutcome::default()
            },
        );
        assert!(out.result.is_none());
        assert_eq!(out.args, json!({"command": "ls -la"}));
        assert!(out.context[0].contains("mind the cwd"));
    }

    #[test]
    fn after_context_is_appended_not_replacing() {
        let result = LoopToolResult {
            content: vec![json!({"type": "text", "text": "out"})],
            details: Value::Null,
            terminate: None,
        };
        assert!(after_override(&result, &[]).is_none());
        let over = after_override(&result, &["noted".into()]).unwrap();
        let content = over.content.unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0], json!({"type": "text", "text": "out"}));
        assert!(content[1]["text"].as_str().unwrap().contains("noted"));
    }

    #[test]
    fn system_prompt_gains_addon_text_only_when_listened() {
        let host = host_with(&[HookPoint::SystemPrompt], vec![reply(json!("addon text"))]);
        assert_eq!(
            with_system_prompt(&host, "base".into(), None),
            "base\n\naddon text"
        );
        let deaf = host_with(&[], vec![reply(json!("never"))]);
        assert_eq!(with_system_prompt(&deaf, "base".into(), None), "base");
    }

    #[test]
    fn prompt_context_becomes_a_reminder() {
        let host = host_with(
            &[HookPoint::OnPrompt],
            vec![reply(json!({"context": "3 lings running"}))],
        );
        let notes = prompt_reminders(&host, "do it", Some("s1"), true);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("<system-reminder>"));
        assert!(notes[0].contains("3 lings running"));
    }

    #[tokio::test]
    async fn the_open_run_step_amends_the_opening() {
        let host = host_with(
            &[HookPoint::SystemPrompt, HookPoint::OnPrompt],
            vec![reply(json!("addon text"))],
        );
        let open = open_run(host, Some("s1".into()), true).expect("listened");
        let opening = open(RunOpening {
            system_prompt: "base".into(),
            prompt: "do it".into(),
            reminders: Vec::new(),
            refusal: None,
        })
        .await;
        assert_eq!(opening.system_prompt, "base\n\naddon text");
        assert_eq!(opening.prompt, "do it");
        assert_eq!(opening.reminders.len(), 1);
        assert!(opening.reminders[0].contains("addon text"));
    }

    #[test]
    fn no_step_when_no_addon_listens_on_the_prompt() {
        let deaf = host_with(&[HookPoint::BeforeToolCall], vec![reply(json!("never"))]);
        assert!(open_run(deaf, None, true).is_none());
    }
}
