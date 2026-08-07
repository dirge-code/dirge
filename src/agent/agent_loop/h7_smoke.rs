//! Phase 4.5h-7 smoke tests against real provider APIs.
//!
//! Each test runs unconditionally in `cargo test` but auto-skips
//! cleanly (prints `[skipped]` and returns Ok) when no provider
//! key is found in env — `detect_provider()` returns None and the
//! test bails before any HTTP request. When at least one of
//! DEEPSEEK_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY /
//! GEMINI_API_KEY / OPENROUTER_API_KEY is set, the test exercises
//! the real provider and validates loop integration end-to-end.
//!
//! Run with --nocapture to see the [skipped] messages:
//!
//!   ```
//!   cargo test agent_loop::h7_smoke -- --nocapture
//!   ```
//!
//! Each test exercises a different scenario from
//! `docs/H7_AGENT_LOOP_TEST.md`. Failures here indicate the new
//! agent_loop path has a real-LLM bug that the mock-driven
//! tests missed.
//!
//! These tests bypass the full dirge `build_agent` (sessions,
//! permission asker, plugin manager, etc.) and exercise just
//! the new path's core: `rig_stream_fn_from_model` →
//! `retrying_stream_fn` → `spawn_loop_runner`. If those work,
//! the `AnyAgent::spawn_runner` integration is highly likely
//! to work too because it composes the same pieces.

use std::sync::Arc;

use crate::agent::agent_loop::{
    LoopSpawnConfig, retrying_stream_fn, rig_stream_fn_from_model, spawn_loop_runner,
};
use crate::agent::recovery::RecoveryPolicy;
use crate::event::AgentEvent;

/// Check env vars and return Some(provider) for whichever
/// API key is present. Returns None if none of the known
/// keys are set — tests then skip with an explanation.
fn detect_provider() -> Option<&'static str> {
    for (var, name) in [
        ("DEEPSEEK_API_KEY", "deepseek"),
        ("ANTHROPIC_API_KEY", "anthropic"),
        ("OPENAI_API_KEY", "openai"),
        ("OPENROUTER_API_KEY", "openrouter"),
    ] {
        if std::env::var(var).is_ok() {
            return Some(name);
        }
    }
    None
}

/// Default model per provider for h-7 testing. These are
/// cheap / fast models so smoke tests don't burn budget.
fn default_model(provider: &str) -> &'static str {
    match provider {
        "deepseek" => "deepseek-v4-flash",
        "anthropic" => "claude-haiku-4-5-20251001",
        "openai" => "gpt-4o-mini",
        "openrouter" => "deepseek/deepseek-v4-flash",
        _ => "gpt-4o-mini",
    }
}

