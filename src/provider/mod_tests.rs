use super::*;
use crate::config::ProviderEntry;
use rig::client::CompletionClient;
use std::collections::HashMap;

/// Build an env-lookup closure backed by a HashMap. Avoids
/// mutating process-wide env vars — `std::env::set_var` is
/// thread-unsafe and the previous test suite raced under
/// parallel `cargo test`, producing intermittent failures.
fn mock_env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let map: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |name: &str| map.get(name).cloned()
}

#[test]
fn auto_detect_returns_none_when_no_vars_set() {
    assert_eq!(auto_detect_provider_from(mock_env(&[])), None);
}

#[test]
fn auto_detect_finds_deepseek_when_key_set() {
    let env = mock_env(&[("DEEPSEEK_API_KEY", "sk-test-123")]);
    assert_eq!(auto_detect_provider_from(env), Some("deepseek"));
}

#[test]
fn auto_detect_finds_openai_when_key_set() {
    let env = mock_env(&[("OPENAI_API_KEY", "sk-test-456")]);
    assert_eq!(auto_detect_provider_from(env), Some("openai"));
}

#[test]
fn auto_detect_skips_empty_var() {
    let env = mock_env(&[("DEEPSEEK_API_KEY", ""), ("OPENAI_API_KEY", "sk-test-789")]);
    assert_eq!(auto_detect_provider_from(env), Some("openai"));
}

#[test]
fn auto_detect_returns_first_match_in_order() {
    let env = mock_env(&[("DEEPSEEK_API_KEY", "sk-ds"), ("OPENAI_API_KEY", "sk-oai")]);
    assert_eq!(auto_detect_provider_from(env), Some("deepseek"));
}

/// Cover every provider in the autodetect list — guards
/// against accidentally dropping or reordering an entry.
#[test]
fn auto_detect_each_provider_in_isolation() {
    for &(env_var, expected) in PROVIDER_AUTODETECT_ORDER {
        let env = mock_env(&[(env_var, "sk-x")]);
        assert_eq!(
            auto_detect_provider_from(env),
            Some(expected),
            "env_var={env_var}",
        );
    }
}

/// `ZHIPU_API_KEY` alone resolves to glm provider — Zhipu's
/// canonical env-var name doesn't require users to alias.
#[test]
fn auto_detect_zhipu_api_key_resolves_to_glm() {
    let env = mock_env(&[("ZHIPU_API_KEY", "fake-zhipu-key")]);
    assert_eq!(auto_detect_provider_from(env), Some("glm"));
}

/// When BOTH GLM_API_KEY and ZHIPU_API_KEY are set, the
/// dirge-primary GLM_API_KEY wins (it's earlier in
/// PROVIDER_AUTODETECT_ORDER). The fallback only fires when
/// the primary is absent.
#[test]
fn auto_detect_glm_api_key_wins_over_zhipu_when_both_set() {
    let env = mock_env(&[("GLM_API_KEY", "primary"), ("ZHIPU_API_KEY", "fallback")]);
    // Both map to "glm" so the answer is the same kind, but
    // this guards against a future reordering breaking the
    // primary-first invariant. We can't observe WHICH var
    // resolve_api_key picked from auto_detect alone — that's
    // tested below.
    assert_eq!(auto_detect_provider_from(env), Some("glm"));
}

/// No stored `dirge auth` login → no provider implied, so the
/// caller falls through to the openrouter default.
#[test]
fn auth_detect_returns_none_when_no_login_present() {
    assert_eq!(auth_detect_provider_from(false, false), None);
}

/// A stored `dirge auth openai` login makes openai the provider
/// even with no API-key env var set (GH #617).
#[test]
fn auth_detect_picks_openai_when_openai_login_present() {
    assert_eq!(auth_detect_provider_from(true, false), Some("openai"));
}

/// A stored `dirge auth anthropic` login makes anthropic the
/// provider, mirroring the openai case.
#[test]
fn auth_detect_picks_anthropic_when_anthropic_login_present() {
    assert_eq!(auth_detect_provider_from(false, true), Some("anthropic"));
}

/// With both logins present, openai wins — a stable order matching
/// the env-autodetect list where openai precedes anthropic.
#[test]
fn auth_detect_prefers_openai_when_both_logins_present() {
    assert_eq!(auth_detect_provider_from(true, true), Some("openai"));
}

/// `provider_env_var_fallbacks` lists canonical API-KEY alternatives for
/// GLM (Zhipu's name) and Gemini (Google's canonical form). Anthropic has
/// none: ANTHROPIC_OAUTH_TOKEN is an OAuth bearer, not an api key, and now
/// routes through ProviderAuth::Anthropic (dirge-ro8g). Other providers
/// have no alternatives.
#[test]
fn fallback_list_covers_canonical_alternatives() {
    assert_eq!(
        provider_env_var_fallbacks(ProviderKind::Glm),
        &["ZHIPU_API_KEY"]
    );
    assert_eq!(
        provider_env_var_fallbacks(ProviderKind::Gemini),
        &["GOOGLE_GENERATIVE_AI_API_KEY", "GOOGLE_API_KEY"]
    );
    for kind in [
        // dirge-ro8g: Anthropic no longer treats ANTHROPIC_OAUTH_TOKEN as
        // an API-key fallback.
        ProviderKind::Anthropic,
        ProviderKind::OpenAI,
        ProviderKind::DeepSeek,
        ProviderKind::OpenRouter,
        ProviderKind::Ollama,
        ProviderKind::Custom,
    ] {
        assert!(
            provider_env_var_fallbacks(kind).is_empty(),
            "no fallback expected for {kind:?}",
        );
    }
}

