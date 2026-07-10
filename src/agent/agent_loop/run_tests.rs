use super::*;
use crate::agent::agent_loop::hooks::{
    AfterToolCallContext, AfterToolCallFn, GetSteeringMessagesFn, PrepareNextTurnFn,
    ShouldStopAfterTurnFn,
};
use crate::agent::agent_loop::message::{StreamEvent, UserMessage};
use crate::agent::agent_loop::result::AfterToolCallResult;
use crate::agent::agent_loop::stream::StreamFn;
use crate::agent::agent_loop::tool::{AbortSignal, LoopTool, LoopToolUpdate};
use crate::agent::agent_loop::types::{
    ConvertToLlmFn, GateMode, LoopConfig, ToolExecutionMode, TurnUpdate,
};
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// An empty reusable-checkpoint slot for the compaction-pass tests. With no
/// cached summary the fold always takes the inline summarizer path, so these
/// tests exercise the same behavior they did before Round 1's fast path.
fn empty_checkpoint_slot() -> super::CheckpointSlot {
    std::sync::Arc::new(std::sync::Mutex::new(None))
}

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

/// Like [`canned_factory`] but records a JSON snapshot of each call's
/// context messages into `seen`, so a test can inspect what the model was
/// actually sent (e.g. a mid-loop memory re-injection).
fn capturing_factory(
    responses: Vec<AssistantMessage>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) -> StreamFn {
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let responses = std::sync::Arc::new(responses);
    std::sync::Arc::new(move |ctx, _opts| {
        seen.lock()
            .unwrap()
            .push(serde_json::to_string(&ctx.messages).unwrap_or_default());
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
        provider_name: None,
        model_name: None,
        asset_dir: None,
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
        code_review_fn: None,
        code_review_mode: crate::agent::agent_loop::types::CodeReviewMode::default(),
        open_issues_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        session_id: None,
        goal_fn: None,
        goal: None,
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

/// Regression: a transient mid-stream error ("error decoding response
/// body") arriving AFTER the model has streamed content must NOT kill
/// the run. The streaming retry layer can't replay the turn (the
/// partial is already on screen), but the run loop recovers: it keeps
/// the preserved partial, nudges the model to continue, and the next
/// turn proceeds — instead of tearing down to idle and dropping any
/// queued steering.
#[tokio::test]
async fn transient_midstream_error_recovers_instead_of_terminating() {
    use crate::agent::agent_loop::message::{DeltaPhase, LoopEvent};
    let call = std::sync::Arc::new(AtomicUsize::new(0));
    let factory: StreamFn = std::sync::Arc::new({
        let call = call.clone();
        move |_ctx, _opts| {
            let n = call.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // First call: stream some text, then die mid-stream
                // with a transient transport error.
                let partial = AssistantMessage::new(
                    vec![ContentBlock::Text {
                        text: "working on it".to_string(),
                    }],
                    StopReason::Stop,
                );
                Box::pin(futures::stream::iter(vec![
                    StreamEvent::Start {
                        partial: AssistantMessage::new(Vec::new(), StopReason::Stop),
                    },
                    StreamEvent::Delta {
                        partial,
                        phase: DeltaPhase::TextDelta,
                    },
                    StreamEvent::Error {
                        error: "error decoding response body".to_string(),
                    },
                ]))
            } else {
                // Recovery turn: complete normally.
                let msg = AssistantMessage::new(
                    vec![ContentBlock::Text {
                        text: "all done now".to_string(),
                    }],
                    StopReason::Stop,
                );
                Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: msg,
                    usage: None,
                }]))
            }
        }
    });

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let messages = run_agent_loop(
        vec![LoopMessage::User(UserMessage::text("start"))],
        empty_context(),
        build_config(),
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None,
    )
    .await;

    // The run recovered past the error: the final assistant turn
    // completed instead of the run dying on the errored turn.
    let last_text = messages.iter().rev().find_map(|m| match m {
        LoopMessage::Assistant(a) => a.content.iter().find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        }),
        _ => None,
    });
    assert_eq!(
        last_text.as_deref(),
        Some("all done now"),
        "run must continue past a transient error and complete the recovery turn"
    );

    // A retry/recovery banner was surfaced so the UI isn't silent.
    let mut saw_retry = false;
    while let Ok(evt) = rx.try_recv() {
        if matches!(evt, LoopEvent::RetryNotice { .. }) {
            saw_retry = true;
        }
    }
    assert!(
        saw_retry,
        "recovery should surface a RetryNotice banner instead of dying silently"
    );
}