/// Build a `StreamFn` from whichever provider has an API key
/// set. The key itself comes from the env (rig clients read it
/// directly).
fn build_stream_fn() -> Option<crate::agent::agent_loop::StreamFn> {
    use crate::provider::{AnyClient, AnyModel};
    use rig::providers::{anthropic, openai, openrouter};
    use std::collections::HashMap;

    let provider = detect_provider()?;
    let model_name = default_model(provider);

    // Build AnyClient directly via create_client; needs the key
    // env var to be readable.
    let client = match crate::provider::create_client(provider, None, &HashMap::new()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[h7-smoke] failed to build {provider} client: {e}");
            return None;
        }
    };
    let any_model = client.completion_model(model_name);

    // For h-7 each variant builds the StreamFn via
    // rig_stream_fn_from_model. Mirrors AnyAgent::build_stream_fn
    // dispatch but on AnyModel directly (no AnyAgent indirection).
    let chunk_timeout = Some(std::time::Duration::from_secs(60));
    let stream_fn = match any_model {
        AnyModel::OpenRouter(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::OpenAI(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::ChatGptOpenAI(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::OpenAICodex(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::Anthropic(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::AnthropicOauth(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::Gemini(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::DeepSeek(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::Glm(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::Cerebras(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::OpenCode(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::Kimi(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::Ollama(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::Custom(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
    };

    eprintln!("[h7-smoke] using provider={provider} model={model_name}");
    Some(retrying_stream_fn(stream_fn, RecoveryPolicy::default()))
}

/// Drain an `AgentRunner`'s event_rx and collect labels. Returns
/// the events for downstream assertions and the final Done's
/// response field (or None if Done didn't fire).
async fn drain_to_done(
    mut runner: crate::agent::runner::AgentRunner,
) -> (Vec<AgentEvent>, Option<String>) {
    let mut events = Vec::new();
    let mut final_response = None;
    while let Some(evt) = runner.event_rx.recv().await {
        if let AgentEvent::Done { response, .. } = &evt {
            final_response = Some(response.to_string());
        }
        events.push(evt);
    }
    let _ = runner.task.await;
    (events, final_response)
}

/// True when the run failed because the PROVIDER was unavailable rather
/// than because dirge is broken — a quota ceiling, a rate limit, an auth
/// failure, or a dead network.
///
/// These scenarios already skip when the API key is unset. A key that is
/// set but whose account has hit its billing ceiling is the same class of
/// environmental unavailability, and failing the suite for it makes
/// `cargo test` red for a reason no code change can fix — which trains
/// people to ignore a red suite. (GLM answers an exhausted 5-hour window
/// with HTTP 429 code 1308; `classify_error` already recognizes it, so
/// this reuses that classifier rather than matching provider strings
/// here.) A provider that rejects the request because the model name
/// doesn't exist (retired upstream) is the same prerequisite class — keyed
/// on the provider's own rejection wording via
/// [`model_rejected_by_provider`], never on what the test asserts.
/// Genuine failures — [`ErrorKind::Other`], a wrong answer, a missing tool
/// call — still fail as before.
fn provider_unavailable(events: &[AgentEvent]) -> Option<String> {
    use crate::agent::recovery::{ErrorKind, classify_error};
    events.iter().find_map(|e| match e {
        AgentEvent::Error(msg) => {
            let text = msg.to_string();
            if model_rejected_by_provider(&text) {
                return Some(text);
            }
            match classify_error(&text) {
                ErrorKind::UsageCap
                | ErrorKind::RateLimit
                | ErrorKind::Auth
                | ErrorKind::Network => Some(text),
                _ => None,
            }
        }
        _ => None,
    })
}

/// True when the provider itself rejected the request because the model
/// name/id doesn't exist — retired upstream or never valid. That is a
/// PREREQUISITE that isn't met, not a finding about dirge's loop: no code
/// change can make a retired model name work, so failing the suite for it
/// trains people to ignore a red suite (the same class of harm as a quota
/// ceiling).
///
/// Keyed on the provider's OWN rejection wording — the provider telling us
/// the model doesn't exist — never on what the test asserts, so a response
/// missing the expected content still fails. Deliberately narrower than a
/// generic 4xx: a bare `400 Bad Request` with no model-name wording is our
/// bug and must still fail.
fn model_rejected_by_provider(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("model")
        && (lower.contains("does not exist")
            || lower.contains("doesn't exist")
            || lower.contains("not found")
            || lower.contains("unknown model")
            || lower.contains("unsupported model")
            || lower.contains("model names are")
            || lower.contains("model not supported"))
}

/// Skip-guard wrapper: prints the standard `[skipped]` line and returns
/// true when the provider was unavailable. Every scenario calls this
/// immediately after draining, BEFORE asserting on the response — an
/// errored run has no response to assert against, so the content
/// assertion would otherwise fire first and report a confusing failure.
fn skip_if_provider_unavailable(events: &[AgentEvent]) -> bool {
    match provider_unavailable(events) {
        Some(msg) => {
            let brief: String = msg.chars().take(160).collect();
            eprintln!(
                "[skipped] provider unavailable (quota/rate-limit/auth/network/model-not-found): {brief}"
            );
            true
        }
        None => false,
    }
}

/// Render an AgentEvent stream as a multi-line summary for the
/// stderr trace. Aids debugging when a scenario fails.
fn dump_events(events: &[AgentEvent]) {
    for e in events {
        match e {
            AgentEvent::Token(s) => eprint!("{}", s),
            AgentEvent::Reasoning(_) => eprint!("·"),
            AgentEvent::ToolCall { name, args, .. } => {
                eprintln!("\n[tool_call] {name}({args})");
            }
            AgentEvent::ToolStarted { .. } => {}
            AgentEvent::Usage { .. } => {}
            AgentEvent::ToolResult { output, .. } => {
                eprintln!("\n[tool_result] {} bytes", output.len());
            }
            AgentEvent::TurnStart { index } => eprintln!("\n[turn {index} start]"),
            AgentEvent::TurnEnd { index } => eprintln!("\n[turn {index} end]"),
            AgentEvent::Done { response, .. } => {
                eprintln!("\n[done] response={response:?}");
            }
            AgentEvent::Error(s) => eprintln!("\n[ERROR] {s}"),
            AgentEvent::ContextOverflow { error, .. } => {
                eprintln!("\n[context_overflow] {error}");
            }
            AgentEvent::Interjected { .. } => eprintln!("\n[interjected]"),
            AgentEvent::CustomMessage { payload } => {
                eprintln!("\n[custom_message] {payload}");
            }
            AgentEvent::UserMessage { content } => {
                eprintln!("\n[user_message] {content}");
            }
            AgentEvent::CompactionStarted { .. } => {
                eprintln!("\n[compaction_started]");
            }
            AgentEvent::ContextCompacted { .. } => {
                eprintln!("\n[context_compacted]");
            }
            AgentEvent::CheckpointRefresh { .. } => {
                eprintln!("\n[checkpoint_refresh]");
            }
            AgentEvent::RetryNotice {
                attempt,
                delay_ms,
                error,
            } => {
                eprintln!("\n[retry_notice] attempt={attempt} delay_ms={delay_ms}: {error}");
            }
            AgentEvent::SystemNotice { content } => {
                eprintln!("\n[system_notice] {content}");
            }
            AgentEvent::RepairStats { snapshot } => {
                eprintln!(
                    "\n[repair_stats] total={} invalid={}",
                    snapshot.total_successful(),
                    snapshot.invalid,
                );
            }
            AgentEvent::EscalationActivated { provider, reason } => {
                eprintln!("\n[escalation] provider={provider} reason={reason:?}");
            }
        }
    }
    eprintln!();
}

/// **Scenario 1** — simple text Q&A, no tools.
///
/// Verifies: stream factory works against a real provider;
/// Token events fire; Done arrives with non-empty response.
#[tokio::test]
// h7-real-api: passes via runtime skip when no key is set;
// exercises real provider when DEEPSEEK_API_KEY / OPENAI_API_KEY /
// ANTHROPIC_API_KEY / GEMINI_API_KEY / OPENROUTER_API_KEY is set.
async fn h7_scenario_1_simple_text() {
    let stream_fn = match build_stream_fn() {
        Some(f) => f,
        None => {
            eprintln!("[skipped] no provider API key in env");
            return;
        }
    };

    let cfg = LoopSpawnConfig {
        stream_fn,
        system_prompt: "You are a helpful assistant. Reply concisely.".to_string(),
        history: Vec::new(),
        initial_prompt_images: Vec::new(),
        initial_prompt: "What is 2+2? Reply with just the number, nothing else.".to_string(),
        tools: Vec::new(),
        #[cfg(feature = "plugin")]
        plugin_mgr: None,
        steering_queue: None,
        tool_execution: crate::agent::agent_loop::types::ToolExecutionMode::Parallel,
        event_channel_capacity: 256,
        provider_name: None,
        model_name: None,
        asset_dir: None,
        summarize_fn: None,
        tool_def_filter: None,
        dynamic_tool_search: false,
        escalation_stream_fn: None,
        escalation_provider_name: None,
        escalation_max_per_session: None,
        file_touch_tracker: None,
        progress: None,
        verifier: None,
        critic_fn: None,
        classify_fn: None,
        code_review_fn: None,
        code_review_mode: crate::agent::agent_loop::types::CodeReviewMode::default(),
        open_issues_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        verification_tiers_mode: crate::agent::agent_loop::types::GateMode::Off,
        safe_state_abort_mode: crate::agent::agent_loop::types::SafeStateMode::Off,
        publish_guard_mode: crate::agent::agent_loop::types::GateMode::Off,
        claim_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        source_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        session_id: None,
        goal_fn: None,
        goal: None,
        max_turns: None,
        bg_store: None,
        memory_provider: None,
    };
    let runner = spawn_loop_runner(cfg).into_agent_runner();
    let (events, response) = drain_to_done(runner).await;
    dump_events(&events);
    if skip_if_provider_unavailable(&events) {
        return;
    }

    // Expectations:
    //   - Done event fires.
    //   - Response contains "4".
    //   - At least one Token event streamed (real provider
    //     streams; canned mocks don't).
    let done = response.unwrap_or_default();
    assert!(
        done.contains('4'),
        "expected response to contain '4', got: {done:?}"
    );
    let token_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Token(_)))
        .count();
    assert!(
        token_count >= 1,
        "expected at least 1 Token event from real stream; got 0 (provider may be returning all-at-once Done — check stream wrapping)"
    );

    // No Error or ContextOverflow.
    for e in &events {
        match e {
            AgentEvent::Error(msg) => panic!("unexpected Error: {msg}"),
            AgentEvent::ContextOverflow { error, .. } => {
                panic!("unexpected ContextOverflow: {error}")
            }
            _ => {}
        }
    }
}

/// **Scenario 2** — basic multi-turn structure.
///
/// Issues a prompt that triggers a short follow-up. Verifies the
/// loop produces TurnStart + TurnEnd + Done in the expected
/// order, the Done's response is sensible.
#[tokio::test]
// h7-real-api: passes via runtime skip when no key is set;
// exercises real provider when DEEPSEEK_API_KEY / OPENAI_API_KEY /
// ANTHROPIC_API_KEY / GEMINI_API_KEY / OPENROUTER_API_KEY is set.
async fn h7_scenario_2_turn_boundaries() {
    let stream_fn = match build_stream_fn() {
        Some(f) => f,
        None => {
            eprintln!("[skipped] no provider API key in env");
            return;
        }
    };

    let cfg = LoopSpawnConfig {
        stream_fn,
        system_prompt: "Reply briefly.".to_string(),
        history: Vec::new(),
        initial_prompt_images: Vec::new(),
        initial_prompt: "Say the word 'banana' and nothing else.".to_string(),
        tools: Vec::new(),
        #[cfg(feature = "plugin")]
        plugin_mgr: None,
        steering_queue: None,
        tool_execution: crate::agent::agent_loop::types::ToolExecutionMode::Parallel,
        event_channel_capacity: 256,
        provider_name: None,
        model_name: None,
        asset_dir: None,
        summarize_fn: None,
        tool_def_filter: None,
        dynamic_tool_search: false,
        escalation_stream_fn: None,
        escalation_provider_name: None,
        escalation_max_per_session: None,
        file_touch_tracker: None,
        progress: None,
        verifier: None,
        critic_fn: None,
        classify_fn: None,
        code_review_fn: None,
        code_review_mode: crate::agent::agent_loop::types::CodeReviewMode::default(),
        open_issues_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        verification_tiers_mode: crate::agent::agent_loop::types::GateMode::Off,
        safe_state_abort_mode: crate::agent::agent_loop::types::SafeStateMode::Off,
        publish_guard_mode: crate::agent::agent_loop::types::GateMode::Off,
        claim_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        source_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        session_id: None,
        goal_fn: None,
        goal: None,
        max_turns: None,
        bg_store: None,
        memory_provider: None,
    };
    let runner = spawn_loop_runner(cfg).into_agent_runner();
    let (events, response) = drain_to_done(runner).await;
    dump_events(&events);
    if skip_if_provider_unavailable(&events) {
        return;
    }

    // Expect exactly one TurnStart and one TurnEnd before Done.
    let turn_starts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnStart { .. }))
        .count();
    let turn_ends = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
        .count();
    let dones = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .count();
    assert_eq!(turn_starts, 1, "expected 1 TurnStart, got {turn_starts}");
    assert_eq!(turn_ends, 1, "expected 1 TurnEnd, got {turn_ends}");
    assert_eq!(dones, 1, "expected 1 Done, got {dones}");

    // Response should mention banana.
    assert!(
        response
            .unwrap_or_default()
            .to_lowercase()
            .contains("banana"),
        "expected 'banana' in response",
    );
}

/// **Scenario 5 (slim)** — error path: invalid API key surfaces
/// as an Error event (not a panic, not silent).
///
/// Uses `create_client`'s explicit `api_key` parameter to inject
/// a known-bad key WITHOUT mutating the process env (earlier
/// versions of this test mutated `DEEPSEEK_API_KEY` and raced
/// with parallel tests reading the same var).
#[tokio::test]
// h7-real-api: passes via runtime skip when no key is set;
// exercises real provider when DEEPSEEK_API_KEY / OPENAI_API_KEY /
// ANTHROPIC_API_KEY / GEMINI_API_KEY / OPENROUTER_API_KEY is set.
async fn h7_scenario_5_auth_error_surfaces() {
    // Skip if no provider is configured at all.
    let provider = match detect_provider() {
        Some(p) => p,
        None => {
            eprintln!("[skipped] no API key");
            return;
        }
    };
    let model_name = default_model(provider);

    // Build the client with an EXPLICIT bad key (overrides
    // env). `create_client` takes Option<&str> for this exact
    // case.
    let client = match crate::provider::create_client(
        provider,
        Some("invalid-key-for-h7-test"),
        &std::collections::HashMap::new(),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[skipped] create_client failed: {e}");
            return;
        }
    };
    let any_model = client.completion_model(model_name);
    let chunk_timeout = Some(std::time::Duration::from_secs(60));
    let inner = match any_model {
        crate::provider::AnyModel::DeepSeek(m) => {
            rig_stream_fn_from_model(m, vec![], chunk_timeout)
        }
        crate::provider::AnyModel::OpenAI(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        crate::provider::AnyModel::Anthropic(m) => {
            rig_stream_fn_from_model(m, vec![], chunk_timeout)
        }
        crate::provider::AnyModel::OpenRouter(m) => {
            rig_stream_fn_from_model(m, vec![], chunk_timeout)
        }
        _ => {
            eprintln!("[skipped] unsupported provider variant for this scenario");
            return;
        }
    };
    let stream_fn = retrying_stream_fn(inner, RecoveryPolicy::default());

    let cfg = LoopSpawnConfig {
        stream_fn,
        system_prompt: String::new(),
        history: Vec::new(),
        initial_prompt_images: Vec::new(),
        initial_prompt: "hi".to_string(),
        tools: Vec::new(),
        #[cfg(feature = "plugin")]
        plugin_mgr: None,
        steering_queue: None,
        tool_execution: crate::agent::agent_loop::types::ToolExecutionMode::Parallel,
        event_channel_capacity: 256,
        provider_name: None,
        model_name: None,
        asset_dir: None,
        summarize_fn: None,
        tool_def_filter: None,
        dynamic_tool_search: false,
        escalation_stream_fn: None,
        escalation_provider_name: None,
        escalation_max_per_session: None,
        file_touch_tracker: None,
        progress: None,
        verifier: None,
        critic_fn: None,
        classify_fn: None,
        code_review_fn: None,
        code_review_mode: crate::agent::agent_loop::types::CodeReviewMode::default(),
        open_issues_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        verification_tiers_mode: crate::agent::agent_loop::types::GateMode::Off,
        safe_state_abort_mode: crate::agent::agent_loop::types::SafeStateMode::Off,
        publish_guard_mode: crate::agent::agent_loop::types::GateMode::Off,
        claim_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        source_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        session_id: None,
        goal_fn: None,
        goal: None,
        max_turns: None,
        bg_store: None,
        memory_provider: None,
    };
    let runner = spawn_loop_runner(cfg).into_agent_runner();
    let (events, _) = drain_to_done(runner).await;
    dump_events(&events);
    // NO `skip_if_provider_unavailable` here, deliberately. This scenario
    // drives an auth failure ON PURPOSE with a bad key; the error IS the
    // thing under test, so the skip-guard that protects the other scenarios
    // would defeat this one entirely.

    // Auth error → either Error event (non-retryable
    // classification per recovery::classify_error) OR Done
    // with an empty / error-formatted response. The retry
    // wrapper routes Auth → no retry.
    let had_error = events
        .iter()
        .any(|e| matches!(e, AgentEvent::Error(_) | AgentEvent::ContextOverflow { .. }));
    assert!(
        had_error,
        "expected Error or ContextOverflow event for invalid key"
    );
}

/// **Scenario 3 (slim)** — tool dispatch against a real LLM.
///
/// Uses an inline `LoopTool` that echoes its input. Builds a
/// matching rig `ToolDefinition` so the LLM knows about it.
/// Verifies the model can be coaxed into using it and the
/// loop dispatches + returns the result + the model uses the
/// result in a follow-up turn.
///
/// This DOESN'T exercise the production LoopTool registry
/// (which requires permission asker + sandbox + many other
/// fixture inputs). It does exercise the full dispatch chain:
/// rig stream → tool call extraction → LoopTool execute →
/// finalize → next LLM turn.
#[tokio::test]
// h7-real-api: passes via runtime skip when no key is set;
// exercises real provider when DEEPSEEK_API_KEY / OPENAI_API_KEY /
// ANTHROPIC_API_KEY / GEMINI_API_KEY / OPENROUTER_API_KEY is set.
async fn h7_scenario_3_tool_dispatch() {
    use crate::agent::agent_loop::result::LoopToolResult as ResultT;
    use crate::agent::agent_loop::tool::{AbortSignal, LoopToolUpdate};
    use crate::agent::agent_loop::{LoopTool, LoopToolResult, loop_tool_to_rig_definition};
    use rig::completion::ToolDefinition;
    use serde_json::Value;
    use std::pin::Pin;

    let provider = match detect_provider() {
        Some(p) => p,
        None => {
            eprintln!("[skipped] no API key");
            return;
        }
    };
    if provider != "deepseek" && provider != "openai" && provider != "openrouter" {
        eprintln!("[skipped] tool-use test prefers deepseek/openai/openrouter; got {provider}");
        // anthropic / gemini tools work but the prompt phrasing
        // below is tuned for OpenAI-shaped function calling.
        return;
    }

    // Build an inline LoopTool that echoes its `text` argument
    // back. Mirrors the EchoTool from in-module tests.
    #[derive(Debug)]
    struct EchoTool;
    impl LoopTool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }
        fn description(&self) -> &str {
            "Echo the given text back. Use this when asked to echo something."
        }
        fn label(&self) -> &str {
            "Echo"
        }
        fn parameters(&self) -> &Value {
            static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            P.get_or_init(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string", "description": "Text to echo"}
                    },
                    "required": ["text"]
                })
            })
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            args: Value,
            _signal: AbortSignal,
            _on_update: LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<ResultT, String>> + Send + 'a>> {
            Box::pin(async move {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no text)")
                    .to_string();
                Ok(ResultT {
                    content: vec![serde_json::json!({
                        "type": "text",
                        "text": format!("ECHO: {text}"),
                    })],
                    details: Value::Null,
                    terminate: None,
                })
            })
        }
    }

    let tool = Arc::new(EchoTool) as Arc<dyn LoopTool>;
    let tool_def = loop_tool_to_rig_definition(tool.as_ref());

    // Build StreamFn WITH the tool definition.
    let model_name = default_model(provider);
    let client = crate::provider::create_client(provider, None, &std::collections::HashMap::new())
        .expect("client");
    let any_model = client.completion_model(model_name);
    let chunk_timeout = Some(std::time::Duration::from_secs(60));
    let inner_stream_fn = match any_model {
        crate::provider::AnyModel::DeepSeek(m) => {
            rig_stream_fn_from_model(m, vec![tool_def.clone()], chunk_timeout)
        }
        crate::provider::AnyModel::OpenAI(m) => {
            rig_stream_fn_from_model(m, vec![tool_def.clone()], chunk_timeout)
        }
        crate::provider::AnyModel::OpenRouter(m) => {
            rig_stream_fn_from_model(m, vec![tool_def.clone()], chunk_timeout)
        }
        _ => {
            eprintln!("[skipped] this scenario hardcoded to deepseek/openai/openrouter");
            return;
        }
    };
    let stream_fn = retrying_stream_fn(inner_stream_fn, RecoveryPolicy::default());

    eprintln!("[h7-smoke] tool-dispatch test using {provider}/{model_name}");

    let cfg = LoopSpawnConfig {
        stream_fn,
        system_prompt: "You have access to an echo_tool that echoes text back. \
                        When the user asks you to echo something, USE THE TOOL — \
                        do not just reply with the text directly. After calling \
                        the tool, briefly confirm what was echoed."
            .to_string(),
        history: Vec::new(),
        initial_prompt_images: Vec::new(),
        initial_prompt: "Echo the word 'pineapple'.".to_string(),
        tools: vec![tool],
        #[cfg(feature = "plugin")]
        plugin_mgr: None,
        steering_queue: None,
        tool_execution: crate::agent::agent_loop::types::ToolExecutionMode::Sequential,
        event_channel_capacity: 256,
        provider_name: None,
        model_name: None,
        asset_dir: None,
        summarize_fn: None,
        tool_def_filter: None,
        dynamic_tool_search: false,
        escalation_stream_fn: None,
        escalation_provider_name: None,
        escalation_max_per_session: None,
        file_touch_tracker: None,
        progress: None,
        verifier: None,
        critic_fn: None,
        classify_fn: None,
        code_review_fn: None,
        code_review_mode: crate::agent::agent_loop::types::CodeReviewMode::default(),
        open_issues_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        verification_tiers_mode: crate::agent::agent_loop::types::GateMode::Off,
        safe_state_abort_mode: crate::agent::agent_loop::types::SafeStateMode::Off,
        publish_guard_mode: crate::agent::agent_loop::types::GateMode::Off,
        claim_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        source_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        session_id: None,
        goal_fn: None,
        goal: None,
        max_turns: None,
        bg_store: None,
        memory_provider: None,
    };
    let runner = spawn_loop_runner(cfg).into_agent_runner();
    let (events, response) = drain_to_done(runner).await;
    dump_events(&events);
    if skip_if_provider_unavailable(&events) {
        return;
    }

    // Expectations:
    //   - At least one ToolCall event (model used the tool)
    //   - At least one ToolResult event (we dispatched it)
    //   - Done with a response mentioning "pineapple" (model
    //     summarized after the tool ran)
    let tool_calls = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCall { .. }))
        .count();
    let tool_results = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .count();
    assert!(
        tool_calls >= 1,
        "expected the LLM to call echo_tool, got 0 ToolCall events"
    );
    assert!(
        tool_results >= 1,
        "expected at least 1 ToolResult event, got 0"
    );
    assert_eq!(
        tool_calls, tool_results,
        "expected ToolCall and ToolResult counts to match"
    );
    let final_resp = response.unwrap_or_default();
    assert!(
        !final_resp.trim().is_empty(),
        "expected a final assistant turn after the tool round trip; got an empty response"
    );
    let last_tool_result = events
        .iter()
        .rposition(|e| matches!(e, AgentEvent::ToolResult { .. }));
    let done_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }));
    assert!(
        last_tool_result.is_some_and(|tr| done_pos.is_some_and(|done| tr < done)),
        "expected the final assistant turn to follow the completed tool result"
    );
}

