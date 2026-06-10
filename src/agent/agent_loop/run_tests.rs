use super::*;
use crate::agent::agent_loop::hooks::{
    AfterToolCallContext, AfterToolCallFn, GetSteeringMessagesFn, PrepareNextTurnFn,
    ShouldStopAfterTurnFn,
};
use crate::agent::agent_loop::message::{StreamEvent, UserMessage};
use crate::agent::agent_loop::result::AfterToolCallResult;
use crate::agent::agent_loop::stream::StreamFn;
use crate::agent::agent_loop::tool::{AbortSignal, LoopTool, LoopToolUpdate};
use crate::agent::agent_loop::types::{ConvertToLlmFn, LoopConfig, ToolExecutionMode, TurnUpdate};
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Build a stream factory that returns canned assistant
/// messages in sequence. Mirrors pi's typical test mock —
/// `callIndex` increments per invocation; each call returns
/// the next canned response.
///
/// `responses` is a Vec; index N is returned on the (N+1)th
/// call. Past the end → final fallback message with
/// stopReason=Stop.
fn canned_factory(responses: Vec<AssistantMessage>) -> StreamFn {
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let responses = std::sync::Arc::new(responses);
    std::sync::Arc::new(move |_ctx, _opts| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let msg = responses.get(n).cloned().unwrap_or_else(|| {
            AssistantMessage::new(
                vec![ContentBlock::Text {
                    text: "end".to_string(),
                }],
                StopReason::Stop,
            )
        });
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason,
            message: msg,
            usage: None,
        }]))
    })
}

fn identity_converter() -> ConvertToLlmFn {
    std::sync::Arc::new(|messages: &[Value]| {
        messages
            .iter()
            .filter(|m| {
                let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
                matches!(role, "user" | "assistant" | "tool" | "toolResult")
            })
            .cloned()
            .collect()
    })
}

fn build_config() -> LoopConfig {
    LoopConfig {
        convert_to_llm: identity_converter(),
        transform_context: None,
        compaction_hooks: None,
        get_api_key: None,
        api_key: None,
        tool_execution: ToolExecutionMode::Sequential,
        before_tool_call: None,
        after_tool_call: None,
        prepare_next_turn: None,
        should_stop_after_turn: None,
        get_steering_messages: None,
        get_followup_messages: None,
        reasoning: None,
        thinking_budgets: None,
        headers: std::collections::HashMap::new(),
        metadata: std::collections::HashMap::new(),
        request_timeout: None,
        provider_name: None,
        model_name: None,
        compact_model: None,
        storm_mutating_tools: None,
        storm_exempt_tools: None,
        repair_stats: std::sync::Arc::new(
            crate::agent::agent_loop::tool_input_repair::RepairStats::new(),
        ),
        truncation_notes: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        tool_def_filter: None,
        dynamic_tool_search: false,
        escalation_stream_fn: None,
        escalation_provider_name: None,
        escalation_pending: std::sync::Arc::new(std::sync::Mutex::new(None)),
        escalation_max_per_session: 3,
        escalation_remaining: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(3)),
        file_touch_tracker: None,
        verifier: None,
        critic_fn: None,
        max_turns: None,
    }
}

fn empty_context() -> Context {
    Context {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
    }
}

/// LOOP-9 integration: `run_compaction_pass` end-to-end. Feed
/// a long conversation, a mock summarizer, and assert that
/// (a) the older messages were dropped, (b) a SUMMARY_PREFIX
/// system message was inserted at the head, (c) the latest
/// user message is still in the tail, and (d) a
/// `ContextCompacted` event was emitted with a rotated session id.
#[tokio::test]
async fn run_compaction_pass_inserts_summary_and_rotates_session() {
    let mut ctx = empty_context();
    ctx.system_prompt = "you are an agent".into();
    // Pad with 25 turns so the compaction window has material.
    ctx.messages.push(serde_json::json!({
        "role": "system", "content": "you are an agent"
    }));
    ctx.messages.push(serde_json::json!({
        "role": "user", "content": "initial task: fix the bug"
    }));
    for i in 0..20 {
        let role = if i % 2 == 0 { "assistant" } else { "user" };
        ctx.messages.push(serde_json::json!({
            "role": role,
            "content": format!("turn {i} with some content to fill bytes"),
        }));
    }
    ctx.messages.push(serde_json::json!({
        "role": "user", "content": "latest user request"
    }));
    let n_before = ctx.messages.len();

    // Mock summarizer: returns a valid Hermes-style summary
    // structure. We assert the prompt was built (non-empty).
    let prompt_seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let prompt_seen_inner = prompt_seen.clone();
    let summarize_fn: Option<crate::agent::compression::SummarizeFn> =
        Some(std::sync::Arc::new(move |prompt: String| {
            let store = prompt_seen_inner.clone();
            Box::pin(async move {
                *store.lock().unwrap() = prompt;
                Ok("## Active Task\nfix the bug\n\n\
                        ## Goal\nresolve the issue\n\n\
                        ## Completed Actions\n1. read the file\n\n\
                        ## Remaining Work\nrun tests"
                    .to_string())
            })
        }));

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(8);
    super::run_compaction_pass(&mut ctx, &summarize_fn, 5, 0, &None, None, &tx).await;
    drop(tx);

    // (a) older messages dropped.
    assert!(
        ctx.messages.len() < n_before,
        "expected compaction to shrink the message list: before={n_before} after={}",
        ctx.messages.len()
    );

    // (b) summary system message with SUMMARY_PREFIX is present.
    let summary_msg = ctx
        .messages
        .iter()
        .find(|m| {
            m.get("role").and_then(|v| v.as_str()) == Some("system")
                && m.get("content")
                    .and_then(|v| v.as_str())
                    .map(|s| s.contains("CONTEXT COMPACTION"))
                    .unwrap_or(false)
        })
        .expect("compaction summary message should be present");
    let body = summary_msg["content"].as_str().unwrap();
    assert!(body.contains("## Active Task"));
    assert!(body.contains("fix the bug"));

    // (c) latest user message preserved.
    let last = ctx.messages.last().unwrap();
    assert_eq!(last["content"].as_str().unwrap(), "latest user request");

    // (d) ContextCompacted event emitted with rotated session id.
    let mut compacted_event_seen = false;
    while let Some(ev) = rx.recv().await {
        if let LoopEvent::ContextCompacted { new_session_id, .. } = ev {
            assert!(
                new_session_id.starts_with("compacted-"),
                "session id should rotate via compacted- prefix; got {new_session_id}"
            );
            compacted_event_seen = true;
        }
    }
    assert!(compacted_event_seen, "expected ContextCompacted event");

    // Sanity: the summarizer received a Hermes structured prompt
    // (built via build_summary_prompt).
    let received = prompt_seen.lock().unwrap().clone();
    assert!(received.contains("TURNS TO SUMMARIZE"));
    assert!(received.contains("## Active Task"));
}

/// dirge-jia8: a plugin `on-compact` hook supplying a valid summary
/// is used INSTEAD of the LLM summarizer; the observe-only
/// `on-before-compact` hook fires. Built from plain closures (no
/// Janet needed) so it runs on the default feature set.
#[tokio::test]
async fn compaction_on_compact_hook_overrides_llm_summary() {
    use crate::agent::agent_loop::types::CompactionHooks;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut ctx = empty_context();
    ctx.messages
        .push(serde_json::json!({"role": "system", "content": "sys"}));
    ctx.messages
        .push(serde_json::json!({"role": "user", "content": "initial"}));
    for i in 0..20 {
        let role = if i % 2 == 0 { "assistant" } else { "user" };
        ctx.messages
            .push(serde_json::json!({"role": role, "content": format!("turn {i} content")}));
    }
    ctx.messages
        .push(serde_json::json!({"role": "user", "content": "latest"}));

    // LLM summarizer returns a DISTINCT summary — if the plugin
    // override works, this text must NOT appear.
    let llm_called = std::sync::Arc::new(AtomicUsize::new(0));
    let llm_called_c = llm_called.clone();
    let summarize_fn: Option<crate::agent::compression::SummarizeFn> =
        Some(std::sync::Arc::new(move |_prompt: String| {
            llm_called_c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok("## Active Task\nLLM-SUMMARY".to_string()) })
        }));

    // on-before observe counter + on-compact returning a custom summary.
    let before_fired = std::sync::Arc::new(AtomicUsize::new(0));
    let before_c = before_fired.clone();
    let hooks = CompactionHooks {
        on_before: std::sync::Arc::new(move |_count, _tokens| {
            let f = before_c.clone();
            Box::pin(async move {
                f.fetch_add(1, Ordering::SeqCst);
            })
        }),
        on_compact: std::sync::Arc::new(move |_middle| {
            Box::pin(async move { Some("## Active Task\nPLUGIN-SUMMARY".to_string()) })
        }),
    };

    let (tx, _rx) = mpsc::channel::<LoopEvent>(8);
    super::run_compaction_pass(&mut ctx, &summarize_fn, 5, 0, &None, Some(&hooks), &tx).await;
    drop(tx);

    // on-before-compact observed the fold.
    assert_eq!(
        before_fired.load(Ordering::SeqCst),
        1,
        "on-before-compact must fire"
    );
    // The plugin summary was applied, not the LLM's.
    let summary_msg = ctx
        .messages
        .iter()
        .find(|m| {
            m.get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.contains("PLUGIN-SUMMARY"))
                .unwrap_or(false)
        })
        .expect("plugin summary must be in the compacted context");
    assert!(
        summary_msg["content"]
            .as_str()
            .unwrap()
            .contains("PLUGIN-SUMMARY")
    );
    assert!(
        !ctx.messages.iter().any(|m| m
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("LLM-SUMMARY"))
            .unwrap_or(false)),
        "LLM summary must NOT appear — plugin override should win",
    );
    assert_eq!(
        llm_called.load(Ordering::SeqCst),
        0,
        "LLM summarizer must NOT be called when the plugin supplies a valid summary",
    );
}