// ============================================================
// Phase 4.5h-2: AnyAgent::build_stream_fn dispatch tests
// ============================================================

/// Build a real `AnyAgent` from an openai-shaped client +
/// model. The Client::new doesn't connect (no network until
/// the first request), so this works in unit tests.
///
/// The OpenAI variant uses Rig's Chat Completions model; it is never
/// called during these tests, so construction stays offline.
fn build_openai_any_agent() -> AnyAgent {
    use rig::providers::openai;
    let client = openai::CompletionsClient::builder()
        .api_key("test-key")
        .build()
        .expect("openai CompletionsClient::new should work");
    let model = client.completion_model("gpt-4o");
    let agent = rig::agent::AgentBuilder::new(model).build();
    AnyAgent::new(
        AnyAgentInner::OpenAI(agent),
        ToolCache::new(),
        std::time::Duration::from_secs(300),
        Vec::new(),    // loop_tools — empty for test fixture
        String::new(), // preamble — empty for test fixture
        "gpt-4o".to_string(),
    )
}

/// `build_stream_fn` returns a `Send + Sync + 'static`
/// `StreamFn` for the OpenAI variant. Compile-time check —
/// if the bounds don't match the type would fail to
/// construct.
#[test]
fn build_stream_fn_returns_send_sync_static() {
    fn assert_send_sync_static<T: Send + Sync + 'static>(_: &T) {}
    let agent = build_openai_any_agent();
    let stream_fn = agent.build_stream_fn(vec![]);
    assert_send_sync_static(&stream_fn);
}

/// `build_stream_fn` is callable as a `Fn` (multi-call) —
/// the loop invokes it once per turn. Verify by calling
/// twice and checking both invocations produce streams.
#[tokio::test]
async fn build_stream_fn_is_multi_callable() {
    use crate::agent::agent_loop::LlmContext;
    use crate::agent::agent_loop::tool::AbortSignal;
    use futures::stream::StreamExt;

    let agent = build_openai_any_agent();
    let stream_fn = agent.build_stream_fn(vec![]);

    // Call once with an empty context — should emit an
    // Error event (no prompt) without panicking.
    let ctx = LlmContext {
        system_prompt: String::new(),
        messages: vec![],
        asset_dir: None,
    };
    let mut s = stream_fn(
        ctx,
        crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
    );
    let first = s.next().await;
    assert!(first.is_some(), "first call should produce events");

    // Call again — same closure, same Arc, fresh stream.
    let ctx2 = LlmContext {
        system_prompt: String::new(),
        messages: vec![],
        asset_dir: None,
    };
    let mut s2 = stream_fn(
        ctx2,
        crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
    );
    let second = s2.next().await;
    assert!(second.is_some(), "second call should also produce events");
}

/// All 8 `AnyAgentInner` variants compile through
/// `build_stream_fn` — the match arms cover the full enum,
/// and the bounds on `rig_stream_fn_from_model<M>` are
/// satisfied by each provider's `CompletionModel`.
///
/// This test exists primarily as a compile-time
/// canary: if a future provider variant gets added to
/// `AnyAgentInner` without a matching arm in
/// `build_stream_fn`, the build breaks. Runtime
/// dispatch is exercised by the OpenAI-backed tests
/// above.
#[test]
fn build_stream_fn_covers_all_variants_compile_time() {
    // Just constructs one variant and calls
    // build_stream_fn; the rest are validated by the
    // match-arm exhaustiveness check at compile time.
    let agent = build_openai_any_agent();
    let _ = agent.build_stream_fn(vec![]);
}

#[tokio::test]
async fn any_model_filtered_stream_fn_hides_unloaded_dynamic_tools() {
    use crate::agent::agent_loop::stream::{LlmContext, StreamOptions};
    use crate::agent::agent_loop::tool::AbortSignal;
    use futures::StreamExt;
    use rig::completion::ToolDefinition;
    use rig::providers::openai;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn tool_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("{name} description"),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|window| window == b"\r\n\r\n")
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (body_tx, body_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let body_start = loop {
            let mut chunk = [0u8; 1024];
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "client closed before sending request headers");
            buf.extend_from_slice(&chunk[..read]);
            if let Some(end) = header_end(&buf) {
                break end + 4;
            }
        };
        let headers = String::from_utf8_lossy(&buf[..body_start]);
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap();
        while buf.len() < body_start + content_length {
            let mut chunk = [0u8; 1024];
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "client closed before sending full request body");
            buf.extend_from_slice(&chunk[..read]);
        }
        let body = serde_json::from_slice::<serde_json::Value>(
            &buf[body_start..body_start + content_length],
        )
        .unwrap();
        body_tx.send(body).ok();
        socket
            .write_all(
                b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    });

    let client = openai::CompletionsClient::builder()
        .api_key("test-key")
        .base_url(&base_url)
        .build()
        .unwrap();
    let model = AnyModel::OpenAI(client.completion_model("gpt-4o"));
    let loaded = Arc::new(Mutex::new(HashSet::from(["mcp_loaded".to_string()])));
    let stream_fn = model.build_stream_fn_with_filter(
        vec![tool_def("mcp_loaded"), tool_def("mcp_hidden")],
        std::time::Duration::from_secs(5),
        Some("openai".to_string()),
        Some(loaded),
    );
    let mut stream = stream_fn(
        LlmContext {
            system_prompt: String::new(),
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
            asset_dir: None,
        },
        StreamOptions::from_signal(AbortSignal::new()),
    );
    while stream.next().await.is_some() {}

    let body = body_rx.await.unwrap();
    server.await.unwrap();
    let tool_names: Vec<_> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();

    assert!(tool_names.contains(&"mcp_loaded"));
    assert!(!tool_names.contains(&"mcp_hidden"));
}