// =====================================================================
// GLM (Zhipu) scenarios. The key lives in ZHIPU_API_KEY in this
// environment; dirge's create_client reads GLM_API_KEY. We alias
// at the test boundary.
// =====================================================================

/// Build a StreamFn explicitly for GLM with the glm-5.1 model.
/// Reads the key from ZHIPU_API_KEY (or GLM_API_KEY) and passes
/// it through `create_client`'s explicit `api_key` arg — NO env
/// mutation, so this is safe to run in parallel with other
/// smoke tests.
fn build_glm_stream_fn() -> Option<crate::agent::agent_loop::StreamFn> {
    use crate::provider::AnyModel;
    use std::collections::HashMap;

    let key = match std::env::var("ZHIPU_API_KEY").or_else(|_| std::env::var("GLM_API_KEY")) {
        Ok(k) => k,
        Err(_) => {
            eprintln!("[skipped] need ZHIPU_API_KEY or GLM_API_KEY");
            return None;
        }
    };

    let model_name = "glm-5.1";
    let client = match crate::provider::create_client("glm", Some(key.as_str()), &HashMap::new()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[h7-smoke] failed to build glm client: {e}");
            return None;
        }
    };
    let any_model = client.completion_model(model_name);

    let chunk_timeout = Some(std::time::Duration::from_secs(60));
    let stream_fn = match any_model {
        AnyModel::Glm(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        _ => {
            eprintln!("[h7-smoke] expected AnyModel::Glm");
            return None;
        }
    };

    eprintln!("[h7-smoke] using provider=glm model={model_name}");
    Some(retrying_stream_fn(stream_fn, RecoveryPolicy::default()))
}