/// A sustained outage (every call fails transiently) must still
/// terminate — the recovery budget caps consecutive recoveries so a
/// dead network can't loop the run forever. After the budget is spent
/// the error surfaces as terminal, exactly as it did before recovery.
#[tokio::test]
async fn sustained_transient_error_terminates_after_budget() {
    use crate::agent::agent_loop::message::DeltaPhase;
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let factory: StreamFn = std::sync::Arc::new({
        let calls = calls.clone();
        move |_ctx, _opts| {
            calls.fetch_add(1, Ordering::SeqCst);
            let partial = AssistantMessage::new(
                vec![ContentBlock::Text {
                    text: "halfway".to_string(),
                }],
                StopReason::Stop,
            );
            Box::pin(futures::stream::iter(vec![
                StreamEvent::Start {
                    partial: AssistantMessage::new(Vec::new(), StopReason::Stop),
                },
                StreamEvent::Delta {
                    partial,
                    phase: DeltaPhase::TextDelta,
                },
                StreamEvent::Error {
                    error: "error decoding response body".to_string(),
                },
            ]))
        }
    });

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let messages = run_agent_loop(
        vec![LoopMessage::User(UserMessage::text("start"))],
        empty_context(),
        build_config(),
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None,
    )
    .await;

    // Recovered MAX_TRANSIENT_RECOVERIES times, then one final terminal
    // error — not an unbounded loop.
    let total_calls = calls.load(Ordering::SeqCst);
    assert_eq!(
        total_calls,
        (MAX_TRANSIENT_RECOVERIES as usize) + 1,
        "run must stop after the recovery budget is exhausted, not loop forever"
    );

    // The run terminated on a real error (not a clean Stop).
    let last = messages
        .iter()
        .rev()
        .find_map(|m| match m {
            LoopMessage::Assistant(a) => Some(a),
            _ => None,
        })
        .expect("an assistant message exists");
    assert_eq!(
        last.stop_reason,
        StopReason::Error,
        "after the budget the error must surface as terminal"
    );
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
    super::run_compaction_pass(
        &mut ctx,
        &summarize_fn,
        5,
        0,
        &None,
        None,
        &tx,
        &empty_checkpoint_slot(),
        &mut 0,
        u64::MAX,
    )
    .await;
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

/// Build a padded context with `n` alternating turns after a system +
/// initial-user pair, for the compaction-pass tests.
fn padded_ctx(n: usize) -> super::Context {
    let mut ctx = empty_context();
    ctx.messages
        .push(serde_json::json!({"role": "system", "content": "you are an agent"}));
    ctx.messages
        .push(serde_json::json!({"role": "user", "content": "initial task"}));
    for i in 0..n {
        let role = if i % 2 == 0 { "assistant" } else { "user" };
        ctx.messages.push(serde_json::json!({
            "role": role,
            "content": format!("turn {i} with some content to fill bytes"),
        }));
    }
    ctx.messages
        .push(serde_json::json!({"role": "user", "content": "latest user request"}));
    ctx
}

/// A summarizer that records whether it was called and returns a distinct
/// inline summary, so a test can tell the inline path from the fast path.
fn recording_summarizer(
    called: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Option<crate::agent::compression::SummarizeFn> {
    Some(std::sync::Arc::new(move |_prompt: String| {
        let called = called.clone();
        Box::pin(async move {
            called.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("## Active Task\nINLINE SUMMARY\n## Remaining Work\nx".to_string())
        })
    }))
}

/// A populated checkpoint slot for the fast-path tests.
fn slot_with(summary: &str, boundary: usize, generation: u64) -> super::CheckpointSlot {
    std::sync::Arc::new(std::sync::Mutex::new(Some(super::CachedCheckpoint {
        summary: summary.to_string(),
        boundary,
        generation,
    })))
}

/// Round 1 fast path: when the slot holds a fresh checkpoint (matching the
/// current epoch) and reusing it clears the fold target, the fold splices it
/// in WITHOUT calling the inline summarizer, then bumps the epoch and clears
/// the slot.
#[tokio::test]
async fn run_compaction_pass_reuses_fresh_checkpoint_without_calling_summarizer() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut ctx = padded_ctx(20);
    let called = std::sync::Arc::new(AtomicBool::new(false));
    let summarize_fn = recording_summarizer(called.clone());
    let slot = slot_with(
        "## Active Task\nFROM CHECKPOINT\n## Remaining Work\nfinish",
        10,
        0,
    );
    let mut generation = 0u64;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(16);
    let outcome = super::run_compaction_pass(
        &mut ctx,
        &summarize_fn,
        5,
        0,
        &None,
        None,
        &tx,
        &slot,
        &mut generation,
        u64::MAX,
    )
    .await;
    drop(tx);

    assert!(
        matches!(outcome, super::SummaryOutcome::Succeeded(_)),
        "reuse should succeed"
    );
    assert!(
        !called.load(Ordering::SeqCst),
        "inline summarizer must NOT be called on the fast path"
    );
    let summary_msg = ctx
        .messages
        .iter()
        .find_map(|m| {
            let c = m.get("content").and_then(|v| v.as_str())?;
            c.contains("CONTEXT COMPACTION").then_some(c)
        })
        .expect("a summary message should be present");
    assert!(
        summary_msg.contains("FROM CHECKPOINT"),
        "the spliced summary should be the checkpoint's, not the inline one"
    );
    assert_eq!(generation, 1, "a successful fold bumps the epoch");
    assert!(
        slot.lock().unwrap().is_none(),
        "the consumed checkpoint slot is cleared after the fold"
    );
}

/// A checkpoint from a stale epoch (generation mismatch) is ignored — the
/// fold falls back to the inline summarizer.
#[tokio::test]
async fn run_compaction_pass_ignores_stale_generation_checkpoint() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut ctx = padded_ctx(20);
    let called = std::sync::Arc::new(AtomicBool::new(false));
    let summarize_fn = recording_summarizer(called.clone());
    // Slot built under epoch 0, but the loop is now at epoch 7.
    let slot = slot_with(
        "## Active Task\nFROM CHECKPOINT\n## Remaining Work\nx",
        10,
        0,
    );
    let mut generation = 7u64;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(16);
    let outcome = super::run_compaction_pass(
        &mut ctx,
        &summarize_fn,
        5,
        0,
        &None,
        None,
        &tx,
        &slot,
        &mut generation,
        u64::MAX,
    )
    .await;
    drop(tx);

    assert!(matches!(outcome, super::SummaryOutcome::Succeeded(_)));
    assert!(
        called.load(Ordering::SeqCst),
        "stale checkpoint → inline summarizer must run"
    );
    let summary_msg = ctx
        .messages
        .iter()
        .find_map(|m| {
            let c = m.get("content").and_then(|v| v.as_str())?;
            c.contains("CONTEXT COMPACTION").then_some(c)
        })
        .expect("a summary message should be present");
    assert!(
        summary_msg.contains("INLINE SUMMARY"),
        "the inline summary should be used when the checkpoint is stale"
    );
}

/// Serializes the tests that touch a process-global memory flag (the
/// memory-dirty flag and the verbatim-pre-recall toggle) so they don't perturb
/// each other under parallel test execution — e.g. a pre-recall test leaving
/// the toggle on while a memory-refresh test runs `run_agent_loop` with a
/// provider. Every test that flips one of those globals holds this lock.
static DIRTY_FLAG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Round 2 flag: `mark_memories_dirty` then `take_memories_dirty` returns
/// true exactly once, then false.
#[test]
fn memories_dirty_flag_is_consumed_once() {
    use crate::agent::agent_loop::context_manager::{mark_memories_dirty, take_memories_dirty};
    let _guard = DIRTY_FLAG_TEST_LOCK.lock().unwrap();
    // Clear any prior state from other tests touching the global.
    let _ = take_memories_dirty();
    mark_memories_dirty();
    assert!(take_memories_dirty(), "first take after mark is true");
    assert!(!take_memories_dirty(), "second take resets to false");
}

/// Round 2 behavior: when consolidation has marked memories dirty, the loop
/// re-injects the refreshed memory block into the model-facing context at the
/// next turn boundary, so the agent sees it without a restart.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // current-thread test runtime; lock only serializes the global flag
async fn memory_refresh_injects_block_at_turn_boundary_when_dirty() {
    use crate::extras::memory_provider::MemoryProvider;
    let _guard = DIRTY_FLAG_TEST_LOCK.lock().unwrap();

    struct StubProvider;
    impl MemoryProvider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }
        fn format_for_system_prompt(&self) -> String {
            "STUBMEM: prefer the fast path".to_string()
        }
        fn view(&self, _t: &str) -> Value {
            serde_json::json!({})
        }
        fn add(&self, _: &str, _: &str, _: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }
        fn replace(&self, _: &str, _: &str, _: &str, _: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }
        fn remove(&self, _: &str, _: &str) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }
    }

    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo.clone());

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    // Turn 1: a tool call (forces another loop iteration → a turn boundary).
    // Turn 2: final text.
    let factory = capturing_factory(
        vec![
            tool_use_response("call-1", "echo", serde_json::json!({"v": 1})),
            text_response("done"),
        ],
        seen.clone(),
    );

    let provider: std::sync::Arc<dyn MemoryProvider> = std::sync::Arc::new(StubProvider);

    // The injected refresh is a `system` message; use a converter that keeps
    // system messages (production's does — fold summaries are system-role).
    let mut config = build_config();
    config.convert_to_llm = std::sync::Arc::new(|messages: &[Value]| {
        messages
            .iter()
            .filter(|m| {
                let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
                matches!(
                    role,
                    "user" | "assistant" | "tool" | "toolResult" | "system"
                )
            })
            .cloned()
            .collect()
    });

    // Simulate background consolidation completing: mark dirty just before
    // the run so the next turn boundary consumes it.
    crate::agent::agent_loop::context_manager::mark_memories_dirty();

    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let _ = run_agent_loop(
        vec![user("echo please")],
        ctx,
        config,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        Some(provider),
    )
    .await;
    drop(tx);

    let snapshots = seen.lock().unwrap().clone();
    assert!(
        snapshots.iter().any(|s| s.contains("STUBMEM")),
        "the refreshed memory block should appear in the model-facing context \
         after the turn boundary; snapshots={snapshots:?}"
    );
}