/// dirge-jia8: an `on-compact` hook returning an INVALID summary
/// (fails validate_summary) falls through to the LLM summarizer —
/// the plugin can't inject garbage as the summary.
#[tokio::test]
async fn compaction_invalid_plugin_summary_falls_through_to_llm() {
    use crate::agent::agent_loop::types::CompactionHooks;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut ctx = empty_context();
    ctx.messages
        .push(serde_json::json!({"role": "system", "content": "sys"}));
    ctx.messages
        .push(serde_json::json!({"role": "user", "content": "initial"}));
    for i in 0..20 {
        let role = if i % 2 == 0 { "assistant" } else { "user" };
        ctx.messages
            .push(serde_json::json!({"role": role, "content": format!("turn {i} content")}));
    }
    ctx.messages
        .push(serde_json::json!({"role": "user", "content": "latest"}));

    let llm_called = std::sync::Arc::new(AtomicUsize::new(0));
    let llm_called_c = llm_called.clone();
    let summarize_fn: Option<crate::agent::compression::SummarizeFn> =
        Some(std::sync::Arc::new(move |_prompt: String| {
            llm_called_c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok("## Active Task\nLLM-SUMMARY".to_string()) })
        }));

    let hooks = CompactionHooks {
        on_before: std::sync::Arc::new(|_c, _t| Box::pin(async {})),
        // Invalid: no required section header → validate_summary fails.
        on_compact: std::sync::Arc::new(move |_middle| {
            Box::pin(async move { Some("garbage with no section header".to_string()) })
        }),
    };

    let (tx, _rx) = mpsc::channel::<LoopEvent>(8);
    super::run_compaction_pass(&mut ctx, &summarize_fn, 5, 0, &None, Some(&hooks), &tx).await;
    drop(tx);

    assert_eq!(
        llm_called.load(Ordering::SeqCst),
        1,
        "invalid plugin summary must fall through to the LLM summarizer",
    );
    assert!(
        ctx.messages.iter().any(|m| m
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("LLM-SUMMARY"))
            .unwrap_or(false)),
        "LLM summary should be applied after the invalid plugin summary",
    );
}

/// LOOP-9: when no summarizer is wired, the compaction pass
/// still runs the cheap pruning and emits ContextCompacted, but
/// does NOT insert a structured summary system message.
#[tokio::test]
async fn run_compaction_pass_without_summarizer_prunes_only() {
    let mut ctx = empty_context();
    // One large tool result that should be pruned.
    ctx.messages.push(serde_json::json!({
        "role": "user", "content": "first"
    }));
    ctx.messages.push(serde_json::json!({
        "role": "toolResult", "content": "x".repeat(2000), "toolName": "bash"
    }));
    ctx.messages.push(serde_json::json!({
        "role": "user", "content": "tail"
    }));
    ctx.messages.push(serde_json::json!({
        "role": "assistant", "content": "tail asst"
    }));

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(4);
    // Use protect_tail = 2 so the large tool result is eligible
    // for pruning (it's at index 1, end = 4 - 2 = 2, so index
    // 1 is in-range).
    super::run_compaction_pass(&mut ctx, &None, 2, 0, &None, None, &tx).await;
    drop(tx);

    // No SUMMARY_PREFIX message inserted.
    let has_summary = ctx.messages.iter().any(|m| {
        m.get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("CONTEXT COMPACTION"))
            .unwrap_or(false)
    });
    assert!(
        !has_summary,
        "no summary should be inserted without summarize_fn"
    );

    // The large tool result was pruned (replaced with a [bash] marker).
    let tool_msg = &ctx.messages[1];
    assert!(tool_msg["content"].as_str().unwrap().contains("[bash]"));

    // ContextCompacted still emitted.
    let mut compacted_event_seen = false;
    while let Some(ev) = rx.recv().await {
        if matches!(ev, LoopEvent::ContextCompacted { .. }) {
            compacted_event_seen = true;
        }
    }
    assert!(compacted_event_seen);
}

/// Mock echo tool for run-loop tests. Records executed args
/// per call so test setups can detect terminate-flag flow.
#[derive(Debug)]
struct EchoTool {
    terminate: bool,
    executed: std::sync::Arc<Mutex<Vec<Value>>>,
}
impl EchoTool {
    fn new() -> Self {
        Self {
            terminate: false,
            executed: std::sync::Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn with_terminate(mut self) -> Self {
        self.terminate = true;
        self
    }
}
impl LoopTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo tool"
    }
    fn label(&self) -> &str {
        "Echo"
    }
    fn parameters(&self) -> &Value {
        static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
    }
    fn execute<'a>(
        &'a self,
        _id: &'a str,
        args: Value,
        _signal: AbortSignal,
        _on_update: LoopToolUpdate,
    ) -> Pin<Box<dyn Future<Output = Result<super::super::LoopToolResult, String>> + Send + 'a>>
    {
        let executed = self.executed.clone();
        let terminate = self.terminate;
        Box::pin(async move {
            executed.lock().unwrap().push(args.clone());
            Ok(super::super::LoopToolResult {
                content: vec![serde_json::json!({"type": "text", "text": "ok"})],
                details: args,
                terminate: if terminate { Some(true) } else { None },
            })
        })
    }
}

fn user(text: &str) -> LoopMessage {
    LoopMessage::User(UserMessage {
        content: text.to_string(),
    })
}

fn text_response(text: &str) -> AssistantMessage {
    AssistantMessage::new(
        vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        StopReason::Stop,
    )
}

fn tool_use_response(id: &str, name: &str, args: Value) -> AssistantMessage {
    AssistantMessage::new(
        vec![ContentBlock::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
        }],
        StopReason::ToolUse,
    )
}

/// Drain channel into a Vec.
async fn drain(rx: &mut mpsc::Receiver<LoopEvent>) -> Vec<LoopEvent> {
    let mut out = Vec::new();
    while let Some(e) = rx.recv().await {
        out.push(e);
    }
    out
}

/// Port of pi test "should emit events with AgentMessage types"
/// (agent-loop.test.ts:84). Full agent loop run — assistant
/// response, no tools.
#[tokio::test]
async fn test_emits_full_agent_loop_event_sequence() {
    let factory = canned_factory(vec![text_response("Hi there!")]);
    let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
    let messages = run_agent_loop(
        vec![user("Hello")],
        empty_context(),
        build_config(),
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    let kinds: Vec<_> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
    // Must contain all pi-required events.
    for required in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(kinds.contains(&required), "missing {required}: {kinds:?}");
    }
    // Return value: user + assistant message.
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role(), "user");
    assert_eq!(messages[1].role(), "assistant");
}

/// Port of pi test "should handle tool calls and results"
/// (agent-loop.test.ts:239). Full-loop scope: assistant emits
/// tool call → loop dispatches → next assistant emits final
/// text.
#[tokio::test]
async fn test_full_loop_with_tool_then_final_text() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo.clone());

    let factory = canned_factory(vec![
        tool_use_response("call-1", "echo", serde_json::json!({"v": 1})),
        text_response("done"),
    ]);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
    let messages = run_agent_loop(
        vec![user("echo")],
        ctx,
        build_config(),
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    // Tool actually executed.
    assert_eq!(echo.executed.lock().unwrap().len(), 1);

    // Roles: user, assistant (tool use), toolResult, assistant (text).
    let roles: Vec<_> = messages.iter().map(|m| m.role()).collect();
    assert_eq!(roles, vec!["user", "assistant", "toolResult", "assistant"]);

    // Stream of events should contain tool_execution_start +
    // tool_execution_end.
    let kinds: Vec<_> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
    assert!(kinds.contains(&"tool_execution_start"));
    assert!(kinds.contains(&"tool_execution_end"));
}

/// Port of pi test "should use prepareNextTurn snapshot before
/// continuing" (agent-loop.test.ts:897). The hook returns a
/// snapshot mutating `context`; subsequent turn observes the
/// mutation.
#[tokio::test]
async fn test_prepare_next_turn_snapshot_applied() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.system_prompt = "first prompt".to_string();
    ctx.tools.push(echo.clone());

    // Track the system_prompt seen at each LLM call.
    let observed_prompts = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_clone = observed_prompts.clone();
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let factory: StreamFn = std::sync::Arc::new(move |llm_ctx, _opts| {
        observed_clone.lock().unwrap().push(llm_ctx.system_prompt);
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let msg = if n == 0 {
            tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
        } else {
            text_response("done")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason,
            message: msg,
            usage: None,
        }]))
    });

    // Hook fires once: returns a new context with a different
    // system prompt.
    let fired = std::sync::Arc::new(AtomicUsize::new(0));
    let fired_clone = fired.clone();
    let hook: PrepareNextTurnFn = std::sync::Arc::new(move |ctx| {
        let fired = fired_clone.clone();
        Box::pin(async move {
            if fired.fetch_add(1, Ordering::SeqCst) > 0 {
                return None; // only on the first invocation
            }
            Some(TurnUpdate {
                context: Some(Context {
                    system_prompt: "second prompt".to_string(),
                    messages: ctx.context.messages.clone(),
                    tools: ctx.context.tools.clone(),
                }),
                ..Default::default()
            })
        })
    });

    let mut config = build_config();
    config.prepare_next_turn = Some(hook);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let _ = run_agent_loop(
        vec![user("echo something")],
        ctx,
        config,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;

    let observed = observed_prompts.lock().unwrap().clone();
    assert_eq!(observed.len(), 2, "expected 2 LLM calls");
    assert_eq!(observed[0], "first prompt");
    assert_eq!(
        observed[1], "second prompt",
        "second LLM call should see the mutated context"
    );
}