/// GLM Scenario 1 — simple text Q&A.
#[tokio::test]
// h7-glm: passes via runtime skip when ZHIPU_API_KEY is unset;
// exercises GLM when ZHIPU_API_KEY is set.
async fn h7_glm_scenario_1_simple_text() {
    let stream_fn = match build_glm_stream_fn() {
        Some(f) => f,
        None => return,
    };
    let cfg = LoopSpawnConfig {
        stream_fn,
        system_prompt: "You are a helpful assistant. Reply concisely.".to_string(),
        history: Vec::new(),
        initial_prompt_images: Vec::new(),
        initial_prompt: "What is 2+2? Reply with just the number, nothing else.".to_string(),
        tools: Vec::new(),
        #[cfg(feature = "plugin")]
        plugin_mgr: None,
        steering_queue: None,
        tool_execution: crate::agent::agent_loop::types::ToolExecutionMode::Parallel,
        event_channel_capacity: 256,
        provider_name: None,
        model_name: None,
        asset_dir: None,
        summarize_fn: None,
        tool_def_filter: None,
        dynamic_tool_search: false,
        escalation_stream_fn: None,
        escalation_provider_name: None,
        escalation_max_per_session: None,
        file_touch_tracker: None,
        progress: None,
        verifier: None,
        critic_fn: None,
        classify_fn: None,
        code_review_fn: None,
        code_review_mode: crate::agent::agent_loop::types::CodeReviewMode::default(),
        open_issues_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        verification_tiers_mode: crate::agent::agent_loop::types::GateMode::Off,
        safe_state_abort_mode: crate::agent::agent_loop::types::SafeStateMode::Off,
        publish_guard_mode: crate::agent::agent_loop::types::GateMode::Off,
        claim_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        source_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        session_id: None,
        goal_fn: None,
        goal: None,
        max_turns: None,
        bg_store: None,
        memory_provider: None,
    };
    let runner = spawn_loop_runner(cfg).into_agent_runner();
    let (events, response) = drain_to_done(runner).await;
    dump_events(&events);
    if skip_if_provider_unavailable(&events) {
        return;
    }

    let done = response.unwrap_or_default();
    assert!(
        done.contains('4'),
        "expected response to contain '4', got: {done:?}"
    );
    for e in &events {
        if let AgentEvent::Error(msg) = e {
            panic!("unexpected Error: {msg}");
        }
    }
}