// --- dirge-7ls: review-runner cache isolation regression --------

/// Phase 4 background review runner must NOT share the
/// `ToolCache` Arc with its parent agent. If it did, any
/// future memory/skill tool that invalidates the cache (or
/// any new tool added to the review allow-list) would
/// pollute the main agent's tool result cache mid-session.
///
/// This regression test asserts the architectural invariant
/// directly at the construction site: a freshly allocated
/// cache passed into `spawn_review_runner_with_cache`
/// remains distinct from the parent agent's cache.
/// `ToolCache::shares_storage_with` is `Arc::ptr_eq` on the
/// internal entries Arc, so a clone returns `true` and a
/// fresh `ToolCache::new()` returns `false`. Keep this test
/// pure / fast — no tokio runtime, no LLM call.
#[test]
fn review_runner_gets_isolated_cache_dirge_7ls() {
    let agent = build_openai_any_agent();
    let parent_cache = agent.cache().clone();

    // A fresh cache MUST NOT share storage with the parent.
    let fresh_cache = ToolCache::new();
    assert!(
        !fresh_cache.shares_storage_with(&parent_cache),
        "ToolCache::new() must produce a distinct Arc — review runner relies on this for isolation"
    );

    // The parent's clone, by contrast, SHARES storage —
    // this is the legacy behaviour we must NOT regress for
    // the main agent / subagent path.
    let parent_clone = parent_cache.clone();
    assert!(
        parent_clone.shares_storage_with(&parent_cache),
        "ToolCache::clone() must share storage — main-agent/subagent path depends on this"
    );

    // And: `cache.clear()` semantics on the main path are
    // preserved (the clone sees the clear). Guards against
    // accidental Arc unsharing during the dirge-7ls fix.
    parent_cache.set("key", "value".to_string());
    assert_eq!(parent_clone.get("key"), Some("value".to_string()));
    parent_cache.clear();
    assert!(parent_clone.get("key").is_none());
}

/// dirge-yai1 — the curator runner exposes ONLY the `skill` tool to
/// the LLM. Prompt-level guards say skill-only too, but a tool-level
/// filter is stronger: the model can't write memory entries even if
/// it tried. Tests the pure `filter_tool_names` helper that backs
/// both the review and curator paths so the filter shape is locked
/// in without needing a real `LoopTool` fixture.
#[test]
fn curator_runner_is_skill_only_dirge_yai1() {
    use super::filter_tool_names;

    // Simulate the registered loop_tools the agent would carry —
    // names match the real production registry.
    let registered_tools = [
        "read",
        "write",
        "edit",
        "bash",
        "grep",
        "find_files",
        "glob",
        "list_dir",
        "write_todo_list",
        "apply_patch",
        "session_search",
        "memory",
        "skill",
        "task",
        "question",
    ];
    let iter_names = || registered_tools.iter().copied();

    // Review filter: memory + skill. Mirrors the existing
    // post-session background review pass that writes to BOTH
    // stores.
    let review_filter = filter_tool_names(iter_names(), &["memory", "skill"]);
    assert_eq!(
        review_filter,
        vec!["memory".to_string(), "skill".to_string()],
        "review filter must be memory + skill in registration order"
    );

    // Curator filter: skill only. Memory FILTERED OUT.
    let curator_filter = filter_tool_names(iter_names(), &["skill"]);
    assert_eq!(
        curator_filter,
        vec!["skill".to_string()],
        "curator filter must contain ONLY skill — dirge-yai1"
    );
    assert!(
        !curator_filter.iter().any(|n| n == "memory"),
        "curator filter MUST NOT include memory — model cannot write entries even if it tried"
    );

    // Curator filter is a strict subset of review filter.
    for name in &curator_filter {
        assert!(
            review_filter.contains(name),
            "curator-only tool '{}' not in review filter — review must be a superset",
            name
        );
    }
    assert!(
        review_filter.len() > curator_filter.len(),
        "review filter must be strictly larger than curator filter"
    );

    // Tools outside the allow-list are filtered out — neither
    // pass should expose read/write/bash/etc to the LLM.
    for forbidden in ["read", "write", "edit", "bash", "task", "session_search"] {
        assert!(
            !review_filter.contains(&forbidden.to_string()),
            "review must not expose '{}'",
            forbidden
        );
        assert!(
            !curator_filter.contains(&forbidden.to_string()),
            "curator must not expose '{}'",
            forbidden
        );
    }
}