/// dirge-6js7 plugin review: prepareNextTurn returning a new
/// thinking_level must actually be APPLIED to the next turn's
/// stream call (config.reasoning), not dropped with a warning.
/// This is the fix for the HIGH "looks present but doesn't fire"
/// finding — the plugin `harness/set-next-thinking-level` slot
/// flows through prepare_next_turn into the live loop.
#[tokio::test]
async fn prepare_next_turn_applies_thinking_level_to_next_turn() {
    use crate::agent::agent_loop::types::ThinkingLevel;

    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo.clone());

    // Record the `reasoning` (thinking level) seen at each LLM call.
    let observed_reasoning = std::sync::Arc::new(Mutex::new(Vec::<Option<ThinkingLevel>>::new()));
    let observed_clone = observed_reasoning.clone();
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let factory: StreamFn = std::sync::Arc::new(move |_llm_ctx, opts| {
        observed_clone.lock().unwrap().push(opts.reasoning);
        let n = counter.fetch_add(1, Ordering::SeqCst);
        // Turn 1 calls a tool (loop continues); turn 2 finishes.
        let msg = if n == 0 {
            tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
        } else {
            text_response("done")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason,
            message: msg,
            usage: None,
        }]))
    });

    // Hook fires after turn 1 and requests a thinking-level swap.
    let fired = std::sync::Arc::new(AtomicUsize::new(0));
    let fired_clone = fired.clone();
    let hook: PrepareNextTurnFn = std::sync::Arc::new(move |_ctx| {
        let fired = fired_clone.clone();
        Box::pin(async move {
            if fired.fetch_add(1, Ordering::SeqCst) > 0 {
                return None;
            }
            Some(TurnUpdate {
                thinking_level: Some(ThinkingLevel::High),
                ..Default::default()
            })
        })
    });

    let mut config = build_config();
    config.prepare_next_turn = Some(hook);
    // Start with no reasoning set so the swap is observable.
    config.reasoning = None;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let _ = run_agent_loop(
        vec![user("go")],
        ctx,
        config,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None,
    )
    .await;

    let observed = observed_reasoning.lock().unwrap().clone();
    assert_eq!(observed.len(), 2, "expected 2 LLM calls");
    assert_eq!(
        observed[0], None,
        "turn 1 runs with the initial reasoning (none)"
    );
    assert_eq!(
        observed[1],
        Some(ThinkingLevel::High),
        "turn 2 must see the thinking_level prepareNextTurn requested — \
         pre-fix this was dropped and turn 2 saw None",
    );
}

/// Port of pi test "should stop after the current turn when
/// shouldStopAfterTurn returns true" (agent-loop.test.ts:970).
#[tokio::test]
async fn test_should_stop_after_turn_stops_loop() {
    let factory = canned_factory(vec![
        text_response("turn one"),
        // Second response should NEVER be requested — hook
        // stops the loop after turn one.
        text_response("should not appear"),
    ]);

    let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
    let llm_calls_clone = llm_calls.clone();
    // Wrap factory to count invocations.
    let factory_counted: StreamFn = std::sync::Arc::new(move |ctx, opts| {
        llm_calls_clone.fetch_add(1, Ordering::SeqCst);
        factory(ctx, opts)
    });

    let hook: ShouldStopAfterTurnFn = std::sync::Arc::new(|_ctx| Box::pin(async move { true }));

    let mut config = build_config();
    config.should_stop_after_turn = Some(hook);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
    let messages = run_agent_loop(
        vec![user("hi")],
        empty_context(),
        config,
        AbortSignal::new(),
        &tx,
        &factory_counted,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    // Only one LLM call.
    assert_eq!(llm_calls.load(Ordering::SeqCst), 1);
    // Messages: user + one assistant.
    assert_eq!(messages.len(), 2);
    // Loop emitted agent_end.
    let kinds: Vec<_> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
    assert!(kinds.contains(&"agent_end"));
}

/// Port of pi test "should stop after a tool batch when every
/// tool result sets terminate=true" (agent-loop.test.ts:1067).
/// LOOP-LEVEL: only one LLM call (the tool dispatch terminates).
#[tokio::test]
async fn test_terminate_stops_loop_after_tool_batch() {
    let echo = std::sync::Arc::new(EchoTool::new().with_terminate());
    let mut ctx = empty_context();
    ctx.tools.push(echo);

    let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
    let llm_calls_clone = llm_calls.clone();
    let factory: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
        llm_calls_clone.fetch_add(1, Ordering::SeqCst);
        let msg = tool_use_response("call-1", "echo", serde_json::json!({"v": 1}));
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: msg,
            usage: None,
        }]))
    });

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let messages = run_agent_loop(
        vec![user("echo")],
        ctx,
        build_config(),
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;

    assert_eq!(llm_calls.load(Ordering::SeqCst), 1, "no second LLM call");
    // user + assistant(tool use) + toolResult — no second
    // assistant text turn.
    let roles: Vec<_> = messages.iter().map(|m| m.role()).collect();
    assert_eq!(roles, vec!["user", "assistant", "toolResult"]);
}

/// Port of pi test "should allow afterToolCall to mark a tool
/// batch as terminating" (agent-loop.test.ts:1184). LOOP-LEVEL.
#[tokio::test]
async fn test_after_tool_call_terminate_stops_loop() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo);

    let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
    let llm_calls_clone = llm_calls.clone();
    let factory: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
        llm_calls_clone.fetch_add(1, Ordering::SeqCst);
        let msg = tool_use_response("call-1", "echo", serde_json::json!({"v": 1}));
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: msg,
            usage: None,
        }]))
    });

    let after: AfterToolCallFn = std::sync::Arc::new(|_ctx: AfterToolCallContext| {
        Box::pin(async move {
            Some(AfterToolCallResult {
                content: None,
                details: None,
                is_error: None,
                terminate: Some(true),
            })
        })
    });
    let mut config = build_config();
    config.after_tool_call = Some(after);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let _ = run_agent_loop(
        vec![user("echo")],
        ctx,
        config,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;

    assert_eq!(llm_calls.load(Ordering::SeqCst), 1, "no second LLM call");
}

/// Port of pi test "should continue after parallel tool calls
/// when not all tool results terminate" (agent-loop.test.ts:1119).
/// LOOP-LEVEL: two LLM calls.
#[tokio::test]
async fn test_continue_when_not_all_terminate() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo);

    let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
    let llm_calls_clone = llm_calls.clone();
    let factory: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
        let n = llm_calls_clone.fetch_add(1, Ordering::SeqCst);
        let msg = if n == 0 {
            tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
        } else {
            text_response("done")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason,
            message: msg,
            usage: None,
        }]))
    });

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let _ = run_agent_loop(
        vec![user("echo")],
        ctx,
        build_config(),
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;

    assert_eq!(
        llm_calls.load(Ordering::SeqCst),
        2,
        "two LLM calls expected"
    );
}