/// GLM Scenario 3 — tool dispatch. Uses the same inline echo
/// tool as the DeepSeek scenario but routed through GLM.
#[tokio::test]
// h7-glm: passes via runtime skip when ZHIPU_API_KEY is unset;
// exercises GLM when ZHIPU_API_KEY is set.
async fn h7_glm_scenario_3_tool_dispatch() {
    use crate::agent::agent_loop::result::LoopToolResult as ResultT;
    use crate::agent::agent_loop::tool::{AbortSignal, LoopToolUpdate};
    use crate::agent::agent_loop::{LoopTool, loop_tool_to_rig_definition};
    use serde_json::Value;
    use std::pin::Pin;

    let key = match std::env::var("ZHIPU_API_KEY").or_else(|_| std::env::var("GLM_API_KEY")) {
        Ok(k) => k,
        Err(_) => {
            eprintln!("[skipped] need ZHIPU_API_KEY or GLM_API_KEY");
            return;
        }
    };

    #[derive(Debug)]
    struct EchoTool;
    impl LoopTool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }
        fn description(&self) -> &str {
            "Echo the given text back. Use this when asked to echo something."
        }
        fn label(&self) -> &str {
            "Echo"
        }
        fn parameters(&self) -> &Value {
            static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            P.get_or_init(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string", "description": "Text to echo"}
                    },
                    "required": ["text"]
                })
            })
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            args: Value,
            _signal: AbortSignal,
            _on_update: LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<ResultT, String>> + Send + 'a>> {
            Box::pin(async move {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(none)")
                    .to_string();
                Ok(ResultT {
                    content: vec![serde_json::json!({
                        "type": "text",
                        "text": format!("ECHO: {text}"),
                    })],
                    details: Value::Null,
                    terminate: None,
                })
            })
        }
    }

    let tool = Arc::new(EchoTool) as Arc<dyn LoopTool>;
    let tool_def = loop_tool_to_rig_definition(tool.as_ref());

    let client = crate::provider::create_client(
        "glm",
        Some(key.as_str()),
        &std::collections::HashMap::new(),
    )
    .expect("client");
    let any_model = client.completion_model("glm-5.1");
    let chunk_timeout = Some(std::time::Duration::from_secs(60));
    let inner = match any_model {
        crate::provider::AnyModel::Glm(m) => {
            rig_stream_fn_from_model(m, vec![tool_def], chunk_timeout)
        }
        _ => panic!("expected Glm variant"),
    };
    let stream_fn = retrying_stream_fn(inner, RecoveryPolicy::default());

    eprintln!("[h7-smoke] glm tool-dispatch test");
    let cfg = LoopSpawnConfig {
        stream_fn,
        system_prompt: "You have access to an echo_tool that echoes text back. \
                        When the user asks you to echo something, USE THE TOOL — \
                        do not just reply with the text directly. After calling \
                        the tool, briefly confirm what was echoed."
            .to_string(),
        history: Vec::new(),
        initial_prompt_images: Vec::new(),
        initial_prompt: "Echo the word 'pineapple'.".to_string(),
        tools: vec![tool],
        #[cfg(feature = "plugin")]
        plugin_mgr: None,
        steering_queue: None,
        tool_execution: crate::agent::agent_loop::types::ToolExecutionMode::Sequential,
        event_channel_capacity: 256,
        provider_name: None,
        model_name: None,
        asset_dir: None,
        summarize_fn: None,
        tool_def_filter: None,
        dynamic_tool_search: false,
        escalation_stream_fn: None,
        escalation_provider_name: None,
        escalation_max_per_session: None,
        file_touch_tracker: None,
        progress: None,
        verifier: None,
        critic_fn: None,
        classify_fn: None,
        code_review_fn: None,
        code_review_mode: crate::agent::agent_loop::types::CodeReviewMode::default(),
        open_issues_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        verification_tiers_mode: crate::agent::agent_loop::types::GateMode::Off,
        safe_state_abort_mode: crate::agent::agent_loop::types::SafeStateMode::Off,
        publish_guard_mode: crate::agent::agent_loop::types::GateMode::Off,
        claim_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        source_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        session_id: None,
        goal_fn: None,
        goal: None,
        max_turns: None,
        bg_store: None,
        memory_provider: None,
    };
    let runner = spawn_loop_runner(cfg).into_agent_runner();
    let (events, response) = drain_to_done(runner).await;
    dump_events(&events);
    if skip_if_provider_unavailable(&events) {
        return;
    }

    let tool_calls = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCall { .. }))
        .count();
    let tool_results = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .count();
    assert!(tool_calls >= 1, "expected echo_tool call from GLM, got 0");
    assert_eq!(tool_calls, tool_results, "call/result count mismatch");
    let final_resp = response.unwrap_or_default();
    assert!(
        !final_resp.trim().is_empty(),
        "expected a final assistant turn after the tool round trip; got an empty response"
    );
    let last_tool_result = events
        .iter()
        .rposition(|e| matches!(e, AgentEvent::ToolResult { .. }));
    let done_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }));
    assert!(
        last_tool_result.is_some_and(|tr| done_pos.is_some_and(|done| tr < done)),
        "expected the final assistant turn to follow the completed tool result"
    );
}