/// dirge-0gxb: with verbatim pre-recall on, the hits from searching the
/// verbatim user message reach the MODEL-FACING context, but the injected
/// block is NEVER persisted into the returned (`new_messages`) history — the
/// core supplemental-not-persisted invariant, exercised end-to-end through
/// `run_agent_loop` (the unit tests only cover `pre_recall_block` formatting).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // current-thread test runtime; lock only serializes the global flag
async fn pre_recall_reaches_model_context_but_not_persisted_history() {
    use crate::agent::agent_loop::context_manager::set_verbatim_pre_recall;
    use crate::extras::memory_provider::MemoryProvider;
    let _guard = DIRTY_FLAG_TEST_LOCK.lock().unwrap();

    // Provider whose search returns a distinctive hit; empty snapshot so the
    // hit isn't de-duped against `<project_memory>`.
    struct RecallProvider;
    impl MemoryProvider for RecallProvider {
        fn name(&self) -> &str {
            "recall-stub"
        }
        fn format_for_system_prompt(&self) -> String {
            String::new()
        }
        fn view(&self, _t: &str) -> Value {
            serde_json::json!({})
        }
        fn add(&self, _: &str, _: &str, _: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }
        fn replace(&self, _: &str, _: &str, _: &str, _: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }
        fn remove(&self, _: &str, _: &str) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }
        fn search(&self, _q: &str) -> Result<Value, String> {
            Ok(serde_json::json!({
                "results": [{"id": "urn:ump:x", "content": "PRERECALLHIT: the widget cache lives in src/cache.rs"}]
            }))
        }
    }

    let ctx = empty_context();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let factory = capturing_factory(vec![text_response("done")], seen.clone());
    let provider: std::sync::Arc<dyn MemoryProvider> = std::sync::Arc::new(RecallProvider);

    // Pre-recall injects a `user`-role message; keep user/assistant/tool/system.
    let mut config = build_config();
    config.convert_to_llm = std::sync::Arc::new(|messages: &[Value]| {
        messages
            .iter()
            .filter(|m| {
                let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
                matches!(
                    role,
                    "user" | "assistant" | "tool" | "toolResult" | "system"
                )
            })
            .cloned()
            .collect()
    });

    set_verbatim_pre_recall(true);
    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let returned = run_agent_loop(
        vec![user("how do I cache the widget")],
        ctx,
        config,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        Some(provider),
    )
    .await;
    drop(tx);
    // Reset BEFORE asserting so a failure can't leak the toggle to other tests.
    set_verbatim_pre_recall(false);

    let snapshots = seen.lock().unwrap().clone();
    assert!(
        snapshots.iter().any(|s| s.contains("PRERECALLHIT")),
        "pre-recall hit must reach the model-facing context; snapshots={snapshots:?}",
    );
    let persisted = format!("{returned:?}");
    assert!(
        !persisted.contains("PRERECALLHIT"),
        "pre-recall block must NOT be persisted into new_messages: {persisted}",
    );
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
    super::run_compaction_pass(
        &mut ctx,
        &summarize_fn,
        5,
        0,
        &None,
        Some(&hooks),
        &tx,
        &empty_checkpoint_slot(),
        &mut 0,
        u64::MAX,
    )
    .await;
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
    super::run_compaction_pass(
        &mut ctx,
        &summarize_fn,
        5,
        0,
        &None,
        Some(&hooks),
        &tx,
        &empty_checkpoint_slot(),
        &mut 0,
        u64::MAX,
    )
    .await;
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
    super::run_compaction_pass(
        &mut ctx,
        &None,
        2,
        0,
        &None,
        None,
        &tx,
        &empty_checkpoint_slot(),
        &mut 0,
        u64::MAX,
    )
    .await;
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
    LoopMessage::User(UserMessage::text(text))
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
            LoopMessage::User(u) => Some(u.text_joined()),
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
        } => u.text_joined() == "interrupt",
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