/// P3a (dirge-crrh): `filter_loop_tools` is the hard tool-allow-list every
/// forked phase agent relies on. It keeps exactly the allowed tools in
/// registration order and excludes everything else (so e.g. a reviewer fork
/// literally cannot reach `write`).
#[cfg(feature = "mcp")]
#[test]
fn filter_loop_tools_is_a_hard_allowlist() {
    use crate::agent::agent_loop::LoopTool;
    use std::sync::Arc;

    let tools: Vec<Arc<dyn LoopTool>> = vec![
        Arc::new(NamedTool("read")),
        Arc::new(NamedTool("grep")),
        Arc::new(NamedTool("write")),
        Arc::new(NamedTool("bash")),
    ];
    let names = |kept: &[Arc<dyn LoopTool>]| {
        kept.iter()
            .map(|t| t.name().to_string())
            .collect::<Vec<_>>()
    };

    // Read-only subset (explore/plan phase), order preserved.
    let kept = crate::provider::spawn::filter_loop_tools(&tools, &["read", "grep"]);
    assert_eq!(names(&kept), vec!["read", "grep"]);

    // Reviewer set (read + bash, NO write/edit).
    let kept = crate::provider::spawn::filter_loop_tools(&tools, &["read", "bash"]);
    assert_eq!(names(&kept), vec!["read", "bash"]);
    assert!(
        !names(&kept).iter().any(|n| n == "write"),
        "reviewer fork must not expose write"
    );

    // Unknown names and an empty allow-list both yield nothing.
    assert!(crate::provider::spawn::filter_loop_tools(&tools, &["nonexistent"]).is_empty());
    assert!(crate::provider::spawn::filter_loop_tools(&tools, &[]).is_empty());
}

/// dirge-ygm3: the review fork swaps its `memory` tool for the review-enabled
/// instance; other tools are untouched, and a set without `memory` is a no-op.
#[cfg(feature = "mcp")]
#[test]
fn swap_in_review_memory_replaces_only_the_memory_tool() {
    use crate::agent::agent_loop::LoopTool;
    use std::sync::Arc;

    let original_memory: Arc<dyn LoopTool> = Arc::new(NamedTool("memory"));
    let skill: Arc<dyn LoopTool> = Arc::new(NamedTool("skill"));
    // Distinct instance, same name — the review-enabled tool.
    let review_memory: Arc<dyn LoopTool> = Arc::new(NamedTool("memory"));

    let mut tools = vec![original_memory.clone(), skill.clone()];
    crate::provider::spawn::swap_in_review_memory(&mut tools, &review_memory);
    assert!(
        Arc::ptr_eq(&tools[0], &review_memory),
        "memory slot now points at the review tool",
    );
    assert!(
        !Arc::ptr_eq(&tools[0], &original_memory),
        "the original memory tool was replaced",
    );
    assert!(Arc::ptr_eq(&tools[1], &skill), "skill tool untouched");

    // No `memory` tool present → no-op (skill-only curator/phase fork).
    let mut skill_only = vec![skill.clone()];
    crate::provider::spawn::swap_in_review_memory(&mut skill_only, &review_memory);
    assert!(
        Arc::ptr_eq(&skill_only[0], &skill),
        "no memory tool → unchanged"
    );
}

/// dirge-z73i: `with_review_route` stashes the alternate stream_fn,
/// provider alias, and model name on AnyAgent so
/// `spawn_review_runner_with_cache` can pick them up. This is a pure
/// fixture test — verifies the setter records the values without
/// firing the full review runner (which would need a live client).
#[test]
fn with_review_route_stashes_alternate_route_dirge_z73i() {
    use crate::agent::agent_loop::message::StreamEvent;
    use std::sync::Arc;

    let agent = build_openai_any_agent();
    assert!(
        agent.review_stream_fn.is_none(),
        "fresh agent has no review route by default"
    );
    assert!(agent.review_provider_name.is_none());
    assert!(agent.review_model_name.is_none());

    // Build a dummy review stream_fn — just yields a single Error
    // event so we can verify identity (it's a different closure
    // from the main agent's stream_fn).
    let dummy: crate::agent::agent_loop::StreamFn = Arc::new(|_ctx, _opts| {
        Box::pin(futures::stream::iter(vec![StreamEvent::Error {
            error: "from-review-route".to_string(),
        }]))
    });

    let agent = agent.with_review_route(dummy.clone(), "glm".to_string(), "glm-4.6".to_string());
    assert!(agent.review_stream_fn.is_some(), "stream_fn stashed");
    assert_eq!(agent.review_provider_name.as_deref(), Some("glm"));
    assert_eq!(agent.review_model_name.as_deref(), Some("glm-4.6"));
}

/// dirge-008x: `with_summarizer` stashes the in-loop compaction
/// summarizer on AnyAgent (default `None`) so `spawn_runner` can forward
/// it to `LoopSpawnConfig.summarize_fn`. Setter-level fixture test — same
/// rationale as `with_review_route` above (firing the real fold needs a
/// live client; the loop-side consumption is covered by run_tests).
#[tokio::test]
async fn with_summarizer_stashes_summarize_fn_dirge_008x() {
    use std::sync::Arc;

    let agent = build_openai_any_agent();
    assert!(
        agent.summarize_fn.is_none(),
        "fresh AnyAgent::new agent has no summarizer by default"
    );

    let dummy: crate::agent::compression::SummarizeFn =
        Arc::new(|prompt: String| Box::pin(async move { Ok(format!("summary of: {prompt}")) }));
    let agent = agent.with_summarizer(dummy);
    let stashed = agent
        .summarize_fn
        .as_ref()
        .expect("summarizer stashed after with_summarizer");
    // The stashed fn is invocable and returns the summary text.
    let out = stashed("hello".to_string()).await.unwrap();
    assert_eq!(out, "summary of: hello");
}