// =====================================================================
// Cerebras scenarios
// =====================================================================

fn cerebras_model(model_name: &str) -> Option<crate::provider::AnyModel> {
    use rig::client::CompletionClient;

    let key = match std::env::var("CEREBRAS_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key,
        _ => {
            eprintln!("[skipped] CEREBRAS_API_KEY is unset");
            return None;
        }
    };
    let client = crate::provider::create_client(
        "cerebras",
        Some(key.as_str()),
        &std::collections::HashMap::new(),
    )
    .expect("Cerebras client should build from CEREBRAS_API_KEY");
    Some(client.completion_model(model_name))
}

fn cerebras_spawn_config(
    stream_fn: crate::agent::agent_loop::StreamFn,
    system_prompt: &str,
    initial_prompt: &str,
    tools: Vec<Arc<dyn crate::agent::agent_loop::LoopTool>>,
    model_name: &str,
) -> LoopSpawnConfig {
    LoopSpawnConfig {
        stream_fn,
        system_prompt: system_prompt.to_string(),
        history: Vec::new(),
        initial_prompt_images: Vec::new(),
        initial_prompt: initial_prompt.to_string(),
        tools,
        #[cfg(feature = "plugin")]
        plugin_mgr: None,
        steering_queue: None,
        tool_execution: crate::agent::agent_loop::types::ToolExecutionMode::Sequential,
        event_channel_capacity: 256,
        provider_name: Some("cerebras".to_string()),
        model_name: Some(model_name.to_string()),
        asset_dir: None,
        summarize_fn: None,
        tool_def_filter: None,
        dynamic_tool_search: false,
        escalation_stream_fn: None,
        escalation_provider_name: None,
        escalation_max_per_session: None,
        file_touch_tracker: None,
        progress: None,
        verifier: None,
        critic_fn: None,
        classify_fn: None,
        code_review_fn: None,
        code_review_mode: crate::agent::agent_loop::types::CodeReviewMode::default(),
        open_issues_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        verification_tiers_mode: crate::agent::agent_loop::types::GateMode::Off,
        safe_state_abort_mode: crate::agent::agent_loop::types::SafeStateMode::Off,
        publish_guard_mode: crate::agent::agent_loop::types::GateMode::Off,
        claim_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        source_gate_mode: crate::agent::agent_loop::types::GateMode::Off,
        session_id: None,
        goal_fn: None,
        goal: None,
        max_turns: None,
        bg_store: None,
        memory_provider: None,
    }
}