/// Port of pi test "should inject queued messages after all
/// tool calls complete" (agent-loop.test.ts:547).
///
/// Setup: assistant emits a tool call. After tool dispatch
/// the loop polls `getSteeringMessages` which returns a user
/// message ONCE. That message is injected before the next
/// assistant call; the second LLM call sees it in its context.
#[tokio::test]
async fn test_steering_messages_injected_after_tool_calls() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo);

    // Steering hook delivers once on the SECOND call (so
    // not on initial poll).
    let poll_count = std::sync::Arc::new(AtomicUsize::new(0));
    let poll_clone = poll_count.clone();
    let steering: GetSteeringMessagesFn = std::sync::Arc::new(move || {
        let poll = poll_clone.clone();
        Box::pin(async move {
            let n = poll.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                vec![user("interrupt")]
            } else {
                Vec::new()
            }
        })
    });

    // Inspector: record what each LLM call sees in its
    // converted message list.
    let saw_interrupt_on_second = std::sync::Arc::new(std::sync::Mutex::new(false));
    let saw_clone = saw_interrupt_on_second.clone();
    let call_counter = std::sync::Arc::new(AtomicUsize::new(0));

    let factory: StreamFn = std::sync::Arc::new(move |llm_ctx, _opts| {
        let n = call_counter.fetch_add(1, Ordering::SeqCst);
        if n == 1 {
            // Second call: check for "interrupt" in messages.
            let found = llm_ctx.messages.iter().any(|m| {
                m.get("role").and_then(|r| r.as_str()) == Some("user")
                    && m.get("content")
                        .and_then(|c| c.as_str())
                        .map(|s| s.contains("interrupt"))
                        == Some(true)
            });
            *saw_clone.lock().unwrap() = found;
        }
        let msg = if n == 0 {
            tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
        } else {
            text_response("done")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason,
            message: msg,
            usage: None,
        }]))
    });

    let mut config = build_config();
    config.get_steering_messages = Some(steering);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
    let messages = run_agent_loop(
        vec![user("start")],
        ctx,
        config,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    assert!(
        *saw_interrupt_on_second.lock().unwrap(),
        "second LLM call should see the injected interrupt"
    );

    // Returned messages include the injected interrupt.
    let user_contents: Vec<String> = messages
        .iter()
        .filter_map(|m| match m {
            LoopMessage::User(u) => Some(u.content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(user_contents, vec!["start", "interrupt"]);

    // The interrupt's message_start fires AFTER the tool
    // result's message_end. We verify by event ordering.
    let events = drain(&mut rx).await;
    let interrupt_idx = events.iter().position(|e| match e {
        LoopEvent::MessageStart {
            message: LoopMessage::User(u),
        } => u.content == "interrupt",
        _ => false,
    });
    let last_tool_result_end_idx = events.iter().rposition(|e| {
        matches!(
            e,
            LoopEvent::MessageEnd {
                message: LoopMessage::ToolResult(_)
            }
        )
    });
    assert!(
        interrupt_idx.unwrap() > last_tool_result_end_idx.unwrap(),
        "interrupt should appear AFTER the tool result message_end"
    );
}

// ============================================================
// Phase 6 — regression tests for hardening paths
// ============================================================

use crate::agent::agent_loop::result::LoopToolResult as PhaseSixToolResult;
use std::sync::Arc as PhaseSixArc;

/// Phase 6: a multi-turn run with a network error in turn 2
/// preserves the FULL history (user prompt, turn 1's
/// assistant + tool-result) across the retry. The retry
/// wrapper isn't directly invoked here (we use mock
/// StreamFn), but the LOOP's context.messages survival
/// across turn errors is the invariant.
///
/// We verify by counting context.messages entries the
/// second LLM call observes. The mock StreamFn captures
/// what each call sees.
#[tokio::test]
async fn loop_preserves_history_across_turns() {
    use crate::agent::agent_loop::stream::{LlmContext, StreamFn};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let observed_lens: PhaseSixArc<Mutex<Vec<usize>>> = PhaseSixArc::new(Mutex::new(Vec::new()));
    let observed_clone = observed_lens.clone();
    let counter = std::sync::Arc::new(AtomicUsize::new(0));

    // Inline echo tool — needed for the tool-result turn
    // that grows the history.
    #[derive(Debug)]
    struct LocalEcho;
    impl LoopTool for LocalEcho {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo"
        }
        fn label(&self) -> &str {
            "Echo"
        }
        fn parameters(&self) -> &Value {
            static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            _args: Value,
            _signal: AbortSignal,
            _on_update: super::super::tool::LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<PhaseSixToolResult, String>> + Send + 'a>> {
            Box::pin(async move {
                Ok(PhaseSixToolResult {
                    content: vec![serde_json::json!({
                        "type": "text",
                        "text": "ok",
                    })],
                    details: Value::Null,
                    terminate: None,
                })
            })
        }
    }

    let factory: StreamFn = std::sync::Arc::new(move |ctx: LlmContext, _opts| {
        observed_clone.lock().unwrap().push(ctx.messages.len());
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let msg = if n == 0 {
            tool_use_response("call-1", "echo", serde_json::json!({}))
        } else {
            text_response("done")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![
            crate::agent::agent_loop::message::StreamEvent::Done {
                reason,
                message: msg,
                usage: None,
            },
        ]))
    });

    let mut ctx = empty_context();
    ctx.tools.push(PhaseSixArc::new(LocalEcho));
    let mut cfg = build_config();
    cfg.tool_execution = ToolExecutionMode::Sequential;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let _ = run_agent_loop(
        vec![user("start")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;

    let lens = observed_lens.lock().unwrap().clone();
    assert_eq!(lens.len(), 2, "expected two LLM calls");
    // First call sees: just user prompt → 1 message.
    assert_eq!(lens[0], 1);
    // Second call sees: user prompt + assistant (tool_use) +
    // tool result → 3 messages. History preserved.
    assert_eq!(
        lens[1], 3,
        "second LLM call should see prior turn's history; got {} messages",
        lens[1],
    );
}

/// Phase 6: full signal-chain regression. Cancel the signal
/// mid-tool; tool aborts; loop's next LLM call's stream
/// observes the same signal and exits via Error path; loop
/// exits cleanly with no infinite-loop or hung tools.
#[tokio::test]
async fn full_signal_chain_exits_cleanly() {
    use crate::agent::agent_loop::stream::{LlmContext, StreamFn};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Mock tool that observes the signal during execution
    // (immediate cancel since the test cancels signal right
    // after spawn).
    #[derive(Debug)]
    struct CancellableTool;
    impl LoopTool for CancellableTool {
        fn name(&self) -> &str {
            "noop"
        }
        fn description(&self) -> &str {
            "Cancellable"
        }
        fn label(&self) -> &str {
            "Noop"
        }
        fn parameters(&self) -> &Value {
            static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            _args: Value,
            _signal: AbortSignal,
            _on_update: super::super::tool::LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<PhaseSixToolResult, String>> + Send + 'a>> {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(PhaseSixToolResult {
                    content: Vec::new(),
                    details: Value::Null,
                    terminate: None,
                })
            })
        }
    }

    // Factory that returns a tool_use response first,
    // then would return a text response on retry (but
    // shouldn't get there because signal is cancelled
    // before turn 2).
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let factory: StreamFn = std::sync::Arc::new(move |_ctx: LlmContext, _opts| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let msg = if n == 0 {
            tool_use_response("call-1", "noop", serde_json::json!({}))
        } else {
            text_response("should-not-reach")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![
            crate::agent::agent_loop::message::StreamEvent::Done {
                reason,
                message: msg,
                usage: None,
            },
        ]))
    });

    let mut ctx = empty_context();
    ctx.tools.push(PhaseSixArc::new(CancellableTool));
    let mut cfg = build_config();
    cfg.tool_execution = ToolExecutionMode::Sequential;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let signal = AbortSignal::new();
    let signal_clone = signal.clone();

    // Spawn the loop in a task; cancel signal after a small
    // yield so the tool has started.
    let task = tokio::spawn(async move {
        run_agent_loop(
            vec![user("start")],
            ctx,
            cfg,
            signal_clone,
            &tx,
            &factory,
            None,
            None, // memory_provider — test default
        )
        .await
    });
    // Yield twice so the loop reaches the tool dispatch
    // before we cancel.
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    signal.cancel();

    // Bound the test: loop must complete in <2s. Without
    // the tool-abort wrap, the 30s blocking tool would
    // exceed this. R3 ensures the next LLM call (if any)
    // also exits promptly via its pre-poll signal check.
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
    assert!(
        result.is_ok(),
        "loop should exit within 2s after signal cancel"
    );
}

// ── dirge-h5tv: build_augmented_focus + transcript helper ──

use crate::extras::memory_provider::MemoryProvider;
use std::sync::Arc;

#[derive(Default)]
struct PreCompressRecorder {
    seen: Mutex<Vec<String>>,
    return_value: Mutex<String>,
}
impl MemoryProvider for PreCompressRecorder {
    fn name(&self) -> &str {
        "pre-compress-recorder"
    }
    fn view(&self, _: &str) -> serde_json::Value {
        serde_json::Value::Null
    }
    fn add(&self, _: &str, _: &str, _kind: Option<&str>) -> Result<serde_json::Value, String> {
        Ok(serde_json::Value::Null)
    }
    fn replace(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _kind: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        Ok(serde_json::Value::Null)
    }
    fn remove(&self, _: &str, _: &str) -> Result<serde_json::Value, String> {
        Ok(serde_json::Value::Null)
    }
    fn on_pre_compress(&self, transcript: &str) -> String {
        self.seen.lock().unwrap().push(transcript.to_string());
        self.return_value.lock().unwrap().clone()
    }
}

fn make_middle() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"role": "user", "content": "what is rust?"}),
        serde_json::json!({"role": "assistant", "content": "a systems language"}),
    ]
}

#[test]
fn build_augmented_focus_returns_none_with_no_inputs() {
    let result = super::build_augmented_focus(None, None, &make_middle());
    assert!(
        result.is_none(),
        "no focus + no provider must yield None instructions"
    );
}

#[test]
fn build_augmented_focus_preserves_focus_when_no_provider() {
    let result = super::build_augmented_focus(Some("error handling"), None, &make_middle());
    assert_eq!(result.as_deref(), Some("error handling"));
}

#[test]
fn build_augmented_focus_folds_provider_insights_into_focus() {
    let provider = Arc::new(PreCompressRecorder::default());
    *provider.return_value.lock().unwrap() = "user prefers async/await over threads".into();
    let provider_dyn: Arc<dyn MemoryProvider> = provider.clone();

    let result =
        super::build_augmented_focus(Some("retry logic"), Some(&provider_dyn), &make_middle());

    let out = result.expect("focus + insights produces Some");
    assert!(out.contains("retry logic"), "user focus must survive");
    assert!(
        out.contains("user prefers async/await over threads"),
        "provider insight must be folded in: {out}"
    );
    assert!(
        out.contains("Provider insights:"),
        "insights must be labelled so the summarizer can attribute them"
    );

    // Provider received the transcript built from the middle slice.
    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "hook fires exactly once");
    assert!(
        seen[0].contains("user: what is rust?")
            && seen[0].contains("assistant: a systems language"),
        "transcript must contain both messages: {:?}",
        seen[0]
    );
}

#[test]
fn build_augmented_focus_yields_insights_alone_when_no_focus() {
    let provider = Arc::new(PreCompressRecorder::default());
    *provider.return_value.lock().unwrap() = "remember the build flags".into();
    let provider_dyn: Arc<dyn MemoryProvider> = provider.clone();

    let result = super::build_augmented_focus(None, Some(&provider_dyn), &make_middle());

    let out = result.expect("insights alone produce Some");
    assert!(out.starts_with("Provider insights:"));
    assert!(out.contains("remember the build flags"));
}