/// dirge-j4dz: a graceful interjection raised DURING a run (e.g. the
/// permission-denial cascade) must halt the loop at the next tool-result
/// boundary. The stream here always returns a tool call, so without an
/// in-loop `is_interjected()` check the run would spin until `max_turns`.
/// With the fix it stops after the first turn.
#[tokio::test]
async fn interjection_halts_at_tool_result_boundary() {
    use crate::agent::agent_loop::stream::{LlmContext, StreamFn};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A tool that raises a graceful interjection when it runs — the same
    // signal the permission-denial cascade sets — then returns normally.
    #[derive(Debug)]
    struct InterjectingTool {
        signal: AbortSignal,
    }
    impl LoopTool for InterjectingTool {
        fn name(&self) -> &str {
            "noop"
        }
        fn description(&self) -> &str {
            "Interjecting"
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
            args: Value,
            _signal: AbortSignal,
            _on_update: super::super::tool::LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<super::super::LoopToolResult, String>> + Send + 'a>>
        {
            self.signal.interject();
            Box::pin(async move {
                Ok(super::super::LoopToolResult {
                    content: vec![serde_json::json!({"type": "text", "text": "ok"})],
                    details: args,
                    terminate: None,
                })
            })
        }
    }

    // Always returns a tool_use — a loop that ignores the interjection
    // would keep taking turns forever (bounded only by max_turns).
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = counter.clone();
    let factory: StreamFn = std::sync::Arc::new(move |_ctx: LlmContext, _opts| {
        counter.fetch_add(1, Ordering::SeqCst);
        let msg = tool_use_response("call-1", "noop", serde_json::json!({}));
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![
            crate::agent::agent_loop::message::StreamEvent::Done {
                reason,
                message: msg,
                usage: None,
            },
        ]))
    });

    let signal = AbortSignal::new();
    let mut ctx = empty_context();
    ctx.tools.push(PhaseSixArc::new(InterjectingTool {
        signal: signal.clone(),
    }));
    let mut cfg = build_config();
    cfg.tool_execution = ToolExecutionMode::Sequential;
    // A high cap so a spinning loop is clearly distinguishable from the
    // halt-after-one-turn behavior we want.
    cfg.max_turns = Some(25);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(256);
    let task = tokio::spawn(async move {
        run_agent_loop(
            vec![user("start")],
            ctx,
            cfg,
            signal,
            &tx,
            &factory,
            None,
            None,
        )
        .await
    });

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    assert!(
        result.is_ok(),
        "loop should exit promptly after interjection"
    );

    let turns = seen.load(Ordering::SeqCst);
    assert_eq!(
        turns, 1,
        "interjection must halt at the first tool-result boundary; the model took {turns} turns"
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

/// dirge-kk3x: a permission/approval denial is tagged `[DENIED]`, not the
/// generic `[ERROR]`, so the critic reads it as a policy wall (out of scope)
/// rather than a failure to demand the assistant fix.
#[test]
fn build_critic_transcript_marks_permission_denials_as_denied() {
    use crate::agent::agent_loop::message::ToolResultMessage;
    let msgs = vec![
        user("commit and push"),
        LoopMessage::ToolResult(ToolResultMessage {
            tool_call_id: "c1".to_string(),
            tool_name: "bash".to_string(),
            content: vec![ContentBlock::Text {
                text: "Permission denied: git is outside the project directory".to_string(),
            }],
            details: serde_json::json!({}),
            is_error: true,
        }),
        // A non-denial error keeps the generic ERROR tag.
        LoopMessage::ToolResult(ToolResultMessage {
            tool_call_id: "c2".to_string(),
            tool_name: "edit".to_string(),
            content: vec![ContentBlock::Text {
                text: "old_string not found".to_string(),
            }],
            details: serde_json::json!({}),
            is_error: true,
        }),
    ];
    let t = super::build_critic_transcript(&msgs);
    assert!(t.contains("TOOL bash [DENIED]: Permission denied"), "{t}");
    assert!(t.contains("TOOL edit [ERROR]: old_string not found"), "{t}");
}

/// dirge-kk3x regression: the [DENIED] tag is gated on `is_error`, mirroring
/// Outcome::classify. A SUCCESSFUL result whose text merely begins
/// "Permission denied" — e.g. bash returns Ok(text) for a failed `ssh` whose
/// output is "Permission denied (publickey).\nExit code: 255" — must NOT be
/// tagged [DENIED], or the critic would excuse genuinely unfinished work.
#[test]
fn build_critic_transcript_does_not_mark_successful_permission_denied_text() {
    use crate::agent::agent_loop::message::ToolResultMessage;
    let msgs = vec![
        user("ssh to the box and deploy"),
        LoopMessage::ToolResult(ToolResultMessage {
            tool_call_id: "c1".to_string(),
            tool_name: "bash".to_string(),
            content: vec![ContentBlock::Text {
                text: "Permission denied (publickey).\nExit code: 255".to_string(),
            }],
            details: serde_json::json!({}),
            // bash surfaces a failed command as a non-error result.
            is_error: false,
        }),
    ];
    let t = super::build_critic_transcript(&msgs);
    assert!(
        t.contains("TOOL bash [result]: Permission denied (publickey)."),
        "a non-error result must keep the [result] tag, not [DENIED]: {t}"
    );
    assert!(!t.contains("[DENIED]"), "{t}");
}

/// Regression (dirge-p9qm): in a long run the head is planning/scaffolding
/// and the implementation + verification land at the END. The builder used
/// to keep only the FIRST 8000 chars, so the critic was fed the planning and
/// never saw the work — wrongly reporting "nothing done". The transcript must
/// keep the original request (head) AND the most recent activity (tail).
#[test]
fn build_critic_transcript_keeps_request_and_recent_work_when_over_budget() {
    use crate::agent::agent_loop::message::ToolResultMessage;
    let mut msgs = vec![user("REQUEST: build an animated water canvas")];
    // Planning chatter that blows well past the budget.
    for i in 0..120 {
        msgs.push(LoopMessage::Assistant(AssistantMessage::new(
            vec![ContentBlock::Text {
                text: format!("planning step {i}: {}", "x".repeat(200)),
            }],
            StopReason::Stop,
        )));
    }
    // The actual work + verification, at the end of the run.
    msgs.push(LoopMessage::Assistant(AssistantMessage::new(
        vec![ContentBlock::Text {
            text: "DONE: created water.js + flowfield.js; tests 12/12 pass".to_string(),
        }],
        StopReason::Stop,
    )));
    msgs.push(LoopMessage::ToolResult(ToolResultMessage {
        tool_call_id: "v".to_string(),
        tool_name: "bash".to_string(),
        content: vec![ContentBlock::Text {
            text: "VERIFIED: WATER RENDERED (cyan/blue flow field)".to_string(),
        }],
        details: serde_json::json!({}),
        is_error: false,
    }));

    let t = super::build_critic_transcript(&msgs);
    assert!(
        t.contains("REQUEST: build an animated water canvas"),
        "original request (head) must survive truncation"
    );
    assert!(
        t.contains("WATER RENDERED"),
        "recent verification (tail) must survive — this is what the critic judges"
    );
    assert!(
        t.contains("tests 12/12 pass"),
        "recent work (tail) must survive"
    );
    assert!(
        t.contains("elided"),
        "an elision marker should mark the dropped middle"
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

/// Storm-breaker graceful failure: when the run gives up because
/// it's stuck looping the same call, it must surface a first-person
/// assistant explanation (not an empty/abrupt stop). Drives the loop
/// with the same single tool call repeated across turns: storm
/// suppresses it, the first all-suppressed turn injects the
/// self-correct nudge, and the next reaches the terminal branch —
/// which appends the failure narrative as an assistant message.
#[tokio::test]
async fn storm_terminal_emits_failure_narrative() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo.clone());

    // Five identical echo calls (distinct ids so each turn's results
    // pair cleanly). Default storm threshold is 3.
    let make = |i: usize| {
        AssistantMessage::new(
            vec![ContentBlock::ToolCall {
                id: format!("call-{i}"),
                name: "echo".to_string(),
                arguments: serde_json::json!({"v": 1}),
            }],
            StopReason::ToolUse,
        )
    };
    let factory = canned_factory((0..5).map(make).collect());

    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let config = build_config();
    let messages = run_agent_loop(
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

    let has_narrative = messages.iter().any(|m| match m {
        LoopMessage::Assistant(a) => a.content.iter().any(|b| match b {
            ContentBlock::Text { text } => text.contains("stopped here to avoid spinning"),
            _ => false,
        }),
        _ => false,
    });
    assert!(
        has_narrative,
        "expected a storm failure-narrative assistant message; got {} messages",
        messages.len()
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
        let outcome = super::run_compaction_pass(
            &mut ctx,
            &summarize_fn,
            5,
            failures,
            &None,
            None,
            &tx,
            &empty_checkpoint_slot(),
            &mut 0,
            u64::MAX,
        )
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
        &empty_checkpoint_slot(),
        &mut 0,
        u64::MAX,
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
        super::run_compaction_pass(
            &mut ctx,
            &summarize_fn,
            5,
            failures,
            &None,
            None,
            &tx,
            &empty_checkpoint_slot(),
            &mut 0,
            u64::MAX,
        )
        .await;
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
        LoopMessage::User(u) => u.text_joined(),
        _ => panic!("expected a user message"),
    };
    assert!(one.contains("1 unfinished todo "), "singular: {one}");
    let many = match todo_nudge_message(3) {
        LoopMessage::User(u) => u.text_joined(),
        _ => panic!("expected a user message"),
    };
    assert!(many.contains("3 unfinished todos "), "plural: {many}");
}

/// dirge-1g3v: the reviewer engages only on what THIS run changed. Given the
/// current working-tree diff and the run-start baseline, `run_delta_to_review`
/// yields the diff to review, or `None` to skip.
#[test]
fn run_delta_to_review_skips_when_unchanged() {
    use crate::agent::agent_loop::code_review::RunDiff;

    let wip = RunDiff {
        capped: "wip diff".to_string(),
        fingerprint: 1,
    };
    // Read-only turn over pre-existing WIP: identical diff → skip. Before the
    // dirge-1g3v gate, any ToolResult drove the judge on the whole dirty tree
    // even when the run touched nothing.
    assert_eq!(run_delta_to_review(Some(&wip), Some(&wip)), None);

    // Clean tree, nothing changed → nothing to review.
    assert_eq!(run_delta_to_review(None, None), None);

    // Agent created changes on a clean tree → review them.
    let new = RunDiff {
        capped: "new diff".to_string(),
        fingerprint: 2,
    };
    assert_eq!(run_delta_to_review(Some(&new), None), Some("new diff"));

    // Agent added to pre-existing WIP → the diff differs → review.
    let wip_more = RunDiff {
        capped: "wip + more".to_string(),
        fingerprint: 3,
    };
    assert_eq!(
        run_delta_to_review(Some(&wip_more), Some(&wip)),
        Some("wip + more")
    );

    // Agent reverted the WIP back to clean → no current diff → nothing to review.
    assert_eq!(run_delta_to_review(None, Some(&wip)), None);
}

/// dirge-8gdv: the skip decision must compare the UNcapped fingerprints, not
/// the size-capped text. When pre-existing WIP already exceeds MAX_DIFF_BYTES,
/// a length-preserving edit landing PAST the cap leaves the two CAPPED strings
/// byte-identical, so the old capped-string comparison saw no change and
/// skipped the reviewer. Two diffs with identical capped text but different
/// fingerprints must be seen as CHANGED.
#[test]
fn run_delta_to_review_engages_when_capped_identical_but_fingerprint_differs() {
    use crate::agent::agent_loop::code_review::RunDiff;

    let capped = "identical capped diff text".to_string();
    let baseline = RunDiff {
        capped: capped.clone(),
        fingerprint: 1,
    };
    let current = RunDiff {
        capped: capped.clone(),
        fingerprint: 2,
    };

    // The bug's premise: the capped text the reviewer would see is identical.
    assert_eq!(baseline.capped, current.capped);
    // …but the fingerprints differ, so the reviewer engages (not skipped).
    assert_eq!(
        run_delta_to_review(Some(&current), Some(&baseline)),
        Some(capped.as_str())
    );
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
                crate::agent::agent_loop::message::UserMessage::text("hook follow-up"),
            )]
        })
    }));

    let mut critic_done = false;
    let mut goal_reacts = 0u8;
    let mut todo_nudges = 0u8;
    let mut resume_nudges: u8 = 0;
    let mut code_review_reacts = 0u8;
    let (review_emit, _review_emit_rx) = tokio::sync::mpsc::channel(64);
    let (msgs, source) = poll_finalization_follow_up(
        &config,
        "sys",
        &[],
        &mut critic_done,
        &mut code_review_reacts,
        None, // code_review_baseline
        &mut std::collections::HashSet::new(),
        &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        &mut goal_reacts,
        &mut todo_nudges,
        &mut resume_nudges,
        GateMode::Off,
        None,
        None,
        &mut 0u8,
        &review_emit,
    )
    .await;

    assert_eq!(source, FollowUpSource::Hook);
    assert_eq!(msgs.len(), 1);
    assert!(
        !critic_done,
        "hook must short-circuit before the critic runs"
    );
    assert_eq!(todo_nudges, 0, "todo gate must not be reached");
}