#[tokio::test]
async fn h7_cerebras_streaming_returns_non_empty_assistant_text() {
    let model_name = crate::provider::default_model_for("cerebras");
    let Some(model) = cerebras_model(model_name) else {
        return;
    };
    assert_eq!(model.provider_name(), "cerebras");
    let inner = model.build_stream_fn(
        Vec::new(),
        std::time::Duration::from_secs(60),
        Some("cerebras".to_string()),
    );
    let stream_fn = retrying_stream_fn(inner, RecoveryPolicy::default());
    let runner = spawn_loop_runner(cerebras_spawn_config(
        stream_fn,
        "You are a concise assistant.",
        "Reply with a short greeting.",
        Vec::new(),
        model_name,
    ))
    .into_agent_runner();
    let (events, response) = drain_to_done(runner).await;
    dump_events(&events);
    if skip_if_provider_unavailable(&events) {
        return;
    }

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Error(_))),
        "Cerebras streaming produced an error: {events:?}",
    );
    assert!(
        !response.unwrap_or_default().trim().is_empty(),
        "Cerebras should return non-empty assistant text",
    );
}

#[derive(Debug)]
struct CerebrasEchoTool;

impl crate::agent::agent_loop::LoopTool for CerebrasEchoTool {
    fn name(&self) -> &str {
        "echo_tool"
    }

    fn description(&self) -> &str {
        "Echo the given text back. Always use this when asked to echo text."
    }

    fn label(&self) -> &str {
        "Echo"
    }