#[test]
fn ui_compaction_model_honors_summarization_provider() {
    use std::collections::HashMap;

    let providers = HashMap::from([
        (
            "main".to_string(),
            crate::config::ProviderEntry {
                provider_type: Some("openai".to_string()),
                model: Some("gpt-main".to_string()),
                api_key: Some("sk-main".to_string()),
                ..Default::default()
            },
        ),
        (
            "summ".to_string(),
            crate::config::ProviderEntry {
                provider_type: Some("deepseek".to_string()),
                model: Some("deepseek-chat".to_string()),
                api_key: Some("sk-summ".to_string()),
                ..Default::default()
            },
        ),
    ]);
    let cfg = crate::config::Config {
        provider: Some("main".to_string()),
        summarization_provider: Some("summ".to_string()),
        providers: Some(providers),
        ..Default::default()
    };
    let main_client =
        crate::provider::create_client_with_auth("main", None, &cfg.providers_map(), cfg.auth)
            .expect("main client builds from literal API key");

    let model = crate::provider::build_compaction_model(&cfg, &main_client, "gpt-main")
        .expect("summarization_provider route resolves");

    assert!(
        matches!(model, crate::provider::AnyModel::DeepSeek(_)),
        "UI compaction must use summarization_provider, not the active session model"
    );
}

#[tokio::test]
async fn in_loop_compaction_refuses_anthropic_oauth_without_summarization_provider() {
    let cfg = crate::config::Config::default();
    let client = rig::providers::anthropic::Client::builder()
        .api_key("sk-ant-oat-test")
        .http_client(crate::provider::anthropic_http::AnthropicHttpClient::new(
            "sk-ant-oat-test".to_string(),
        ))
        .build()
        .expect("Anthropic OAuth client builds");
    let model = crate::provider::AnyModel::AnthropicOauth(client.completion_model("claude-sonnet"));

    let summarize = crate::provider::build_summarize_fn(&cfg, model);
    let err = summarize("prompt".to_string())
        .await
        .expect_err("Anthropic OAuth must not be used for in-loop side compaction");

    assert!(
        err.to_string().contains("summarization_provider"),
        "error should tell the user to configure summarization_provider: {err}"
    );
}

#[test]
fn ui_compaction_refuses_anthropic_oauth_without_summarization_provider() {
    let cfg = crate::config::Config::default();
    let main_client = crate::provider::AnyClient::AnthropicOauth(
        rig::providers::anthropic::Client::builder()
            .api_key("sk-ant-oat-test")
            .http_client(crate::provider::anthropic_http::AnthropicHttpClient::new(
                "sk-ant-oat-test".to_string(),
            ))
            .build()
            .expect("Anthropic OAuth client builds"),
    );

    let err = match crate::provider::build_compaction_model(&cfg, &main_client, "claude-sonnet") {
        Ok(_) => panic!("Anthropic OAuth must not be used for side compaction"),
        Err(err) => err,
    };

    assert!(
        err.to_string().contains("summarization_provider"),
        "error should tell the user to configure summarization_provider: {err}"
    );
}

#[test]
fn oauth_compaction_disabled_error_is_detected_through_context_wrapping() {
    use anyhow::Context;

    // The disabled-compaction error is the routing key the reactive-overflow
    // handler uses to switch to prune-only fallback. It must stay detectable
    // even if a caller wraps it with extra context — `anyhow`'s `to_string()`
    // only shows the outermost message, so a naive top-level match would miss
    // a wrapped error and silently drop the prune-only fallback.
    let bare = anyhow::anyhow!(crate::provider::ANTHROPIC_OAUTH_COMPACTION_DISABLED);
    assert!(
        crate::provider::is_anthropic_oauth_compaction_disabled_error(&bare),
        "bare disabled-compaction error must be detected"
    );

    let wrapped = Err::<(), _>(bare)
        .context("preparing compaction")
        .unwrap_err();
    assert!(
        crate::provider::is_anthropic_oauth_compaction_disabled_error(&wrapped),
        "disabled-compaction error must be detected through a context wrapper"
    );

    let unrelated = anyhow::anyhow!("some other failure").context("preparing compaction");
    assert!(
        !crate::provider::is_anthropic_oauth_compaction_disabled_error(&unrelated),
        "unrelated errors must not be misrouted to the prune-only fallback"
    );
}

// --- C6/C7: compaction prefix is full + includes tool calls -----

use super::summarize;
use crate::session::{MessageRole, SessionMessage, ToolCallEntry, ToolCallState};
use compact_str::CompactString;

fn sm(role: MessageRole, content: &str, tool_calls: Vec<ToolCallEntry>) -> SessionMessage {
    SessionMessage {
        role,
        content: CompactString::from(content),
        estimated_tokens: 0,
        id: CompactString::from("test-id"),
        timestamp: 0,
        tool_calls,
        images: Vec::new(),
    }
}

/// C7: assistant tool calls land in the serialized form with
/// args + result. Previously they were dropped entirely so the
/// summarizer saw only `[Assistant]: <text>` with no record
/// that bash/read/edit ever ran.
#[test]
fn serialize_conversation_includes_tool_calls() {
    let msgs = vec![
        sm(MessageRole::User, "list rust files", vec![]),
        sm(
            MessageRole::Assistant,
            "I'll find them.",
            vec![ToolCallEntry {
                id: "call_1".into(),
                name: "find_files".into(),
                args: serde_json::json!({"pattern": "*.rs"}),
                state: ToolCallState::Completed {
                    result: "src/main.rs\nsrc/lib.rs".into(),
                },
            }],
        ),
    ];
    let out = summarize::serialize_conversation(&msgs);
    assert!(out.contains("[User]"), "missing role tag: {out}");
    assert!(
        out.contains("[Tool: find_files("),
        "missing tool call line: {out}"
    );
    assert!(
        out.contains("src/main.rs"),
        "missing tool result content: {out}"
    );
}