// ── dirge-jo0o: code-review react counting + advisory dedup ──────────

fn review_finding(
    severity: crate::agent::agent_loop::code_review::Severity,
    body: &str,
) -> crate::agent::agent_loop::code_review::Finding {
    crate::agent::agent_loop::code_review::Finding {
        severity,
        location: None,
        body: body.to_string(),
    }
}

/// An advisory-only (medium/low) review finalizes; it must NOT spend the
/// bounded react budget — only a stubborn blocking finding should.
#[test]
fn advisory_only_review_does_not_count_against_budget() {
    use crate::agent::agent_loop::code_review::Severity;
    let mut seen = std::collections::HashSet::new();
    let r = decide_review_reaction(vec![review_finding(Severity::Low, "nit")], &mut seen);
    assert!(r.advisory_to_emit.is_some(), "advisory surfaced");
    assert!(r.blocking_followup.is_none(), "nothing blocking");
    assert!(
        !r.counts_against_budget,
        "advisory-only review must not burn the react budget"
    );
}

/// A blocking (high/critical) review re-enters the loop and spends one unit
/// of the react budget.
#[test]
fn blocking_review_counts_against_budget() {
    use crate::agent::agent_loop::code_review::Severity;
    let mut seen = std::collections::HashSet::new();
    let r = decide_review_reaction(vec![review_finding(Severity::High, "SQLi")], &mut seen);
    assert!(r.blocking_followup.is_some(), "high-severity blocks");
    assert!(r.counts_against_budget, "blocking finding counts");
}

/// The same advisory text is emitted once, not re-posted every finalization.
#[test]
fn repeat_advisory_is_deduped() {
    use crate::agent::agent_loop::code_review::Severity;
    let mut seen = std::collections::HashSet::new();
    let first = decide_review_reaction(
        vec![review_finding(Severity::Medium, "same note")],
        &mut seen,
    );
    assert!(first.advisory_to_emit.is_some(), "first advisory emits");
    let second = decide_review_reaction(
        vec![review_finding(Severity::Medium, "same note")],
        &mut seen,
    );
    assert!(
        second.advisory_to_emit.is_none(),
        "identical advisory must not re-emit"
    );
    assert!(!second.counts_against_budget);
}

/// Mixed review: emit the advisory (first time), re-enter on the blocking
/// finding, and count it.
#[test]
fn mixed_review_emits_advisory_and_blocks_and_counts() {
    use crate::agent::agent_loop::code_review::Severity;
    let mut seen = std::collections::HashSet::new();
    let r = decide_review_reaction(
        vec![
            review_finding(Severity::High, "SQLi"),
            review_finding(Severity::Low, "nit"),
        ],
        &mut seen,
    );
    assert!(r.advisory_to_emit.is_some(), "low surfaced as advisory");
    assert!(r.blocking_followup.is_some(), "high blocks");
    assert!(r.counts_against_budget);
}

/// With no hook/verifier/critic and the todo gate exhausted, the authority
/// reports `None` so the run finalizes. (`todo_nudges = MAX` keeps this
/// deterministic regardless of the process-global todo list.)
#[tokio::test]
async fn finalization_all_gates_silent_yields_none() {
    let config = build_config(); // hook/verifier/critic all None
    let mut critic_done = false;
    let mut goal_reacts = 0u8;
    let mut todo_nudges = MAX_TODO_NUDGES; // todo gate bounded out
    let mut resume_nudges: u8 = 0;
    let mut code_review_reacts = 0u8;
    let (review_emit, _review_emit_rx) = tokio::sync::mpsc::channel(64);
    let (msgs, source) = poll_finalization_follow_up(
        &config,
        "sys",
        &[],
        &mut critic_done,
        &mut code_review_reacts,
        None, // code_review_baseline
        &mut std::collections::HashSet::new(),
        &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        &mut goal_reacts,
        &mut todo_nudges,
        &mut resume_nudges,
        GateMode::Off,
        None,
        None,
        &mut 0u8,
        &review_emit,
    )
    .await;

    assert!(msgs.is_empty());
    assert_eq!(source, FollowUpSource::None);
}

/// Goal gate: an unmet stop condition re-enters the loop and counts the
/// re-entry. `critic_done = true` isolates the goal gate from the
/// one-shot critic so we observe it specifically.
#[tokio::test]
async fn finalization_goal_unmet_reenters_and_counts() {
    use crate::agent::agent_loop::critic::CriticFn;
    let mut config = build_config();
    config.goal = Some("all tests pass and committed".into());
    let judge: CriticFn =
        Arc::new(|_p| Box::pin(async { Ok("GOAL: UNMET\n- tests still failing".to_string()) }));
    config.goal_fn = Some(judge);

    let mut critic_done = true; // skip the one-shot critic
    let mut goal_reacts = 0u8;
    let mut todo_nudges = MAX_TODO_NUDGES;
    let mut resume_nudges: u8 = 0;
    let mut code_review_reacts = 0u8;
    let (review_emit, _review_emit_rx) = tokio::sync::mpsc::channel(64);
    let (msgs, source) = poll_finalization_follow_up(
        &config,
        "sys",
        &[],
        &mut critic_done,
        &mut code_review_reacts,
        None, // code_review_baseline
        &mut std::collections::HashSet::new(),
        &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        &mut goal_reacts,
        &mut todo_nudges,
        &mut resume_nudges,
        GateMode::Off,
        None,
        None,
        &mut 0u8,
        &review_emit,
    )
    .await;

    assert_eq!(source, FollowUpSource::Goal);
    assert_eq!(goal_reacts, 1, "an unmet goal counts one re-entry");
    assert_eq!(msgs.len(), 1);
}