#[test]
fn build_augmented_focus_treats_empty_provider_output_as_none() {
    let provider = Arc::new(PreCompressRecorder::default());
    // Empty string return from on_pre_compress — provider has
    // nothing to contribute this turn.
    *provider.return_value.lock().unwrap() = "".into();
    let provider_dyn: Arc<dyn MemoryProvider> = provider.clone();

    let result = super::build_augmented_focus(None, Some(&provider_dyn), &make_middle());
    assert!(
        result.is_none(),
        "empty provider output + no focus must yield None"
    );

    // But the hook still fired (so it can do internal bookkeeping
    // even if its return is empty).
    assert_eq!(provider.seen.lock().unwrap().len(), 1);
}

#[test]
fn transcript_from_value_slice_renders_role_prefixes() {
    let messages = vec![
        serde_json::json!({"role": "user", "content": "hello"}),
        serde_json::json!({"role": "assistant", "content": "hi"}),
        serde_json::json!({"role": "system", "content": ""}), // empty — skipped
    ];
    let t = super::transcript_from_value_slice(&messages);
    assert!(t.contains("user: hello"));
    assert!(t.contains("assistant: hi"));
    assert!(
        !t.contains("system: "),
        "empty content must be skipped: {t:?}"
    );
}

/// The critic transcript feeds a LOAD-BEARING critic prompt (the F6 in-loop
/// critic just had a stale-summary bug fixed). Pin its exact output byte-for-
/// byte so a refactor can't silently shift the `USER:`/`ASSISTANT:` labels, the
/// `ASSISTANT called name(args)` tool-call rendering, the `TOOL name [tag]: …`
/// result line, or the trimming — none of which the `.contains()` tests catch.
#[test]
fn build_critic_transcript_pins_the_exact_critic_facing_format() {
    use crate::agent::agent_loop::message::ToolResultMessage;
    let msgs = vec![
        user("do the thing"),
        LoopMessage::Assistant(AssistantMessage::new(
            vec![
                ContentBlock::Text {
                    text: "  on it  ".to_string(),
                },
                ContentBlock::ToolCall {
                    id: "c1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({"path": "/x"}),
                },
            ],
            StopReason::Stop,
        )),
        LoopMessage::ToolResult(ToolResultMessage {
            tool_call_id: "c1".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text {
                text: "file contents".to_string(),
            }],
            details: serde_json::json!({}),
            is_error: false,
        }),
    ];
    assert_eq!(
        super::build_critic_transcript(&msgs),
        "USER: do the thing\n\
         ASSISTANT: on it\n\
         ASSISTANT called read({\"path\":\"/x\"})\n\
         TOOL read [result]: file contents\n",
    );
}

// =====================================================================
// dirge-ngic — scavenge must inspect both Thinking AND Text blocks.
// Reasonix combines both at `loop.ts:910-913` →
// `repair/index.ts:71`. Previously dirge merged only Thinking, so
// any DSML invoke that streamed as visible content (the common
// case on Anthropic cache hits) was lost.
// =====================================================================

/// dirge-ngic: a DSML invoke that lives only in `ContentBlock::Text`
/// (no Thinking block at all) must be picked up by the scavenger.
/// Proves the run.rs source builder includes Text — without the
/// fix this orphan call goes unrecovered, the model loop stalls
/// waiting for a tool result that never dispatches.
#[test]
fn scavenge_source_recovers_dsml_invoke_from_text_only() {
    let dsml = "<|DSML|invoke name=\"read_file\"><|DSML|parameter name=\"path\" string=\"true\">/tmp/x</|DSML|parameter></|DSML|invoke>";
    let blocks = vec![ContentBlock::Text {
        text: dsml.to_string(),
    }];

    let source = super::build_scavenge_source(&blocks);
    assert!(
        source.contains("DSML"),
        "scavenge source must include Text block content: {source:?}",
    );

    let allowed: std::collections::HashSet<String> =
        ["read_file".to_string()].into_iter().collect();
    let result =
        crate::agent::agent_loop::scavenge::scavenge_tool_calls(Some(&source), &allowed, 4);
    assert_eq!(
        result.calls.len(),
        1,
        "orphan DSML in Text must be recovered: calls={:?}",
        result.calls
    );
    assert_eq!(result.calls[0].name, "read_file");
}

/// dirge-ngic: mixed Thinking + Text content — both contribute to
/// the scavenge corpus. Order is preserved (Thinking first as it
/// streams first), separated by `\n` so DSML on a line boundary
/// doesn't merge with surrounding chatter.
#[test]
fn scavenge_source_concatenates_thinking_and_text() {
    let blocks = vec![
        ContentBlock::Thinking {
            text: "Plan: call list_dir.".to_string(),
        },
        ContentBlock::Text {
            text: "Acting now.".to_string(),
        },
    ];
    let source = super::build_scavenge_source(&blocks);
    assert_eq!(source, "Plan: call list_dir.\nActing now.");
}

/// dirge-ngic: tool-call and other non-text blocks contribute
/// nothing to the scavenge corpus — only Thinking and Text.
#[test]
fn scavenge_source_skips_non_text_blocks() {
    let blocks = vec![
        ContentBlock::Text {
            text: "visible".to_string(),
        },
        ContentBlock::ToolCall {
            id: "call_1".to_string(),
            name: "noop".to_string(),
            arguments: serde_json::json!({}),
        },
    ];
    let source = super::build_scavenge_source(&blocks);
    assert_eq!(source, "visible");
}

// =====================================================================
// dirge-7bwx — truncation repair must run BEFORE storm so two
// streams whose raw args differ but heal to the same form dedupe
// under the storm filter. Reasonix order: `repair/index.ts:88-109`
// (truncation) then `:113-121` (storm).
// =====================================================================

/// dirge-7bwx: two ToolCalls with different truncated arg strings
/// that repair to the same canonical form must, after
/// `apply_truncation_repair`, present identical parsed arguments.
/// Pre-fix these survived storm because their pre-repair raw
/// strings hashed differently and only got repaired at dispatch
/// time, after the de-dupe window had closed.
#[test]
fn truncation_repair_canonicalizes_divergent_streams_before_storm() {
    use crate::agent::agent_loop::tool_input_repair::{RepairKind, RepairStats};
    use crate::agent::agent_loop::tools::ToolCall;

    // Same logical call, different truncation points.
    let call_a_raw = r#"{"path": "/tmp/x""#; // unterminated object
    let call_b_raw = r#"{"path": "/tmp/x"}"#; // already complete
    // Quick sanity: distinct strings → distinct pre-repair sigs.
    assert_ne!(call_a_raw, call_b_raw);

    let mut tool_calls = vec![
        ToolCall {
            id: "call_a".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::Value::String(call_a_raw.to_string()),
        },
        ToolCall {
            id: "call_b".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::Value::String(call_b_raw.to_string()),
        },
    ];

    let stats = RepairStats::new();
    let notes = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        String,
        Vec<String>,
    >::new()));
    super::apply_truncation_repair(&mut tool_calls, &stats, &notes);

    // Truncated A repaired; B was already valid JSON-as-string but
    // parsed-and-replaced.
    assert_eq!(tool_calls[0].arguments, tool_calls[1].arguments);
    assert_eq!(tool_calls[0].arguments["path"], "/tmp/x");
    assert!(
        stats.snapshot().truncation_fixed >= 1,
        "at least the truncated call must record TruncationFixed",
    );
}

/// dirge-7bwx: hard-fallback (closer can't rebalance) does NOT
/// replace arguments. Original `Value::String(raw)` is preserved
/// so `validate_and_repair` downstream surfaces a real validation
/// error rather than silently dispatching a fabricated value —
/// matches Reasonix's invariant at `repair/index.ts:93-102`.
/// Review-fix #1: telemetry STILL records the truncation event
/// (Reasonix bumps `truncationsFixed` on fallback at
/// `repair/index.ts:99`) so operators see unrecoverable-rate.
/// Review-fix #2: notes are emitted with the
/// `⚠️ TRUNCATION UNRECOVERABLE` prefix Reasonix uses at `:101`.
#[test]
fn truncation_repair_preserves_raw_on_hard_fallback() {
    use crate::agent::agent_loop::tool_input_repair::RepairStats;
    use crate::agent::agent_loop::tools::ToolCall;

    let unsalvageable = "}}}garbage no opening".to_string();
    let mut tool_calls = vec![ToolCall {
        id: "call_garbage".to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::Value::String(unsalvageable.clone()),
    }];

    let stats = RepairStats::new();
    let notes = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        String,
        Vec<String>,
    >::new()));
    super::apply_truncation_repair(&mut tool_calls, &stats, &notes);

    // Either preserved as the same Value::String, OR if the
    // closer happened to find a structured interpretation, it
    // must NOT be the empty/fabricated `{}` that masks a real
    // error. We test the strict case where fallback fires.
    if let serde_json::Value::String(after) = &tool_calls[0].arguments {
        assert_eq!(
            after, &unsalvageable,
            "hard fallback must not mutate the raw string",
        );
    }
    // Empty object is the canonical fabricated value Reasonix
    // refuses to emit; assert we never silently substitute it.
    assert_ne!(
        tool_calls[0].arguments,
        serde_json::json!({}),
        "hard fallback must not silently fabricate an empty object",
    );

    // dirge-7bwx review-fix #1: Reasonix parity — the counter
    // bumps on hard-fallback too (`repair/index.ts:99`).
    assert_eq!(
        stats.snapshot().truncation_fixed,
        1,
        "fallback must still bump truncation_fixed for operator telemetry",
    );

    // dirge-7bwx review-fix #2: the per-call notes carry the
    // `⚠️ TRUNCATION UNRECOVERABLE` prefix Reasonix uses at
    // `repair/index.ts:101`, attributed to the tool name.
    let sink = notes.lock().unwrap();
    let entry = sink
        .get("call_garbage")
        .expect("notes must be recorded for the fallback call");
    assert!(
        entry.iter().any(|n| n.contains("TRUNCATION UNRECOVERABLE")),
        "expected ⚠️ TRUNCATION UNRECOVERABLE prefix in notes: {entry:?}",
    );
    assert!(
        entry.iter().any(|n| n.contains("[read_file]")),
        "expected [tool_name] prefix in notes: {entry:?}",
    );
}