/// C7: interrupted + failed tool calls also surface.
#[test]
fn serialize_conversation_marks_interrupted_and_failed() {
    let msgs = vec![sm(
        MessageRole::Assistant,
        "trying",
        vec![
            ToolCallEntry {
                id: "a".into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "sleep 9999"}),
                state: ToolCallState::Interrupted,
            },
            ToolCallEntry {
                id: "b".into(),
                name: "read".into(),
                args: serde_json::json!({"path": "/missing"}),
                state: ToolCallState::Failed {
                    error: "no such file".into(),
                },
            },
        ],
    )];
    let out = summarize::serialize_conversation(&msgs);
    assert!(out.contains("<interrupted>"), "got: {out}");
    assert!(out.contains("<failed: no such file>"), "got: {out}");
}

/// C7 bound: a single tool result over the per-tool cap (2KB)
/// truncates with a marker, preserving structure of the rest
/// of the conversation.
#[test]
fn serialize_conversation_truncates_huge_tool_results() {
    let big: String = "x".repeat(5000);
    let msgs = vec![sm(
        MessageRole::Assistant,
        "huge",
        vec![ToolCallEntry {
            id: "c".into(),
            name: "grep".into(),
            args: serde_json::json!({"pattern": "."}),
            state: ToolCallState::Completed { result: big },
        }],
    )];
    let out = summarize::serialize_conversation(&msgs);
    assert!(
        out.contains("(truncated, 5000 bytes total)"),
        "expected truncation marker; got: {out}"
    );
}

/// C6: a long full-conversation prefix is NOT truncated by the
/// caller-side 6000-char cap any more. compress_messages no
/// longer slices `conversation`; the full string reaches the
/// summarizer. Regression test the unchanged-passthrough via
/// serialize_conversation's length on a large input.
#[test]
fn serialize_conversation_returns_full_prefix() {
    let msgs: Vec<SessionMessage> = (0..200)
        .map(|i| sm(MessageRole::Assistant, &format!("turn {i}"), vec![]))
        .collect();
    let out = summarize::serialize_conversation(&msgs);
    // 200 turns × ~10 chars each = ~2000 chars; below the old
    // 6000 cap but the principle still holds: the function is
    // a pure mapper, no length cap. Confirm by checking the
    // last turn is present.
    assert!(out.contains("turn 199"), "tail must be present: {out}");
    assert!(out.contains("turn 0"), "head must be present: {out}");
}

// ============================================================
// PROV-1: Custom-provider validation tests
// ============================================================

/// Custom provider with https base_url is accepted.
#[test]
fn custom_provider_https_is_allowed() {
    let custom = std::collections::HashMap::from([(
        "my-proxy".to_string(),
        ProviderEntry {
            provider_type: Some("custom".to_string()),
            base_url: Some("https://my-proxy.example.com/v1".to_string()),
            ..Default::default()
        },
    )]);
    let result = resolve_provider_info("my-proxy", &custom);
    assert!(result.is_some(), "https provider should resolve");
}

/// Custom provider with http base_url is rejected unless allow_insecure.
#[test]
fn custom_provider_http_rejected_without_allow_insecure() {
    let custom = std::collections::HashMap::from([(
        "bad-proxy".to_string(),
        ProviderEntry {
            provider_type: Some("custom".to_string()),
            base_url: Some("http://bad-proxy.example.com/v1".to_string()),
            ..Default::default()
        },
    )]);
    let result = resolve_provider_info("bad-proxy", &custom);
    assert!(
        result.is_none(),
        "http provider without allow_insecure should be rejected"
    );
}

/// Custom provider with http base_url + allow_insecure: true is accepted.
#[test]
fn custom_provider_http_allowed_with_allow_insecure() {
    let custom = std::collections::HashMap::from([(
        "local-ollama".to_string(),
        ProviderEntry {
            provider_type: Some("custom".to_string()),
            base_url: Some("http://localhost:11434/v1".to_string()),
            allow_insecure: true,
            multimodal: None,
            ..Default::default()
        },
    )]);
    let result = resolve_provider_info("local-ollama", &custom);
    assert!(
        result.is_some(),
        "http provider with allow_insecure should be accepted"
    );
}

/// dirge-j3jd: a custom alias backed by an `openai` provider_type must get
/// OpenAI's default model, not the OpenRouter `vendor/model` fallback.
#[test]
fn default_model_for_entry_resolves_alias_provider_type() {
    let entry = ProviderEntry {
        provider_type: Some("openai".to_string()),
        base_url: Some("https://proxy.internal/v1".to_string()),
        ..Default::default()
    };
    assert_eq!(default_model_for_entry("my-openai", &entry), "gpt-4o");

    let anthropic = ProviderEntry {
        provider_type: Some("anthropic".to_string()),
        ..Default::default()
    };
    assert_eq!(
        default_model_for_entry("work-claude", &anthropic),
        "claude-sonnet-4-6"
    );
}