/// A met goal lets the run finalize and does NOT count a re-entry.
#[tokio::test]
async fn finalization_goal_met_finalizes() {
    use crate::agent::agent_loop::critic::CriticFn;
    let mut config = build_config();
    config.goal = Some("all tests pass".into());
    let judge: CriticFn = Arc::new(|_p| Box::pin(async { Ok("GOAL: MET".to_string()) }));
    config.goal_fn = Some(judge);

    let mut critic_done = true;
    let mut goal_reacts = 0u8;
    let mut todo_nudges = MAX_TODO_NUDGES;
    let mut resume_nudges: u8 = 0;
    let mut code_review_reacts = 0u8;
    let (review_emit, _review_emit_rx) = tokio::sync::mpsc::channel(64);
    let (msgs, source) = poll_finalization_follow_up(
        &config,
        "sys",
        &[],
        &mut critic_done,
        &mut code_review_reacts,
        None, // code_review_baseline
        &mut std::collections::HashSet::new(),
        &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        &mut goal_reacts,
        &mut todo_nudges,
        &mut resume_nudges,
        GateMode::Off,
        None,
        None,
        &mut 0u8,
        &review_emit,
    )
    .await;

    assert!(msgs.is_empty());
    assert_eq!(source, FollowUpSource::None);
    assert_eq!(goal_reacts, 0);
}

/// Once the re-entry bound is reached, an unmet goal no longer blocks —
/// the run finalizes rather than looping forever on a bad stop condition.
#[tokio::test]
async fn finalization_goal_bound_stops_reentry() {
    use crate::agent::agent_loop::critic::CriticFn;
    let mut config = build_config();
    config.goal = Some("unsatisfiable".into());
    let judge: CriticFn = Arc::new(|_p| Box::pin(async { Ok("GOAL: UNMET".to_string()) }));
    config.goal_fn = Some(judge);

    let mut critic_done = true;
    let mut goal_reacts = crate::agent::agent_loop::goal::MAX_GOAL_REACT;
    let mut todo_nudges = MAX_TODO_NUDGES;
    let mut resume_nudges: u8 = 0;
    let mut code_review_reacts = 0u8;
    let (review_emit, _review_emit_rx) = tokio::sync::mpsc::channel(64);
    let (msgs, source) = poll_finalization_follow_up(
        &config,
        "sys",
        &[],
        &mut critic_done,
        &mut code_review_reacts,
        None, // code_review_baseline
        &mut std::collections::HashSet::new(),
        &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        &mut goal_reacts,
        &mut todo_nudges,
        &mut resume_nudges,
        GateMode::Off,
        None,
        None,
        &mut 0u8,
        &review_emit,
    )
    .await;

    assert!(msgs.is_empty());
    assert_eq!(source, FollowUpSource::None, "bound reached → finalize");
}

/// Goal gate stays OFF when no judge (`goal_fn`) is configured, even with
/// a goal set.
#[tokio::test]
async fn finalization_goal_without_judge_is_inert() {
    let mut config = build_config();
    config.goal = Some("all tests pass".into());
    config.goal_fn = None; // no judge

    let mut critic_done = true;
    let mut goal_reacts = 0u8;
    let mut todo_nudges = MAX_TODO_NUDGES;
    let mut resume_nudges: u8 = 0;
    let mut code_review_reacts = 0u8;
    let (review_emit, _review_emit_rx) = tokio::sync::mpsc::channel(64);
    let (msgs, source) = poll_finalization_follow_up(
        &config,
        "sys",
        &[],
        &mut critic_done,
        &mut code_review_reacts,
        None, // code_review_baseline
        &mut std::collections::HashSet::new(),
        &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        &mut goal_reacts,
        &mut todo_nudges,
        &mut resume_nudges,
        GateMode::Off,
        None,
        None,
        &mut 0u8,
        &review_emit,
    )
    .await;

    assert!(msgs.is_empty());
    assert_eq!(source, FollowUpSource::None);
    assert_eq!(goal_reacts, 0);
}