/// dirge-7bwx review-fix #3+5: end-to-end wiring proof. Drives
/// `run_agent_loop` with a canned assistant message that emits
/// THREE tool calls whose raw arg strings differ but heal to
/// the same canonical form. Default storm threshold is 3, so:
///   - pre-fix: 3 distinct raw `Value::String`s → 3 distinct
///     storm signatures → 3 executions, 0 suppressed.
///   - post-fix: `apply_truncation_repair` heals all three to
///     identical `Value::Object` BEFORE `storm.filter_calls`,
///     so storm's third entry hits `count >= threshold-1` and
///     suppresses → 2 executions + 1 storm-suppress.
/// This test would FAIL on the pre-hoist code (validate_and_repair
/// only ran post-storm), proving the wiring fix is live.
#[tokio::test]
async fn dirge_7bwx_end_to_end_storm_dedupes_after_truncation_repair() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo.clone());

    // Three calls whose raws differ but heal to the same form.
    // `{"v":1` and `{"v": 1` and `{"v":1 ` all heal to {"v":1}.
    fn truncated(raw: &str) -> serde_json::Value {
        serde_json::Value::String(raw.to_string())
    }
    let response = AssistantMessage::new(
        vec![
            ContentBlock::ToolCall {
                id: "tool-1".to_string(),
                name: "echo".to_string(),
                arguments: truncated(r#"{"v":1"#), // tight
            },
            ContentBlock::ToolCall {
                id: "tool-2".to_string(),
                name: "echo".to_string(),
                arguments: truncated(r#"{"v": 1"#), // single space
            },
            ContentBlock::ToolCall {
                id: "tool-3".to_string(),
                name: "echo".to_string(),
                arguments: truncated(r#"{"v":  1"#), // double space
            },
        ],
        StopReason::ToolUse,
    );
    let factory = canned_factory(vec![response, text_response("done")]);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
    let config = build_config();
    let repair_stats = config.repair_stats.clone();
    let _messages = run_agent_loop(
        vec![user("echo")],
        ctx,
        config,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None,
    )
    .await;
    drop(tx);

    // Storm default threshold=3 → first two pass, third is
    // suppressed. If the truncation hoist hadn't fired, all
    // three raws would have hashed differently and all three
    // would have executed.
    let executed_count = echo.executed.lock().unwrap().len();
    assert_eq!(
        executed_count, 2,
        "storm must catch the 3rd identical-post-repair call; got {executed_count} executions",
    );

    // Truncation repair recorded for all three.
    let snap = repair_stats.snapshot();
    assert_eq!(
        snap.truncation_fixed, 3,
        "truncation_fixed must be incremented per truncated call; got {snap:?}",
    );

    // Event stream: exactly two ToolExecutionEnd events.
    let events = drain(&mut rx).await;
    let execution_ends = events
        .iter()
        .filter(|e| e.kind() == "tool_execution_end")
        .count();
    assert_eq!(
        execution_ends,
        2,
        "expected 2 tool_execution_end events; got events={:?}",
        events.iter().map(|e| e.kind()).collect::<Vec<_>>(),
    );
}

/// dirge-ngic review-fix #3: end-to-end wiring proof for the
/// scavenge-source fix. Drives `run_agent_loop` with a canned
/// assistant message containing a DSML invoke ONLY in
/// `ContentBlock::Text` (no Thinking block, no declared
/// ToolCall). The loop must build the scavenge corpus from
/// Text (build_scavenge_source includes both Thinking and Text)
/// and dispatch the recovered call. Pre-fix this orphan would
/// not be recovered and zero executions would happen.
#[tokio::test]
async fn dirge_ngic_end_to_end_orphan_dsml_in_text_dispatches() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo.clone());

    // DSML invoke in Text only, no declared tool_calls. Empty
    // ToolUse-stopped message means scavenge is the ONLY path
    // to dispatch.
    let dsml = r#"<|DSML|invoke name="echo"><|DSML|parameter name="v" string="false">1</|DSML|parameter></|DSML|invoke>"#;
    let response = AssistantMessage::new(
        vec![ContentBlock::Text {
            text: dsml.to_string(),
        }],
        StopReason::ToolUse,
    );
    let factory = canned_factory(vec![response, text_response("done")]);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let config = build_config();
    let _messages = run_agent_loop(
        vec![user("echo")],
        ctx,
        config,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None,
    )
    .await;
    drop(tx);

    // Pre-fix: scavenge_source only had Thinking → empty
    // corpus → no scavenged call → 0 executions. Post-fix:
    // Text is included → DSML recovered → 1 execution.
    let executed = echo.executed.lock().unwrap();
    assert_eq!(
        executed.len(),
        1,
        "orphan DSML in Text must be recovered and dispatched (post-dirge-ngic); got {} executions",
        executed.len(),
    );
}

/// dirge-7bwx review-fix #2: successful repair also forwards
/// notes (without the unrecoverable prefix) so the model sees
/// what was fixed. Reasonix parity at `repair/index.ts:106`.
#[test]
fn truncation_repair_forwards_notes_on_successful_repair() {
    use crate::agent::agent_loop::tool_input_repair::RepairStats;
    use crate::agent::agent_loop::tools::ToolCall;

    let truncated = r#"{"path": "/tmp/x"#; // unterminated string
    let mut tool_calls = vec![ToolCall {
        id: "call_ok".to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::Value::String(truncated.to_string()),
    }];

    let stats = RepairStats::new();
    let notes = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        String,
        Vec<String>,
    >::new()));
    super::apply_truncation_repair(&mut tool_calls, &stats, &notes);

    // Args were promoted to the parsed form.
    assert_eq!(tool_calls[0].arguments["path"], "/tmp/x");
    // Counter bumped on success too.
    assert_eq!(stats.snapshot().truncation_fixed, 1);
    // Notes attributed to the tool, WITHOUT the unrecoverable
    // prefix.
    let sink = notes.lock().unwrap();
    let entry = sink
        .get("call_ok")
        .expect("notes must be recorded for the successful repair");
    assert!(entry.iter().any(|n| n.contains("[read_file]")));
    assert!(
        entry
            .iter()
            .all(|n| !n.contains("TRUNCATION UNRECOVERABLE")),
        "successful repair must not carry the unrecoverable prefix: {entry:?}",
    );
}

/// dirge-7bwx: structurally valid args (real `Value::Object`)
/// pass through untouched — only `Value::String` triggers the
/// repair pass.
#[test]
fn truncation_repair_leaves_already_parsed_args_alone() {
    use crate::agent::agent_loop::tool_input_repair::{RepairKind, RepairStats};
    use crate::agent::agent_loop::tools::ToolCall;

    let already_parsed = serde_json::json!({ "path": "/tmp/y" });
    let mut tool_calls = vec![ToolCall {
        id: "call_ok".to_string(),
        name: "read_file".to_string(),
        arguments: already_parsed.clone(),
    }];

    let stats = RepairStats::new();
    let notes = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        String,
        Vec<String>,
    >::new()));
    super::apply_truncation_repair(&mut tool_calls, &stats, &notes);

    assert_eq!(tool_calls[0].arguments, already_parsed);
    assert_eq!(
        stats.snapshot().truncation_fixed,
        0,
        "no repair should be recorded for already-parsed args",
    );
}

// ============================================================
// dirge-k6be — turn-end per-tool-result cap wiring
// ============================================================

/// dirge-k6be end-to-end: a tool that returns a 60 KB result
/// drops into the transcript verbatim, but the NEXT model
/// call must see the capped form. Proves `run_loop` calls
/// `cap_oversized_tool_results` before each
/// `stream_assistant_response`, matching Reasonix
/// `loop.ts:486-503` (`healActiveLogBeforeSend`).
#[tokio::test]
async fn dirge_k6be_oversized_tool_result_capped_before_next_model_call() {
    use crate::agent::agent_loop::stream::{LlmContext, StreamFn};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Tool that returns ~60 KB so it's well over the 3000-token
    // (12 KB) cap.
    #[derive(Debug)]
    struct BigOutputTool;
    impl LoopTool for BigOutputTool {
        fn name(&self) -> &str {
            "big_read"
        }
        fn description(&self) -> &str {
            "Big tool"
        }
        fn label(&self) -> &str {
            "BigRead"
        }
        fn parameters(&self) -> &Value {
            static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            _args: Value,
            _signal: AbortSignal,
            _on_update: super::super::tool::LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<super::super::LoopToolResult, String>> + Send + 'a>>
        {
            let huge = "x".repeat(60_000);
            Box::pin(async move {
                Ok(super::super::LoopToolResult {
                    content: vec![serde_json::json!({
                        "type": "text",
                        "text": huge,
                    })],
                    details: Value::Null,
                    terminate: None,
                })
            })
        }
    }

    // Capture what each model call sees so we can assert the
    // tool result was capped before the second call.
    let observed_second_call_payload: std::sync::Arc<Mutex<Option<Vec<Value>>>> =
        std::sync::Arc::new(Mutex::new(None));
    let observed_clone = observed_second_call_payload.clone();
    let counter = std::sync::Arc::new(AtomicUsize::new(0));

    let factory: StreamFn = std::sync::Arc::new(move |ctx: LlmContext, _opts| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        if n == 1 {
            *observed_clone.lock().unwrap() = Some(ctx.messages.clone());
        }
        let msg = if n == 0 {
            tool_use_response("call-1", "big_read", serde_json::json!({}))
        } else {
            text_response("done")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![
            crate::agent::agent_loop::message::StreamEvent::Done {
                reason,
                message: msg,
                usage: None,
            },
        ]))
    });

    let mut ctx = empty_context();
    ctx.tools.push(std::sync::Arc::new(BigOutputTool));
    let mut cfg = build_config();
    cfg.tool_execution = ToolExecutionMode::Sequential;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let _ = run_agent_loop(
        vec![user("start")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None,
    )
    .await;

    let observed = observed_second_call_payload.lock().unwrap();
    let messages = observed
        .as_ref()
        .expect("second model call must have happened");

    // Find the tool-result message in the payload the model
    // saw on call #2.
    let tool_result = messages
        .iter()
        .find(|m| {
            m.get("role").and_then(|v| v.as_str()) == Some("toolResult")
                || m.get("role").and_then(|v| v.as_str()) == Some("tool")
        })
        .expect("second call must include the tool result");

    // The result must be CAPPED — its content's total text
    // length is far below the original 60 KB. The 3000-token
    // cap = 12 KB; allow some slack for marker overhead.
    let blocks = tool_result["content"]
        .as_array()
        .expect("tool result content should be an array of blocks");
    let total_text_len: usize = blocks
        .iter()
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .map(|t| t.len())
        .sum();
    assert!(
        total_text_len < 60_000,
        "tool result must be capped before the second model call; got {total_text_len} chars",
    );
    assert!(
        total_text_len < 14_000,
        "capped result must be near the ~12 KB cap; got {total_text_len} chars",
    );
    // And the marker must be present.
    let combined: String = blocks
        .iter()
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect();
    assert!(
        combined.contains("truncated"),
        "capped result must carry the truncation marker",
    );
}