/// dirge-j3jd: `default_model_for_alias` looks the entry up in the
/// providers map; a custom alias resolves to its backend default, while an
/// undeclared (built-in) name still resolves directly.
#[test]
fn default_model_for_alias_uses_map_then_builtin_fallback() {
    let providers = HashMap::from([(
        "my-openai".to_string(),
        ProviderEntry {
            provider_type: Some("openai".to_string()),
            ..Default::default()
        },
    )]);
    // Custom alias → resolved via entry → OpenAI default.
    assert_eq!(default_model_for_alias("my-openai", &providers), "gpt-4o");
    // Undeclared name that IS a built-in → direct resolution.
    assert_eq!(
        default_model_for_alias("anthropic", &providers),
        "claude-sonnet-4-6"
    );
    // The bare alias WITHOUT the map would have wrongly fallen back here:
    assert_eq!(default_model_for("my-openai"), "deepseek/deepseek-v4-flash");
}

/// dirge-8sku: an UNTRUSTED plugin shadowing a built-in name is still
/// rejected (collision guard ENFORCED) — guards against credential
/// interception. Tested directly via the validator since the plugin
/// registry is a process-global OnceLock.
#[test]
fn plugin_provider_builtin_name_collision_rejected() {
    let res = validate_custom_provider(
        "openai",
        "https://evil.example.com/v1",
        false,
        /* enforce_builtin_collision */ true,
    );
    assert!(
        res.is_err(),
        "plugin shadowing a built-in name must be rejected"
    );
    assert!(res.unwrap_err().contains("collides with built-in"));
}

/// dirge-8sku: a CONFIG-declared alias of a built-in name with a custom
/// base_url is the documented, trusted use (e.g. `ollama` → openai
/// backend + local proxy) and must be ACCEPTED — previously it was wrongly
/// rejected as a collision, contradicting docs/config.md.
#[test]
fn config_alias_of_builtin_name_with_base_url_is_accepted() {
    let providers = std::collections::HashMap::from([(
        "ollama".to_string(),
        ProviderEntry {
            provider_type: Some("openai".to_string()),
            base_url: Some("http://localhost:11434/v1".to_string()),
            allow_insecure: true,
            multimodal: None,
            ..Default::default()
        },
    )]);
    let result = resolve_provider_info("ollama", &providers);
    assert!(
        result.is_some(),
        "config-declared alias of a built-in name should be accepted"
    );
    let info = result.unwrap();
    assert_eq!(info.kind, ProviderKind::OpenAI);
    assert_eq!(info.base_url.as_deref(), Some("http://localhost:11434/v1"));
}

/// dirge-8sku: the URL-scheme check still applies to config aliases —
/// the collision guard is skipped, NOT all validation.
#[test]
fn config_alias_still_enforces_url_scheme() {
    let res = validate_custom_provider(
        "openai",
        "http://evil.example.com/v1", // insecure, non-local
        false,                        // allow_insecure = false
        /* enforce_builtin_collision */ false,
    );
    assert!(
        res.is_err(),
        "config alias must still reject insecure http:// without allow_insecure"
    );
    assert!(res.unwrap_err().contains("insecure base_url"));
}

// ============================================================
// dirge-u13u: compaction prompt-injection defense
// ============================================================

/// If any of the messages contains the literal untrusted-material
/// delimiter, `compress_messages` must bail BEFORE issuing an LLM call
/// (we use a bogus base URL to prove no network is touched — if the
/// check failed open, the test would hit the URL and fail with a
/// connection error instead of the expected "reserved delimiter"
/// error).
#[test]
fn compaction_rejects_input_containing_delimiter() {
    // dirge-tv3p: the delimiter/injection check moved into the pure,
    // synchronous `build_compaction_prompt` (the on-thread half), so we test
    // it directly — no client/network needed.
    let poisoned = format!(
        "innocent text {} attacker payload {} more",
        crate::agent::prompt::COMPACTION_DELIMITER_OPEN,
        crate::agent::prompt::COMPACTION_DELIMITER_CLOSE,
    );
    let msgs = vec![sm(MessageRole::User, &poisoned, vec![])];

    let result = crate::provider::build_compaction_prompt(&msgs, None, None);

    assert!(
        result.is_err(),
        "compaction must reject input containing the reserved delimiter"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("reserved delimiter"),
        "error should mention the reserved-delimiter reason, got: {err}"
    );
}

/// Sanity: clean input passes the delimiter check and yields a prompt. This
/// confirms the check is precisely scoped and isn't over-rejecting innocuous
/// content.
#[test]
fn compaction_passes_check_on_clean_input() {
    let msgs = vec![sm(
        MessageRole::User,
        "ordinary message, no markers",
        vec![],
    )];

    let result = crate::provider::build_compaction_prompt(&msgs, None, None);

    assert!(
        result.is_ok(),
        "clean input must NOT trip the delimiter check, got: {:?}",
        result.err()
    );
}

// ============================================================
// dirge-ffwa: background MCP tool injection + dynamic_tool_search
// ============================================================

/// Minimal LoopTool fixture — only `name()` matters for these tests;
/// `execute` is never called.
#[cfg(feature = "mcp")]
#[derive(Debug)]
struct NamedTool(&'static str);

#[cfg(feature = "mcp")]
impl crate::agent::agent_loop::LoopTool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "test"
    }
    fn label(&self) -> &str {
        "test"
    }
    fn parameters(&self) -> &serde_json::Value {
        static EMPTY: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
    }
    fn execute<'a>(
        &'a self,
        _id: &'a str,
        _args: serde_json::Value,
        _signal: crate::agent::agent_loop::tool::AbortSignal,
        _on_update: crate::agent::agent_loop::tool::LoopToolUpdate,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<crate::agent::agent_loop::LoopToolResult, String>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move { Ok(crate::agent::agent_loop::LoopToolResult::default()) })
    }
}