/// Open-issues gate Off → inert (FollowUpSource::None).
#[tokio::test]
async fn open_issues_gate_off_is_inert() {
    let config = build_config();
    let mut critic_done = true;
    let mut goal_reacts = 0u8;
    let mut todo_nudges = MAX_TODO_NUDGES;
    let mut resume_nudges: u8 = 0;
    let mut open_issues_nudges: u8 = 0;
    let mut code_review_reacts = 0u8;
    let (review_emit, _review_emit_rx) = tokio::sync::mpsc::channel(64);

    // Create a temp DB with open issues for this session.
    let dir = temp_dir("open-issues-off");
    let db_path = dir.join("state.db");
    let store = crate::extras::issue_db::IssueStore::open_at(&db_path).unwrap();
    let sid = "open-issues-off-sess";
    store
        .create("wire up telemetry", "", None, Some(sid))
        .unwrap();

    let (msgs, source) = poll_finalization_follow_up(
        &config,
        "sys",
        &[],
        &mut critic_done,
        &mut code_review_reacts,
        None,
        &mut std::collections::HashSet::new(),
        &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        &mut goal_reacts,
        &mut todo_nudges,
        &mut resume_nudges,
        GateMode::Off, // opt-out
        Some(db_path.as_path()),
        Some(sid),
        &mut open_issues_nudges,
        &review_emit,
    )
    .await;

    assert!(msgs.is_empty(), "Off mode should be inert");
    assert_eq!(source, FollowUpSource::None);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Open-issues gate blocking with N session issues open → returns a
/// `[open-issues]` nudge (FollowUpSource::OpenIssues) listing titles.
#[tokio::test]
async fn open_issues_gate_blocking_with_session_open_issues_nudges() {
    use crate::agent::agent_loop::run::OPEN_ISSUES_NUDGE_TAG;
    let config = build_config();
    let mut critic_done = true;
    let mut goal_reacts = 0u8;
    let mut todo_nudges = MAX_TODO_NUDGES;
    let mut resume_nudges: u8 = 0;
    let mut open_issues_nudges: u8 = 0;
    let mut code_review_reacts = 0u8;
    let (review_emit, _review_emit_rx) = tokio::sync::mpsc::channel(64);

    let dir = temp_dir("open-issues-blocking");
    let db_path = dir.join("state.db");
    let store = crate::extras::issue_db::IssueStore::open_at(&db_path).unwrap();
    let sid = "open-issues-blocking-sess";
    store
        .create("wire up telemetry", "", None, Some(sid))
        .unwrap();
    store
        .create("add metrics dashboard", "", None, Some(sid))
        .unwrap();

    let (msgs, source) = poll_finalization_follow_up(
        &config,
        "sys",
        &[],
        &mut critic_done,
        &mut code_review_reacts,
        None,
        &mut std::collections::HashSet::new(),
        &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        &mut goal_reacts,
        &mut todo_nudges,
        &mut resume_nudges,
        GateMode::Blocking,
        Some(db_path.as_path()),
        Some(sid),
        &mut open_issues_nudges,
        &review_emit,
    )
    .await;

    assert_eq!(source, FollowUpSource::OpenIssues);
    assert_eq!(open_issues_nudges, 1);
    assert_eq!(msgs.len(), 1);
    let content = match &msgs[0] {
        LoopMessage::User(u) => u.text_joined(),
        _ => panic!("expected User message"),
    };
    assert!(
        content.starts_with(OPEN_ISSUES_NUDGE_TAG),
        "expected [open-issues] tag, got: {content}"
    );
    assert!(content.contains("wire up telemetry"), "{content}");
    assert!(content.contains("add metrics dashboard"), "{content}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Blocking bound stops re-entry after MAX_OPEN_ISSUES_NUDGES.
#[tokio::test]
async fn open_issues_gate_blocking_has_bound() {
    let config = build_config();
    let mut critic_done = true;
    let mut goal_reacts = 0u8;
    let mut todo_nudges = MAX_TODO_NUDGES;
    let mut resume_nudges: u8 = 0;
    let mut open_issues_nudges: u8 = MAX_OPEN_ISSUES_NUDGES; // already at bound
    let mut code_review_reacts = 0u8;
    let (review_emit, _review_emit_rx) = tokio::sync::mpsc::channel(64);

    let dir = temp_dir("open-issues-bound");
    let db_path = dir.join("state.db");
    let store = crate::extras::issue_db::IssueStore::open_at(&db_path).unwrap();
    let sid = "open-issues-bound-sess";
    store
        .create("wire up telemetry", "", None, Some(sid))
        .unwrap();

    let (msgs, source) = poll_finalization_follow_up(
        &config,
        "sys",
        &[],
        &mut critic_done,
        &mut code_review_reacts,
        None,
        &mut std::collections::HashSet::new(),
        &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        &mut goal_reacts,
        &mut todo_nudges,
        &mut resume_nudges,
        GateMode::Blocking,
        Some(db_path.as_path()),
        Some(sid),
        &mut open_issues_nudges,
        &review_emit,
    )
    .await;

    assert!(msgs.is_empty(), "bounded gate should be inert");
    assert_eq!(source, FollowUpSource::None);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Zero open session issues → inert (FollowUpSource::None).
#[tokio::test]
async fn open_issues_gate_zero_open_session_issues_is_inert() {
    let config = build_config();
    let mut critic_done = true;
    let mut goal_reacts = 0u8;
    let mut todo_nudges = MAX_TODO_NUDGES;
    let mut resume_nudges: u8 = 0;
    let mut open_issues_nudges: u8 = 0;
    let mut code_review_reacts = 0u8;
    let (review_emit, _review_emit_rx) = tokio::sync::mpsc::channel(64);

    let dir = temp_dir("open-issues-zero");
    let db_path = dir.join("state.db");
    let _store = crate::extras::issue_db::IssueStore::open_at(&db_path).unwrap();
    // No issues for this session.
    let sid = "open-issues-zero-sess";

    let (msgs, source) = poll_finalization_follow_up(
        &config,
        "sys",
        &[],
        &mut critic_done,
        &mut code_review_reacts,
        None,
        &mut std::collections::HashSet::new(),
        &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        &mut goal_reacts,
        &mut todo_nudges,
        &mut resume_nudges,
        GateMode::Blocking,
        Some(db_path.as_path()),
        Some(sid),
        &mut open_issues_nudges,
        &review_emit,
    )
    .await;

    assert!(msgs.is_empty(), "zero open issues should be inert");
    assert_eq!(source, FollowUpSource::None);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Missing db → inert (fail-open).
#[tokio::test]
async fn open_issues_gate_missing_db_is_inert() {
    let config = build_config();
    let mut critic_done = true;
    let mut goal_reacts = 0u8;
    let mut todo_nudges = MAX_TODO_NUDGES;
    let mut resume_nudges: u8 = 0;
    let mut open_issues_nudges: u8 = 0;
    let mut code_review_reacts = 0u8;
    let (review_emit, _review_emit_rx) = tokio::sync::mpsc::channel(64);

    let (msgs, source) = poll_finalization_follow_up(
        &config,
        "sys",
        &[],
        &mut critic_done,
        &mut code_review_reacts,
        None,
        &mut std::collections::HashSet::new(),
        &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        &mut goal_reacts,
        &mut todo_nudges,
        &mut resume_nudges,
        GateMode::Blocking,
        None, // no db
        Some("some-sess"),
        &mut open_issues_nudges,
        &review_emit,
    )
    .await;

    assert!(msgs.is_empty(), "missing db should be inert (fail-open)");
    assert_eq!(source, FollowUpSource::None);
}

/// Advisory mode emits a SystemNotice when issues are open but does
/// NOT re-enter the loop.
#[tokio::test]
async fn open_issues_gate_advisory_emits_notice_but_does_not_reenter() {
    let config = build_config();
    let mut critic_done = true;
    let mut goal_reacts = 0u8;
    let mut todo_nudges = MAX_TODO_NUDGES;
    let mut resume_nudges: u8 = 0;
    let mut open_issues_nudges: u8 = 0;
    let mut code_review_reacts = 0u8;
    let (review_emit, mut review_emit_rx) = tokio::sync::mpsc::channel(64);

    let dir = temp_dir("open-issues-advisory");
    let db_path = dir.join("state.db");
    let store = crate::extras::issue_db::IssueStore::open_at(&db_path).unwrap();
    let sid = "open-issues-advisory-sess";
    store
        .create("wire up telemetry", "", None, Some(sid))
        .unwrap();

    let (msgs, source) = poll_finalization_follow_up(
        &config,
        "sys",
        &[],
        &mut critic_done,
        &mut code_review_reacts,
        None,
        &mut std::collections::HashSet::new(),
        &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        &mut goal_reacts,
        &mut todo_nudges,
        &mut resume_nudges,
        GateMode::Advisory,
        Some(db_path.as_path()),
        Some(sid),
        &mut open_issues_nudges,
        &review_emit,
    )
    .await;

    // Advisory does NOT re-enter (returns empty messages).
    assert!(msgs.is_empty(), "advisory should not re-enter");
    assert_eq!(source, FollowUpSource::None);
    assert_eq!(open_issues_nudges, 1, "counts the advisory");

    // Check that a SystemNotice was emitted.
    match review_emit_rx.try_recv() {
        Ok(crate::agent::agent_loop::message::LoopEvent::SystemNotice { content }) => {
            assert!(
                content.contains("issue(s) from this session are still open"),
                "{content}"
            );
        }
        other => panic!("expected SystemNotice, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

fn temp_dir(suffix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dirge-ksjl-{}-{}-{suffix}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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
        LoopMessage::User(u) => {
            let t = u.text_joined();
            if t.contains("[Recovery checkpoint]") {
                Some(t)
            } else {
                None
            }
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
            LoopMessage::User(u) if u.text_joined().contains("[Recovery checkpoint]")
        )),
        "a success between failures must reset the streak"
    );
}

// dirge-x6yi: the turn-start issue-board reminder does synchronous rusqlite
// I/O (open + query). run_agent_loop now hands it to spawn_blocking so a
// contended/locked state.db can't stall the loop task. The extracted reader
// keeps the same behavior — a real board yields the reminder, a missing db
// yields None without panicking.
#[test]
fn issue_board_reminder_block_reads_board_and_tolerates_missing_db() {
    let dir = std::env::temp_dir().join(format!(
        "dirge-x6yi-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("state.db");

    let store = crate::extras::issue_db::IssueStore::open_at(&db_path).unwrap();
    store.create("wire up telemetry", "", None, None).unwrap();

    let block =
        super::issue_board_reminder_block(&db_path).expect("a non-empty board yields a reminder");
    assert!(block.contains("wire up telemetry"), "{block}");

    // Missing db → best-effort None, no panic.
    assert!(super::issue_board_reminder_block(&dir.join("nope.db")).is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

// ── last_action_failed_and_stopped ──────────────────────────────────────

/// Helper: construct a `LoopMessage::ToolResult` for test use.
fn tool_err(id: &str, name: &str, is_error: bool) -> LoopMessage {
    LoopMessage::ToolResult(crate::agent::agent_loop::message::ToolResultMessage {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        content: vec![crate::agent::agent_loop::message::ContentBlock::Text {
            text: "error output".to_string(),
        }],
        details: serde_json::json!({}),
        is_error,
    })
}

/// Like `tool_err` but with caller-controlled result text — lets a test feed
/// a permission-denial excerpt or a storm-breaker backfill stub verbatim
/// (dirge-g3xv).
fn tool_err_text(id: &str, name: &str, is_error: bool, text: &str) -> LoopMessage {
    LoopMessage::ToolResult(crate::agent::agent_loop::message::ToolResultMessage {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        content: vec![crate::agent::agent_loop::message::ContentBlock::Text {
            text: text.to_string(),
        }],
        details: serde_json::json!({}),
        is_error,
    })
}

fn asst_no_tools(text: &str) -> LoopMessage {
    LoopMessage::Assistant(crate::agent::agent_loop::message::AssistantMessage::new(
        vec![crate::agent::agent_loop::message::ContentBlock::Text {
            text: text.to_string(),
        }],
        crate::agent::agent_loop::message::StopReason::Stop,
    ))
}

fn asst_with_tool(id: &str, name: &str, args: serde_json::Value) -> LoopMessage {
    LoopMessage::Assistant(crate::agent::agent_loop::message::AssistantMessage::new(
        vec![crate::agent::agent_loop::message::ContentBlock::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
        }],
        crate::agent::agent_loop::message::StopReason::ToolUse,
    ))
}

#[test]
fn last_action_failed_and_stopped_true_on_error_tool_then_text() {
    // Tail: ToolResult(is_error=true), Assistant(no tool calls)
    let msgs = vec![
        user("do it"),
        asst_with_tool("c1", "read", serde_json::json!({"path": "/x"})),
        tool_err("c1", "read", true),
        asst_no_tools("failed, let me stop"),
    ];
    assert!(last_action_failed_and_stopped(&msgs));
}

#[test]
fn last_action_failed_and_stopped_false_when_all_tool_results_ok() {
    // Tail: ToolResult(is_error=false), Assistant(no tools)
    let msgs = vec![
        user("do it"),
        asst_with_tool("c1", "read", serde_json::json!({"path": "/x"})),
        tool_err("c1", "read", false),
        asst_no_tools("done"),
    ];
    assert!(!last_action_failed_and_stopped(&msgs));
}

#[test]
fn last_action_failed_and_stopped_false_when_no_tool_result_before_final_assistant() {
    // Tail: Assistant(text), Assistant(text) — the anti-loop / model-replied-to-nudge case.
    let msgs = vec![
        user("go"),
        asst_with_tool("c1", "read", serde_json::json!({"path": "/x"})),
        tool_err("c1", "read", true),
        asst_no_tools("nudged reply 1"),
        asst_no_tools("nudged reply 2"),
    ];
    assert!(!last_action_failed_and_stopped(&msgs));
}

#[test]
fn last_action_failed_and_stopped_false_when_last_assistant_has_tool_calls() {
    let msgs = vec![
        user("go"),
        tool_err("c1", "read", true),
        asst_with_tool("c2", "write", serde_json::json!({"path": "/y"})),
    ];
    assert!(!last_action_failed_and_stopped(&msgs));
}

#[test]
fn last_action_failed_and_stopped_false_when_last_is_not_assistant() {
    let msgs = vec![
        user("go"),
        asst_with_tool("c1", "read", serde_json::json!({})),
        tool_err("c1", "read", true),
    ];
    assert!(!last_action_failed_and_stopped(&msgs));
}

#[test]
fn last_action_failed_and_stopped_false_on_empty() {
    let msgs: Vec<LoopMessage> = vec![];
    assert!(!last_action_failed_and_stopped(&msgs));
}

#[test]
fn last_action_failed_and_stopped_detects_error_among_mixed_results() {
    // Multiple ToolResults, one success then one error.
    let msgs = vec![
        user("go"),
        asst_with_tool("c1", "read", serde_json::json!({"path": "/a"})),
        tool_err("c1", "read", false),
        asst_with_tool("c2", "write", serde_json::json!({"path": "/b"})),
        tool_err("c2", "write", true),
        asst_no_tools("write failed, stopping"),
    ];
    assert!(last_action_failed_and_stopped(&msgs));
}

// dirge-g3xv: permission denials and storm-breaker backfill stubs are NOT
// retryable — the resume-after-failure gate must not arm for them.
#[test]
fn last_action_failed_and_stopped_false_on_permission_denial() {
    // A permission/approval refusal is only unblockable by the user; re-issuing
    // re-prompts. The gate must not arm (RED currently: true).
    let msgs = vec![
        user("go"),
        asst_with_tool("c1", "bash", serde_json::json!({})),
        tool_err_text("c1", "bash", true, "Permission denied by user"),
        asst_no_tools("you denied it"),
    ];
    assert!(!last_action_failed_and_stopped(&msgs));
}

#[test]
fn last_action_failed_and_stopped_false_on_suppressed_backfill_stub() {
    // The backfill stub literally means "do NOT repeat". Re-issuing re-triggers
    // the suppressed call, so the gate must not arm (RED currently: true).
    let msgs = vec![
        user("go"),
        asst_with_tool("c1", "bash", serde_json::json!({})),
        tool_err_text(
            "c1",
            "bash",
            true,
            crate::agent::agent_loop::tools::SUPPRESSED_CALL_NOTE,
        ),
        asst_no_tools("ok, stopping"),
    ];
    assert!(!last_action_failed_and_stopped(&msgs));
}

#[test]
fn last_action_failed_and_stopped_true_on_genuine_error() {
    // A genuine, mechanically-recoverable failure (bad edit args) still arms the
    // gate. Regression guard — should already pass and keep passing.
    let msgs = vec![
        user("go"),
        asst_with_tool("c1", "edit", serde_json::json!({})),
        tool_err_text("c1", "edit", true, "old_string not found in file"),
        asst_no_tools("gave up"),
    ];
    assert!(last_action_failed_and_stopped(&msgs));
}

#[test]
fn last_action_failed_and_stopped_true_when_mixed_denial_and_genuine() {
    // A denial result AND a genuine-error result in the same tail: a real
    // retryable failure is still present, so the gate arms.
    let msgs = vec![
        user("go"),
        asst_with_tool("c1", "bash", serde_json::json!({})),
        tool_err_text("c1", "bash", true, "Permission denied by user"),
        asst_with_tool("c2", "edit", serde_json::json!({})),
        tool_err_text("c2", "edit", true, "old_string not found in file"),
        asst_no_tools("stopping"),
    ];
    assert!(last_action_failed_and_stopped(&msgs));
}

// ── bounded resume counter (MAX_RESUME_NUDGE) ───────────────────────────

#[test]
fn last_action_failed_and_stopped_bounded() {
    // When resume_nudges is already at MAX_RESUME_NUDGE, the gate must not fire.
    let msgs = vec![
        user("do it"),
        asst_with_tool("c1", "read", serde_json::json!({"path": "/x"})),
        tool_err("c1", "read", true),
        asst_no_tools("failed"),
    ];
    let resume_nudges = MAX_RESUME_NUDGE;
    assert!(!(resume_nudges < MAX_RESUME_NUDGE && last_action_failed_and_stopped(&msgs)));
}