// ============================================================
// dirge-el3n — proactive turn-start fold wiring
// ============================================================

/// dirge-el3n end-to-end: when the message log is loaded with
/// content over 90% of the context window AT TURN START, the
/// proactive fold fires before the next model call. Without
/// the fix the warning was logged but nothing was shrunk.
/// Asserts the second LLM call sees a SMALLER context than
/// the loaded one — proving the fold actually ran.
#[tokio::test]
async fn dirge_el3n_proactive_fold_fires_when_threshold_crossed_at_turn_start() {
    use crate::agent::agent_loop::stream::{LlmContext, StreamFn};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Pre-load a context that's well over 90% of the
    // 128_000-token default ctx window. 130_000 chars / 4 ≈
    // 32_500 tokens. To cross 0.9 ratio (= 115_200 tokens) we
    // need ~460_000 chars of content.
    let huge_text = "x".repeat(500_000);
    let preloaded = vec![serde_json::json!({
        "role": "toolResult",
        "content": [{"type": "text", "text": huge_text}],
        "toolName": "read",
    })];

    // Capture the message count the second model call sees.
    // After the fold, oversized tool results in the middle
    // section should have been pruned to 1-liners — total
    // string content should drop materially.
    let observed_second_call_total_chars: std::sync::Arc<Mutex<Option<usize>>> =
        std::sync::Arc::new(Mutex::new(None));
    let observed_clone = observed_second_call_total_chars.clone();
    let counter = std::sync::Arc::new(AtomicUsize::new(0));

    let factory: StreamFn = std::sync::Arc::new(move |ctx: LlmContext, _opts| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // Total content text on the FIRST call (the call
            // that's supposed to be preceded by the fold).
            let total: usize = ctx
                .messages
                .iter()
                .map(|m| match m.get("content") {
                    Some(serde_json::Value::String(s)) => s.len(),
                    Some(serde_json::Value::Array(blocks)) => blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .map(|t| t.len())
                        .sum(),
                    _ => 0,
                })
                .sum();
            *observed_clone.lock().unwrap() = Some(total);
        }
        let msg = text_response("ok");
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![
            crate::agent::agent_loop::message::StreamEvent::Done {
                reason,
                message: msg,
                usage: None,
            },
        ]))
    });

    let mut ctx = empty_context();
    ctx.messages = preloaded;
    let mut cfg = build_config();
    cfg.tool_execution = ToolExecutionMode::Sequential;
    // The proactive fold uses ctx_max from the model's known
    // window. With no model_name set, it defaults to 128_000.

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let _ = run_agent_loop(
        vec![user("start")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None,
    )
    .await;

    let observed = observed_second_call_total_chars.lock().unwrap();
    let total_after_fold = observed.expect("first model call must have happened");
    // The fold should have shrunk the 500 KB tool-result text
    // dramatically — pruning replaces oversized tool results
    // with 1-line summaries. Pre-fix this value would have
    // been ~500_000 (no fold fired). Post-fix it must be way
    // smaller because prune_tool_outputs ran.
    assert!(
        total_after_fold < 100_000,
        "proactive fold should have shrunk the preloaded transcript; saw {total_after_fold} chars",
    );
}

/// dirge-el3n: the proactive fold does NOT fire when the
/// ratio is comfortably under threshold. Guards against
/// over-aggressive folding that would shrink useful context.
#[tokio::test]
async fn dirge_el3n_proactive_fold_does_not_fire_under_threshold() {
    use crate::agent::agent_loop::stream::{LlmContext, StreamFn};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Modest tool result — well under 90% of 128k token window.
    let modest = "y".repeat(4_000);
    let preloaded = vec![serde_json::json!({
        "role": "toolResult",
        "content": [{"type": "text", "text": modest}],
        "toolName": "read",
    })];

    let observed_first_call_chars: std::sync::Arc<Mutex<Option<usize>>> =
        std::sync::Arc::new(Mutex::new(None));
    let observed_clone = observed_first_call_chars.clone();
    let counter = std::sync::Arc::new(AtomicUsize::new(0));

    let factory: StreamFn = std::sync::Arc::new(move |ctx: LlmContext, _opts| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            let total: usize = ctx
                .messages
                .iter()
                .map(|m| match m.get("content") {
                    Some(serde_json::Value::String(s)) => s.len(),
                    Some(serde_json::Value::Array(blocks)) => blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .map(|t| t.len())
                        .sum(),
                    _ => 0,
                })
                .sum();
            *observed_clone.lock().unwrap() = Some(total);
        }
        let msg = text_response("ok");
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![
            crate::agent::agent_loop::message::StreamEvent::Done {
                reason,
                message: msg,
                usage: None,
            },
        ]))
    });

    let mut ctx = empty_context();
    ctx.messages = preloaded;
    let mut cfg = build_config();
    cfg.tool_execution = ToolExecutionMode::Sequential;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let _ = run_agent_loop(
        vec![user("start")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None,
    )
    .await;

    // Under-threshold: tool-result content must be present in
    // full (modulo the dirge-k6be cap which only fires above
    // 3000 tokens = ~12 KB; 4 KB is well under that). The
    // fold must NOT have shrunk the transcript.
    let observed = observed_first_call_chars.lock().unwrap();
    let total = observed.expect("first model call must have happened");
    assert!(
        total >= 4_000,
        "under-threshold ratio must not trigger fold; saw {total} chars (input was 4000)",
    );
}

// IMPROVEMENTS_PLAN #1: the compaction circuit breaker. After
// MAX_CONSECUTIVE_COMPACTION_FAILURES failures the LLM summarizer is no
// longer invoked (cheap pruning still runs).
#[test]
fn record_compaction_outcome_drives_counter() {
    let mut f = 0u32;
    super::record_compaction_outcome(&mut f, super::SummaryOutcome::Failed);
    assert_eq!(f, 1);
    super::record_compaction_outcome(&mut f, super::SummaryOutcome::Failed);
    assert_eq!(f, 2);
    super::record_compaction_outcome(&mut f, super::SummaryOutcome::Skipped);
    assert_eq!(f, 2, "skip must not change the counter");
    super::record_compaction_outcome(&mut f, super::SummaryOutcome::Succeeded(0));
    assert_eq!(f, 0, "success resets the counter");
}

#[tokio::test]
async fn compaction_circuit_breaker_skips_summarizer_after_max_failures() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let calls_inner = calls.clone();
    // Summarizer that always fails — and counts its invocations.
    let summarize_fn: Option<crate::agent::compression::SummarizeFn> =
        Some(std::sync::Arc::new(move |_prompt: String| {
            let c = calls_inner.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::anyhow!("summarizer boom"))
            })
        }));

    let make_ctx = || {
        let mut ctx = empty_context();
        ctx.messages
            .push(serde_json::json!({"role":"system","content":"agent"}));
        ctx.messages
            .push(serde_json::json!({"role":"user","content":"task"}));
        for i in 0..20 {
            let role = if i % 2 == 0 { "assistant" } else { "user" };
            ctx.messages.push(serde_json::json!({
                "role": role, "content": format!("turn {i} with filler content")
            }));
        }
        ctx.messages
            .push(serde_json::json!({"role":"user","content":"latest"}));
        ctx
    };

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);

    // Sub-threshold: the summarizer IS called and reports Failed.
    for failures in 0..super::MAX_CONSECUTIVE_COMPACTION_FAILURES {
        let mut ctx = make_ctx();
        let outcome =
            super::run_compaction_pass(&mut ctx, &summarize_fn, 5, failures, &None, None, &tx)
                .await;
        assert_eq!(
            outcome,
            super::SummaryOutcome::Failed,
            "failures={failures}: summarizer should run and fail"
        );
    }
    let calls_before_open = calls.load(Ordering::SeqCst);
    assert_eq!(
        calls_before_open,
        super::MAX_CONSECUTIVE_COMPACTION_FAILURES as usize,
        "summarizer should run once per sub-threshold attempt"
    );

    // At the threshold: breaker open → summarizer NOT called again, and
    // the cheap prune-only fallback still runs (context doesn't grow).
    let mut ctx = make_ctx();
    let n_before = ctx.messages.len();
    let outcome = super::run_compaction_pass(
        &mut ctx,
        &summarize_fn,
        5,
        super::MAX_CONSECUTIVE_COMPACTION_FAILURES,
        &None,
        None,
        &tx,
    )
    .await;
    assert_eq!(
        outcome,
        super::SummaryOutcome::Skipped,
        "breaker open → summarizer skipped"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        calls_before_open,
        "breaker open: summarizer must NOT be invoked"
    );
    assert!(
        ctx.messages.len() <= n_before,
        "prune-only fallback must not grow context"
    );
}