/// dirge-tpx6: with `dynamic_tool_search` on, background-injected MCP
/// tools must be appended to the live `tool_search` registry so the model
/// can DISCOVER them — but NOT force-loaded, so they stay search-gated
/// (don't ship in every request) exactly like build-time MCP tools.
#[cfg(feature = "mcp")]
#[test]
fn extend_loop_tools_adds_injected_to_search_registry_not_loaded() {
    use crate::agent::tools::tool_search::ToolMeta;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    let filter: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let registry: Arc<Mutex<Vec<ToolMeta>>> = Arc::new(Mutex::new(Vec::new()));
    let mut agent =
        build_openai_any_agent().with_dynamic_tool_search(filter.clone(), registry.clone());

    let tools: Vec<Arc<dyn crate::agent::agent_loop::LoopTool>> = vec![
        Arc::new(NamedTool("mcp_alpha")),
        Arc::new(NamedTool("mcp_beta")),
    ];
    agent.extend_loop_tools(tools);

    // Appended to the live dispatch registry…
    assert_eq!(agent.loop_tools.len(), 2);
    // …and to the SEARCHABLE registry so `tool_search` can surface them…
    let reg = registry.lock().unwrap();
    assert!(
        reg.iter().any(|m| m.name == "mcp_alpha"),
        "reg missing alpha"
    );
    assert!(reg.iter().any(|m| m.name == "mcp_beta"), "reg missing beta");
    // …but NOT force-loaded: they stay search-gated (not in every request).
    assert!(
        filter.lock().unwrap().is_empty(),
        "injected tools must not be pre-loaded — discovered via tool_search"
    );
}

/// When `dynamic_tool_search` is OFF (no registry), injection still grows
/// the dispatch registry and touches no search state.
#[cfg(feature = "mcp")]
#[test]
fn extend_loop_tools_without_dynamic_search_only_grows_registry() {
    use std::sync::Arc;

    let mut agent = build_openai_any_agent(); // tool_search_registry == None
    let tools: Vec<Arc<dyn crate::agent::agent_loop::LoopTool>> = vec![Arc::new(NamedTool("x"))];
    agent.extend_loop_tools(tools);

    assert_eq!(agent.loop_tools.len(), 1);
    assert!(agent.tool_def_filter.is_none());
    assert!(agent.tool_search_registry.is_none());
}

/// dirge-tpx6 end-to-end: a background-injected tool, under
/// `dynamic_tool_search`, travels the WHOLE path through the real
/// request-def + filter functions: built into the per-request def list →
/// HIDDEN until discovered → discoverable by `tool_search` ranking the
/// live registry → VISIBLE once the loaded-set is marked → dispatchable
/// by name. Composes the pieces no single unit test covers.
#[cfg(feature = "mcp")]
#[test]
fn injected_tool_is_gated_then_visible_then_dispatchable() {
    use crate::agent::agent_loop::loop_tool_to_rig_definition;
    use crate::agent::agent_loop::rig_stream_factory::filter_tool_defs;
    use crate::agent::tools::tool_search::{ToolMeta, rank_tools};
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    let filter: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let registry: Arc<Mutex<Vec<ToolMeta>>> = Arc::new(Mutex::new(Vec::new()));
    let mut agent =
        build_openai_any_agent().with_dynamic_tool_search(filter.clone(), registry.clone());

    // Background injection of an MCP-style tool.
    agent.extend_loop_tools(vec![Arc::new(NamedTool("mcp_demo"))]);

    // The per-request tool-def list `spawn_runner` builds from loop_tools
    // includes it (so dispatch can resolve it once the model calls it).
    let defs: Vec<_> = agent
        .loop_tools
        .iter()
        .map(|t| loop_tool_to_rig_definition(t.as_ref()))
        .collect();
    assert!(
        defs.iter().any(|d| d.name == "mcp_demo"),
        "injected tool must be in the def list"
    );

    // GATED: before discovery the request filter hides it (not loaded).
    let before = filter_tool_defs(&defs, Some(&filter));
    assert!(
        !before.iter().any(|d| d.name == "mcp_demo"),
        "must be hidden until discovered via tool_search"
    );

    // DISCOVERABLE: tool_search ranks the LIVE registry and finds it.
    {
        let reg = registry.lock().unwrap();
        let hits = rank_tools(&reg, "mcp_demo", 5);
        assert!(
            hits.iter().any(|m| m.name == "mcp_demo"),
            "tool_search must be able to discover the injected tool"
        );
    }

    // tool_search marks a hit loaded — simulate that single effect.
    filter.lock().unwrap().insert("mcp_demo".to_string());

    // VISIBLE: now the def ships on the next request.
    let after = filter_tool_defs(&defs, Some(&filter));
    assert!(
        after.iter().any(|d| d.name == "mcp_demo"),
        "must ship in the request once discovered"
    );

    // DISPATCHABLE: the loop resolves the call by name in loop_tools.
    assert!(
        agent.loop_tools.iter().any(|t| t.name() == "mcp_demo"),
        "dispatch must find the tool by name"
    );
}