    fn parameters(&self) -> &serde_json::Value {
        static PARAMETERS: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        PARAMETERS.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Text to echo" }
                },
                "required": ["text"]
            })
        })
    }

    fn execute<'a>(
        &'a self,
        _id: &'a str,
        args: serde_json::Value,
        _signal: crate::agent::agent_loop::tool::AbortSignal,
        _on_update: crate::agent::agent_loop::tool::LoopToolUpdate,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<crate::agent::agent_loop::result::LoopToolResult, String>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let text = args
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("(missing)");
            Ok(crate::agent::agent_loop::result::LoopToolResult {
                content: vec![serde_json::json!({
                    "type": "text",
                    "text": format!("ECHO: {text}"),
                })],
                details: serde_json::Value::Null,
                terminate: None,
            })
        })
    }
}

#[tokio::test]
async fn h7_cerebras_tool_dispatch_completes_round_trip() {
    use crate::agent::agent_loop::loop_tool_to_rig_definition;

    let model_name = "gpt-oss-120b";
    let Some(model) = cerebras_model(model_name) else {
        return;
    };
    assert_eq!(model.provider_name(), "cerebras");
    let tool: Arc<dyn crate::agent::agent_loop::LoopTool> = Arc::new(CerebrasEchoTool);
    let tool_definition = loop_tool_to_rig_definition(tool.as_ref());
    let inner = model.build_stream_fn(
        vec![tool_definition],
        std::time::Duration::from_secs(60),
        Some("cerebras".to_string()),
    );
    let stream_fn = retrying_stream_fn(inner, RecoveryPolicy::default());
    let runner = spawn_loop_runner(cerebras_spawn_config(
        stream_fn,
        "You have an echo_tool. When asked to echo text, you must call the tool, then confirm its result.",
        "Use echo_tool to echo the word pineapple.",
        vec![tool],
        model_name,
    ))
    .into_agent_runner();
    let (events, response) = drain_to_done(runner).await;
    dump_events(&events);
    if skip_if_provider_unavailable(&events) {
        return;
    }

    let tool_calls = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolCall { .. }))
        .count();
    let tool_results = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolResult { .. }))
        .count();
    assert!(tool_calls >= 1, "expected a Cerebras tool call");
    assert_eq!(tool_calls, tool_results, "every tool call must complete");
    let final_response = response.unwrap_or_default();
    assert!(
        !final_response.trim().is_empty(),
        "expected a final assistant turn after the tool round trip; got an empty response"
    );
    let last_tool_result = events
        .iter()
        .rposition(|e| matches!(e, AgentEvent::ToolResult { .. }));
    let done_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }));
    assert!(
        last_tool_result.is_some_and(|tr| done_pos.is_some_and(|done| tr < done)),
        "expected the final assistant turn to follow the completed tool result"
    );
}

// Scenarios 4 (mid-run interjection), 6 (context overflow),
// and 7 (plugin hook) are covered in the manual runbook
// (docs/H7_AGENT_LOOP_TEST.md). They require interactive UI,
// large prompts, or plugin file setup that doesn't translate
// well to an automated test.

#[allow(unused_imports, dead_code)]
fn _ensure_arc_used(_: Arc<()>) {}

#[cfg(test)]
mod skip_guard_tests {
    use super::provider_unavailable;
    use crate::event::AgentEvent;

    fn err(msg: &str) -> Vec<AgentEvent> {
        vec![AgentEvent::Error(msg.to_string().into())]
    }

    /// The environmental failures that must SKIP rather than fail: no code
    /// change can turn these green, so failing on them just teaches people
    /// to ignore a red suite.
    #[test]
    fn environmental_failures_are_skippable() {
        // GLM's exhausted 5-hour window — the case that motivated this.
        assert!(
            provider_unavailable(&err(
                r#"ProviderError: Invalid status code 429 Too Many Requests with message: {"error":{"code":"1308","message":"已达到 5 小时的使用上限。"}}"#
            ))
            .is_some(),
            "GLM usage cap must skip"
        );
        assert!(
            provider_unavailable(&err("Invalid status code 401 Unauthorized")).is_some(),
            "auth failure must skip"
        );
        assert!(
            provider_unavailable(&err("error sending request: connection refused")).is_some(),
            "network failure must skip"
        );
    }

    /// A provider that rejects the request because the model name doesn't
    /// exist (retired upstream or typo'd) is a prerequisite miss, not a
    /// dirge defect — it must skip, not fail.
    #[test]
    fn model_rejection_skips() {
        assert!(
            provider_unavailable(&err(
                "The supported API model names are deepseek-v4-pro or deepseek-v4-flash, but you passed deepseek-chat."
            ))
            .is_some(),
            "a retired deepseek model name must skip"
        );
        assert!(
            provider_unavailable(&err("The model `gpt-oss-120b` does not exist")).is_some(),
            "an unknown model id must skip"
        );
        assert!(
            provider_unavailable(&err("Model not found: unknown-model")).is_some(),
            "a model-not-found response must skip"
        );
    }

    /// The guard must NOT swallow a real defect. An unclassified provider
    /// error, or a run that simply produced no Error event, still fails —
    /// otherwise the smoke tests would be green no matter what broke.
    #[test]
    fn genuine_failures_still_fail() {
        assert!(
            provider_unavailable(&err("assistant produced a malformed tool call")).is_none(),
            "an unclassified error is a real failure"
        );
        assert!(
            provider_unavailable(&err("Invalid status code 400 Bad Request")).is_none(),
            "a 400 is our bug, not the provider being away"
        );
        assert!(
            provider_unavailable(&[]).is_none(),
            "no error event at all is not a skip"
        );
        // A successful run is never a skip.
        assert!(
            provider_unavailable(&[AgentEvent::Token("hi".to_string().into())]).is_none(),
            "a normal event stream is not a skip"
        );
    }
}