// IMPROVEMENTS_PLAN #5: the ContextCompacted event reports whether the
// pass was prune-only, prune+summary, or prune+failed-summary.
#[tokio::test]
async fn context_compacted_reports_compaction_kind() {
    use crate::event::CompactionKind;

    async fn kind_for(
        summarize_fn: Option<crate::agent::compression::SummarizeFn>,
        failures: u32,
    ) -> CompactionKind {
        let mut ctx = empty_context();
        ctx.messages
            .push(serde_json::json!({"role":"system","content":"agent"}));
        ctx.messages
            .push(serde_json::json!({"role":"user","content":"task"}));
        for i in 0..20 {
            let role = if i % 2 == 0 { "assistant" } else { "user" };
            ctx.messages.push(serde_json::json!({
                "role": role, "content": format!("turn {i} with filler content")
            }));
        }
        ctx.messages
            .push(serde_json::json!({"role":"user","content":"latest"}));
        let (tx, mut rx) = mpsc::channel::<LoopEvent>(8);
        super::run_compaction_pass(&mut ctx, &summarize_fn, 5, failures, &None, None, &tx).await;
        drop(tx);
        while let Some(ev) = rx.recv().await {
            if let LoopEvent::ContextCompacted {
                compaction_kind, ..
            } = ev
            {
                return compaction_kind;
            }
        }
        panic!("no ContextCompacted event emitted");
    }

    // Valid summary → PruneAndSummary.
    let good: Option<crate::agent::compression::SummarizeFn> = Some(std::sync::Arc::new(
        |_p: String| {
            Box::pin(async move {
                Ok("## Active Task\nx\n\n## Goal\ny\n\n## Completed Actions\n1. z\n\n## Remaining Work\nw"
                    .to_string())
            })
        },
    ));
    assert_eq!(kind_for(good, 0).await, CompactionKind::PruneAndSummary);

    // Failing summary → PruneAndFailedSummary.
    let bad: Option<crate::agent::compression::SummarizeFn> =
        Some(std::sync::Arc::new(|_p: String| {
            Box::pin(async move { Err(anyhow::anyhow!("boom")) })
        }));
    assert_eq!(
        kind_for(bad, 0).await,
        CompactionKind::PruneAndFailedSummary
    );

    // No summarizer wired → PruneOnly.
    assert_eq!(kind_for(None, 0).await, CompactionKind::PruneOnly);

    // Summarizer wired but the circuit breaker is OPEN (failures at the
    // cap) → PruneSummarizerDisabled, NOT PruneOnly. The distinct kind
    // keeps the ongoing-failure signal visible after the breaker latches
    // instead of masquerading as a healthy no-summarizer pass. Use a
    // summarizer that would SUCCEED if called, to prove the kind comes
    // from the breaker being open and not from the summarizer's outcome.
    let would_succeed: Option<crate::agent::compression::SummarizeFn> = Some(std::sync::Arc::new(
        |_p: String| {
            Box::pin(async move {
                Ok("## Active Task\nx\n\n## Goal\ny\n\n## Completed Actions\n1. z\n\n## Remaining Work\nw"
                    .to_string())
            })
        },
    ));
    assert_eq!(
        kind_for(would_succeed, super::MAX_CONSECUTIVE_COMPACTION_FAILURES).await,
        CompactionKind::PruneSummarizerDisabled
    );
}

// ── dirge-vcsn: unified finalization interjection authority ──────────

/// The unfinished-todo nudge wording agrees in number with the count.
#[test]
fn todo_nudge_message_pluralizes() {
    let one = match todo_nudge_message(1) {
        LoopMessage::User(u) => u.content,
        _ => panic!("expected a user message"),
    };
    assert!(one.contains("1 unfinished todo "), "singular: {one}");
    let many = match todo_nudge_message(3) {
        LoopMessage::User(u) => u.content,
        _ => panic!("expected a user message"),
    };
    assert!(many.contains("3 unfinished todos "), "plural: {many}");
}

/// Highest-priority gate (the caller hook) short-circuits the lower gates:
/// when it yields a follow-up, the critic is never consulted (`critic_done`
/// stays false) and the todo gate isn't reached. This locks the precedence.
#[tokio::test]
async fn finalization_hook_short_circuits_lower_gates() {
    let mut config = build_config();
    config.get_followup_messages = Some(std::sync::Arc::new(|| {
        Box::pin(async {
            vec![LoopMessage::User(
                crate::agent::agent_loop::message::UserMessage {
                    content: "hook follow-up".into(),
                },
            )]
        })
    }));

    let mut critic_done = false;
    let mut todo_nudges = 0u8;
    let (msgs, source) =
        poll_finalization_follow_up(&config, "sys", &[], &mut critic_done, &mut todo_nudges).await;

    assert_eq!(source, FollowUpSource::Hook);
    assert_eq!(msgs.len(), 1);
    assert!(
        !critic_done,
        "hook must short-circuit before the critic runs"
    );
    assert_eq!(todo_nudges, 0, "todo gate must not be reached");
}

/// With no hook/verifier/critic and the todo gate exhausted, the authority
/// reports `None` so the run finalizes. (`todo_nudges = MAX` keeps this
/// deterministic regardless of the process-global todo list.)
#[tokio::test]
async fn finalization_all_gates_silent_yields_none() {
    let config = build_config(); // hook/verifier/critic all None
    let mut critic_done = false;
    let mut todo_nudges = MAX_TODO_NUDGES; // todo gate bounded out
    let (msgs, source) =
        poll_finalization_follow_up(&config, "sys", &[], &mut critic_done, &mut todo_nudges).await;

    assert!(msgs.is_empty());
    assert_eq!(source, FollowUpSource::None);
}

/// A tool that always fails. Distinct args per call so the storm
/// breaker (which only suppresses *identical* repeats) lets every call
/// through — the scenario the failure tracker exists to catch.
#[derive(Debug)]
struct FailingTool;
impl LoopTool for FailingTool {
    fn name(&self) -> &str {
        "boom"
    }
    fn description(&self) -> &str {
        "Always fails"
    }
    fn label(&self) -> &str {
        "Boom"
    }
    fn parameters(&self) -> &Value {
        static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
    }
    fn execute<'a>(
        &'a self,
        _id: &'a str,
        _args: Value,
        _signal: AbortSignal,
        _on_update: LoopToolUpdate,
    ) -> Pin<Box<dyn Future<Output = Result<super::super::LoopToolResult, String>> + Send + 'a>>
    {
        Box::pin(async move { Err("boom: nothing matched".to_string()) })
    }
}

/// dirge-opdt: three consecutive *distinct* tool failures inject a
/// recovery checkpoint into the conversation. Distinct args dodge the
/// storm breaker, proving the failure tracker covers the gap storm
/// leaves open.
#[tokio::test]
async fn consecutive_distinct_failures_inject_recovery_checkpoint() {
    let mut ctx = empty_context();
    ctx.tools.push(std::sync::Arc::new(FailingTool));

    let factory = canned_factory(vec![
        tool_use_response("c1", "boom", serde_json::json!({"n": 1})),
        tool_use_response("c2", "boom", serde_json::json!({"n": 2})),
        tool_use_response("c3", "boom", serde_json::json!({"n": 3})),
        text_response("giving up"),
    ]);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(256);
    let messages = run_agent_loop(
        vec![user("do the thing")],
        ctx,
        build_config(),
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None,
    )
    .await;
    drop(tx);

    let checkpoint = messages.iter().find_map(|m| match m {
        LoopMessage::User(u) if u.content.contains("[Recovery checkpoint]") => {
            Some(u.content.clone())
        }
        _ => None,
    });
    let body =
        checkpoint.expect("a recovery checkpoint must be injected after 3 distinct failures");
    assert!(body.contains("3 tool calls in a row have failed"));
    assert!(body.contains("boom: nothing matched"));
    assert!(body.contains("DIFFERENT next step"));
}

/// A single failure followed by a success leaves no checkpoint — the
/// streak resets on the good result.
#[tokio::test]
async fn failure_then_success_injects_no_checkpoint() {
    let mut ctx = empty_context();
    ctx.tools.push(std::sync::Arc::new(FailingTool));
    ctx.tools.push(std::sync::Arc::new(EchoTool::new()));

    let factory = canned_factory(vec![
        tool_use_response("c1", "boom", serde_json::json!({"n": 1})),
        tool_use_response("c2", "echo", serde_json::json!({"v": 1})),
        tool_use_response("c3", "boom", serde_json::json!({"n": 2})),
        text_response("ok"),
    ]);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(256);
    let messages = run_agent_loop(
        vec![user("go")],
        ctx,
        build_config(),
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None,
    )
    .await;
    drop(tx);

    assert!(
        !messages.iter().any(|m| matches!(
            m,
            LoopMessage::User(u) if u.content.contains("[Recovery checkpoint]")
        )),
        "a success between failures must reset the streak"
    );
}
