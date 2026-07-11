//! Agent construction and auxiliary-route wiring.
//!
//! Split out of `provider/mod.rs` (dirge-4y4l): the dependency-injection
//! seam that turns a resolved [`AnyModel`] + config into a fully wired
//! [`AnyAgent`], plus the standalone stream-fn / callback builders for
//! the escalation, critic, approval, and background-review routes. The
//! `AnyAgent` type and its methods live in the parent module; this file
//! only orchestrates the builders.

use std::collections::HashMap;

use crate::agent::builder;
use crate::cli::Cli;
use crate::config::{Config, ProviderAuth, ProviderEntry};
use crate::context::ContextFiles;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::sandbox::Sandbox;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;

use crate::agent::tools::plan::PlanSwitchSender;
use crate::agent::tools::question::QuestionSender;

use super::{
    AnyAgent, AnyAgentInner, AnyClient, AnyModel, client, default_model_for_entry, summarize,
};

pub(crate) const ANTHROPIC_OAUTH_COMPACTION_DISABLED: &str = concat!(
    "Anthropic OAuth is not used for compaction side-LLM calls; ",
    "configure `summarization_provider` to a non-Anthropic-OAuth provider",
);

pub(crate) fn is_anthropic_oauth_compaction_disabled_error(err: &anyhow::Error) -> bool {
    // Walk the full source chain, not just the outermost message: `anyhow`'s
    // `to_string()` shows only the top context, so a `bail!` wrapped with
    // `.context(...)` would otherwise escape detection and skip the prune-only
    // fallback this error is meant to route to.
    err.chain().any(|cause| {
        cause
            .to_string()
            .contains(ANTHROPIC_OAUTH_COMPACTION_DISABLED)
    })
}

fn openai_api_billing_fallback_key(cli: &Cli) -> Option<&str> {
    cli.resolved_api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .or_else(|| cli.api_key.as_deref().filter(|key| !key.is_empty()))
}

/// dirge-ovjk: resolve the model name for a role provider's client. `entry.model`
/// carries the explicit-vs-default signal (Some = the user set it), so a
/// codex-authed role provider with no model still resolves to the Codex default
/// while an explicit `gpt-4o` is honored — the same rule the main session model
/// follows. Every role-client site (critic, review, escalation, summarization,
/// subagent, approval) goes through here so `completion_model` never has to
/// remap.
fn resolve_entry_model_name(client: &AnyClient, alias: &str, entry: &ProviderEntry) -> String {
    let requested = entry
        .model
        .clone()
        .unwrap_or_else(|| default_model_for_entry(alias, entry).to_string());
    super::resolve_model_name(client, &requested, entry.model.is_some())
}

#[cfg(test)]
pub fn create_client(
    provider_name: &str,
    api_key: Option<&str>,
    providers: &HashMap<String, ProviderEntry>,
) -> anyhow::Result<AnyClient> {
    client::create_client(provider_name, api_key, providers)
}

pub fn create_client_with_auth(
    provider_name: &str,
    api_key: Option<&str>,
    providers: &HashMap<String, ProviderEntry>,
    default_auth: Option<crate::config::ProviderAuth>,
) -> anyhow::Result<AnyClient> {
    client::create_client_with_auth(provider_name, api_key, providers, default_auth)
}

fn create_role_client(
    provider_name: &str,
    providers: &HashMap<String, ProviderEntry>,
    default_auth: Option<ProviderAuth>,
) -> anyhow::Result<AnyClient> {
    create_client_with_auth(provider_name, None, providers, default_auth)
}

// Arity matches `build_agent_inner` — explicit DI signature kept
// grep-able, refactoring into a struct is tracked separately.
#[allow(clippy::too_many_arguments)]
pub async fn build_agent(
    model: AnyModel,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    question_tx: Option<QuestionSender>,
    plan_tx: Option<PlanSwitchSender>,
    bg_store: Option<crate::agent::tools::background::BackgroundStore>,
    #[cfg(feature = "lsp")] lsp_manager: Option<std::sync::Arc<crate::lsp::manager::LspManager>>,
    sandbox: Sandbox,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    // Live session id forwarded to SessionSearchTool so the model's
    // session_search calls exclude the current session. See dirge-502b.
    session_id: Option<String>,
) -> AnyAgent {
    let parent_model = model.clone();
    // Resolve the per-provider chunk timeout once here so every
    // spawn_runner / run_print call on the resulting agent uses the
    // same value. Provider name comes from the resolved CLI / config
    // (already factored into resolve_provider above the call site).
    let provider_name = cli.resolve_provider(cfg);
    let chunk_timeout = cfg.resolve_stream_chunk_timeout(&provider_name);
    // Capture the model identifier before `match model` consumes
    // it — forwarded into `AnyAgent.model_name` so `spawn_runner`
    // can plumb it through to the `tool_input_repair` telemetry.
    let model_name = parent_model.name();

    // dirge-nw25: the model `task`-spawned subagents default to. When
    // `subagent_provider` is configured (and differs from the default
    // route) this is its model; otherwise the main model. Only the
    // `TaskTool` in `build_loop_tools` consumes `parent_model`, so routing
    // here is sufficient. A `task(agent=…)` profile model still overrides.
    let subagent_model = resolve_subagent_model(cfg);
    let loop_task_model = subagent_model.unwrap_or_else(|| parent_model.clone());

    macro_rules! build_inner {
        ($m:expr, $variant:ident) => {{
            // Clone params before consuming them in
            // build_agent_inner so build_loop_tools has fresh
            // copies. PermCheck / AskSender / Sandbox / Arc<...>
            // are all Clone-cheap.
            let permission_for_loop = permission.clone();
            let ask_tx_for_loop = ask_tx.clone();
            let question_tx_for_loop = question_tx.clone();
            let plan_tx_for_loop = plan_tx.clone();
            let bg_store_for_loop = bg_store.clone();
            let sandbox_for_loop = sandbox.clone();
            // dirge-nw25: the loop's TaskTool gets the subagent-routed model
            // (subagent_provider when set, else the main model).
            let parent_model_for_loop = Some(loop_task_model.clone());
            #[cfg(feature = "lsp")]
            let lsp_for_loop = lsp_manager.clone();

            // build_agent_inner now only needs model + cli/cfg/context for the
            // preamble — all tool wiring flows to build_loop_tools below
            // [dirge-tfip]. The ACTIVE model name + provider are passed
            // explicitly so model-family steering tracks /model and /agent
            // swaps instead of the launch-time CLI model (dirge-5db6).
            let (agent, cache, memory_provider) =
                builder::build_agent_inner($m, cli, cfg, context, &provider_name, &model_name)
                    .await;

            // Phase 4.5h-6: also build the LoopTool registry the
            // new agent_loop path dispatches against. Tools share
            // the same cache as the rig path (tool result
            // dedup) — though after h-6 the rig path no longer
            // runs, so this is effectively single-owner.
            //
            // Phase-3: build_loop_tools returns `(tools,
            // tool_def_filter)`. When `cfg.dynamic_tool_search`
            // is on, `tool_def_filter` is `Some` and a
            // `ToolSearchTool` has been registered inside `tools`
            // with the same Arc.
            let (loop_tools, dyn_search, review_memory_tool) = builder::build_loop_tools(
                cache.clone(),
                permission_for_loop,
                ask_tx_for_loop,
                question_tx_for_loop,
                plan_tx_for_loop,
                bg_store_for_loop,
                #[cfg(feature = "lsp")]
                lsp_for_loop,
                sandbox_for_loop,
                parent_model_for_loop,
                #[cfg(feature = "mcp")]
                mcp_manager,
                #[cfg(feature = "semantic")]
                semantic_manager,
                cli,
                cfg,
                session_id.clone(),
            )
            .await;

            // Phase 4.5h-6: extract the rig Agent's preamble so
            // the new path can pass it as Context.system_prompt.
            // rig's Agent has `preamble: Option<String>` public.
            // Phase-3: when dynamic-tool-search is on, append a
            // one-liner nudge so the model knows to call
            // `tool_search` before reaching for unknown tools.
            let mut preamble = agent.preamble.clone().unwrap_or_default();
            if dyn_search.is_some() {
                if !preamble.is_empty() {
                    preamble.push_str("\n\n");
                }
                preamble.push_str(crate::agent::prompt::DYNAMIC_TOOL_SEARCH_PROMPT);
            }

            let mut agent = AnyAgent::new(
                AnyAgentInner::$variant(agent),
                cache,
                chunk_timeout,
                loop_tools,
                preamble,
                model_name.clone(),
            );
            // dirge-7tvq: attach the memory provider so session-end
            // and pre-compress hooks can dispatch through the trait.
            if let Some(provider) = memory_provider {
                agent = agent.with_memory_provider(provider);
            }
            // dirge-ygm3: stash the review-enabled memory tool so the review
            // runner can swap it in (it's not in the main loop-tool set).
            if let Some(tool) = review_memory_tool {
                agent = agent.with_review_memory_tool(tool);
            }
            if let Some(ds) = dyn_search {
                agent.with_dynamic_tool_search(ds.filter, ds.registry)
            } else {
                agent
            }
        }};
    }

    let mut agent = match model {
        AnyModel::OpenRouter(m) => build_inner!(m, OpenRouter),
        AnyModel::OpenAI(m) => build_inner!(m, OpenAI),
        AnyModel::ChatGptOpenAI(m) => build_inner!(m, ChatGptOpenAI),
        AnyModel::OpenAICodex(m) => build_inner!(m, OpenAICodex),
        AnyModel::Anthropic(m) => build_inner!(m, Anthropic),
        AnyModel::AnthropicOauth(m) => build_inner!(m, AnthropicOauth),
        AnyModel::Gemini(m) => build_inner!(m, Gemini),
        AnyModel::DeepSeek(m) => build_inner!(m, DeepSeek),
        AnyModel::Glm(m) => build_inner!(m, Glm),
        AnyModel::OpenCode(m) => build_inner!(m, OpenCode),
        AnyModel::Ollama(m) => build_inner!(m, Ollama),
        AnyModel::Custom(m) => build_inner!(m, Custom),
    };

    if matches!(parent_model, AnyModel::OpenAICodex(_)) {
        match client::create_openai_api_key_fallback_client(
            &provider_name,
            openai_api_billing_fallback_key(cli),
            &cfg.providers_map(),
        ) {
            Ok(Some(fallback_client)) => {
                let fallback_model = fallback_client.completion_model(model_name.clone());
                agent = agent.with_openai_api_key_billing_fallback(fallback_model, ask_tx.clone());
                tracing::info!(
                    target: "dirge::provider",
                    provider = %provider_name,
                    model = %model_name,
                    "OpenAI API-key billing fallback armed; requires user confirmation before use",
                );
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    target: "dirge::provider",
                    provider = %provider_name,
                    error = %err,
                    "failed to arm OpenAI API-key billing fallback",
                );
            }
        }
    }

    // dirge-008x + dirge-nw25: wire the in-loop LLM compaction summarizer.
    // The proactive folds in `run_agent_loop` need a `SummarizeFn` to call
    // a model; without one they degrade to a prune-only pass. Prefer the
    // configured `summarization_provider` (so that role key is actually
    // consumed, not just advertised); otherwise fall back to the main
    // model. Either way adapts `summarize_with_model` (AnyModel + prompt →
    // summary) to the `SummarizeFn` shape.
    {
        let summarize_fn = build_summarize_fn(cfg, parent_model.clone());
        agent = agent.with_summarizer(summarize_fn);
    }

    // Phase 4 part 1 — dual-client escalation wiring.
    //
    // When the user has configured `escalation_provider` AND it
    // resolves to a DIFFERENT (alias, entry) than `ConfigRole::Default`,
    // build a second StreamFn that the loop will swap to for ONE call
    // after a repair-exhaustion or tree-sitter syntactic failure.
    //
    // The escalation route reuses:
    //   - The same tool definitions as the default loop (we just
    //     need a different model behind them).
    //   - The same chunk timeout — escalation should not be
    //     stricter or laxer than the default for stream chunk
    //     health.
    //
    // If `escalation_provider` is configured but the alias doesn't
    // resolve to a present entry AND isn't a built-in (this means
    // `resolve_role` returns None), surface an error rather than
    // silently disabling — the user asked for a feature and we
    // owe them a clear failure mode.
    if cfg.escalation_provider.is_some() {
        let default_role = cfg.resolve_role(crate::config::ConfigRole::Default);
        let escalation_role = cfg.resolve_role(crate::config::ConfigRole::Escalation);
        match (default_role, escalation_role) {
            (Some((default_alias, _)), Some((escalation_alias, escalation_entry))) => {
                // Equal aliases (case-insensitive) → escalation
                // has no effect; skip the duplicate client.
                if default_alias.eq_ignore_ascii_case(&escalation_alias) {
                    tracing::debug!(
                        target: "dirge::provider",
                        alias = %escalation_alias,
                        "escalation provider equals default; skipping duplicate client construction",
                    );
                } else {
                    match build_escalation_stream_fn(
                        &escalation_alias,
                        &escalation_entry,
                        &cfg.providers_map(),
                        cfg.auth,
                        chunk_timeout,
                        agent.loop_tools(),
                    ) {
                        Ok(stream_fn) => {
                            agent = agent.with_escalation(stream_fn, escalation_alias.clone());
                            tracing::info!(
                                target: "dirge::provider",
                                alias = %escalation_alias,
                                "dual-client escalation wired",
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                target: "dirge::provider",
                                alias = %escalation_alias,
                                error = %e,
                                "failed to construct escalation client; running without escalation",
                            );
                            eprintln!(
                                "warning: escalation_provider '{}' configured but client build failed: {}",
                                escalation_alias, e
                            );
                        }
                    }
                }
            }
            (_, None) => {
                // escalation_provider was set but resolve_role
                // returned None — alias doesn't name a present
                // entry and isn't a built-in. Hard-fail loudly per
                // the plan: don't silently disable.
                let alias = cfg.escalation_provider.clone().unwrap_or_default();
                tracing::error!(
                    target: "dirge::provider",
                    alias = %alias,
                    "escalation_provider configured but alias does not resolve to a known provider",
                );
                eprintln!(
                    "error: escalation_provider '{}' is configured but does not match any entry \
                     in `providers` or any built-in (anthropic/openai/deepseek/glm/gemini/ollama/openrouter). \
                     Either add it under `providers` or remove the `escalation_provider` setting.",
                    alias
                );
            }
            (None, _) => {
                // Default itself isn't resolvable — let the
                // caller's "no provider" error path handle it.
            }
        }
    }

    // F6 tier 3 — bounded critic + goal-gate wiring. Opt-in: only when the
    // user has set `critic_provider`. `resolve_role(Critic)` has no default
    // fallback, so an unset provider means no critic / goal gate (no cost).
    //
    // The critic and goal gate are DECOUPLED: they share one judge client
    // but bake DIFFERENT system preambles — the critic under the (possibly
    // overridden) critic preamble, the goal gate under its own fixed
    // GOAL_PREAMBLE. A prompt may suppress the critic (`critic: false`)
    // without affecting the goal gate.
    if cfg.critic_provider.is_some() {
        match cfg.resolve_role(crate::config::ConfigRole::Critic) {
            Some((alias, entry)) => {
                // Resolve the active prompt's critic settings:
                //   critic: false   → suppress the critic (goal gate unaffected)
                //   critic_preamble → override config + built-in for this prompt
                let active_prompt = context
                    .current_prompt_name
                    .as_deref()
                    .and_then(|name| context.prompts.get(name));
                let critic_disabled = active_prompt.and_then(|p| p.critic) == Some(false);
                // dirge-iyf5: the diff-aware reviewer's engagement mode.
                // A prompt-level `code_review` front-matter value wins over
                // the config-level `code_review`; an unrecognized prompt
                // value falls back to the config resolution.
                let code_review_mode = active_prompt
                    .and_then(|p| p.code_review.as_deref())
                    .and_then(crate::agent::agent_loop::types::CodeReviewMode::from_wire)
                    .unwrap_or_else(|| cfg.resolve_code_review_mode());
                let critic_preamble: std::sync::Arc<str> =
                    match active_prompt.and_then(|p| p.critic_preamble.as_deref()) {
                        Some(p) => std::sync::Arc::from(p),
                        None => std::sync::Arc::from(cfg.resolve_critic_preamble()),
                    };
                let providers = cfg.providers_map();
                match create_role_client(&alias, &providers, cfg.auth) {
                    Ok(raw_client) => {
                        let client = std::sync::Arc::new(raw_client);
                        let model_name = resolve_entry_model_name(&client, &alias, &entry);
                        // Goal gate: always wired (fires only with --goal),
                        // judged under its OWN fixed preamble — decoupled
                        // from any critic override.
                        agent = agent.with_goal_fn(build_judge_fn(
                            client.clone(),
                            model_name.clone(),
                            "critic",
                            std::sync::Arc::from(crate::agent::agent_loop::goal::GOAL_PREAMBLE),
                        ));
                        // Critic: wired unless the active prompt disables it.
                        if !critic_disabled {
                            agent = agent.with_critic(build_judge_fn(
                                client.clone(),
                                model_name.clone(),
                                "critic",
                                critic_preamble.clone(),
                            ));
                            // dirge-iyf5: the diff-aware code reviewer shares
                            // the critic's judge client but bakes its own
                            // REVIEW_PREAMBLE. Gated on the same prompt flag —
                            // a `critic: false` (read-only/exploratory) prompt
                            // suppresses both; those modes leave no diff anyway.
                            // `code_review = off` leaves the judge unarmed
                            // entirely (no diff capture, no review, zero cost);
                            // advisory/blocking arm it and forward the mode.
                            use crate::agent::agent_loop::types::CodeReviewMode;
                            if code_review_mode != CodeReviewMode::Off {
                                agent = agent.with_code_review_fn(build_judge_fn(
                                    client.clone(),
                                    model_name.clone(),
                                    "code-review",
                                    std::sync::Arc::from(
                                        crate::agent::agent_loop::code_review::REVIEW_PREAMBLE,
                                    ),
                                ));
                                agent = agent.with_code_review_mode(code_review_mode);
                            }
                            tracing::info!(
                                target: "dirge::provider",
                                alias = %alias,
                                code_review = code_review_mode.as_str(),
                                "in-loop critic wired; code reviewer armed per mode",
                            );
                        } else {
                            tracing::info!(
                                target: "dirge::provider",
                                alias = %alias,
                                "critic disabled by active prompt; goal gate unaffected",
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(target: "dirge::provider", alias = %alias, error = %e, "failed to build critic client; running without critic");
                        eprintln!(
                            "warning: critic_provider '{alias}' configured but client build failed: {e}"
                        );
                    }
                }
            }
            None => {
                let alias = cfg.critic_provider.clone().unwrap_or_default();
                eprintln!(
                    "error: critic_provider '{alias}' is configured but does not match any entry \
                     in `providers` or any built-in. Either add it under `providers` or remove \
                     the `critic_provider` setting."
                );
            }
        }
    }

    // Phase 4 part 2 — context-depth reminder wiring.
    if let Some(threshold) = cfg.resolve_context_depth_threshold() {
        agent = agent.with_context_depth_reminder(threshold);
    }

    // dirge-ksjl — open-issues finalization gate mode, resolved from
    // config. Default is Off (opt-in; nagging is intrusive).
    agent = agent.with_open_issues_gate_mode(cfg.resolve_open_issues_gate_mode());
    agent = agent.with_session_id(session_id);

    // dirge-9tfq — install the BackgroundStore on the agent so
    // `spawn_runner` can thread it into `LoopSpawnConfig.bg_store`,
    // wiring the subagent-completion follow-up path. Done after
    // the variant-dispatch `build_inner!` macro so every variant
    // gets the store. When `bg_store` is `None` (test paths,
    // `--no-tools`) the agent skips the wiring entirely.
    if let Some(store) = bg_store.as_ref() {
        agent = agent.with_bg_store(store.clone());
    }

    // dirge-z73i — background-review route wiring.
    //
    // When the user has configured `review_provider` AND it
    // resolves to a different (alias, entry) than `ConfigRole::Default`,
    // build a review-specific stream_fn so `spawn_review_runner` runs
    // through the configured cheaper / smarter model.
    //
    // Same equality short-circuit as escalation: if the resolved
    // alias equals the default, skip the duplicate client (the
    // fallback inside `spawn_review_runner_with_cache` produces an
    // identical request).
    if cfg.review_provider.is_some() {
        let default_role = cfg.resolve_role(crate::config::ConfigRole::Default);
        let review_role = cfg.resolve_role(crate::config::ConfigRole::Review);
        match (default_role, review_role) {
            (Some((default_alias, _)), Some((review_alias, review_entry))) => {
                if default_alias.eq_ignore_ascii_case(&review_alias) {
                    tracing::debug!(
                        target: "dirge::provider",
                        alias = %review_alias,
                        "review provider equals default; skipping duplicate client construction",
                    );
                } else {
                    match build_review_stream_fn(
                        &review_alias,
                        &review_entry,
                        &cfg.providers_map(),
                        cfg.auth,
                        chunk_timeout,
                        agent.loop_tools(),
                    ) {
                        Ok((stream_fn, model_name)) => {
                            agent = agent.with_review_route(
                                stream_fn,
                                review_alias.clone(),
                                model_name,
                            );
                            tracing::info!(
                                target: "dirge::provider",
                                alias = %review_alias,
                                "review-provider route wired",
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                target: "dirge::provider",
                                alias = %review_alias,
                                "failed to build review stream_fn: {e}",
                            );
                            eprintln!(
                                "error: failed to build review stream_fn for '{}': {}",
                                review_alias, e
                            );
                        }
                    }
                }
            }
            (_, None) => {
                let alias = cfg.review_provider.as_deref().unwrap_or("(unset)");
                tracing::warn!(
                    target: "dirge::provider",
                    alias = %alias,
                    "review_provider configured but alias does not resolve to a known provider",
                );
                eprintln!(
                    "error: review_provider '{}' is configured but does not match any entry \
                     in `providers` or any built-in. Either add it under `providers` or \
                     remove the `review_provider` setting.",
                    alias
                );
            }
            (None, _) => {
                // Default not resolvable — caller's "no provider"
                // error path handles it.
            }
        }
    }

    // dirge-nqr — per-run assistant-turn cap. CLI `--max-agent-turns`
    // > config `max_agent_turns` > default 100 (matches the existing
    // `cli::resolve_max_agent_turns` precedence). Always set: the
    // loop already had an implicit cap inherited from the legacy rig
    // builder; this wires it through the agent_loop path so `run_print`
    // and the interactive flow both honor it.
    agent = agent.with_max_turns(Some(cli.resolve_max_agent_turns(cfg)));
    // Goal gate stop condition. Off unless `--goal` is set (and a critic
    // provider is configured to judge it); harmless otherwise. Warn on the
    // misconfiguration where a goal is given but no judge resolves — the
    // gate would silently never fire.
    if cli.goal.as_deref().is_some_and(|g| !g.trim().is_empty())
        && cfg
            .resolve_role(crate::config::ConfigRole::Critic)
            .is_none()
    {
        tracing::warn!(
            target: "dirge::goal",
            "--goal is set but no critic_provider is configured to judge it; the goal gate will not fire",
        );
    }
    agent = agent.with_goal(cli.goal.clone());

    agent
}

/// Phase 4 part 1: build a standalone StreamFn for the escalation
/// route. Constructs a fresh `AnyClient` for the alias, builds an
/// `AnyModel` against it using either the entry's `model` field or
/// the provider's default, then wraps with the same tool defs as
/// the main loop.
fn build_escalation_stream_fn(
    alias: &str,
    entry: &ProviderEntry,
    providers: &HashMap<String, ProviderEntry>,
    default_auth: Option<ProviderAuth>,
    chunk_timeout: std::time::Duration,
    loop_tools: &[std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>],
) -> anyhow::Result<crate::agent::agent_loop::StreamFn> {
    use crate::agent::agent_loop::{loop_tool_to_rig_definition, retrying_stream_fn};
    use crate::agent::recovery::RecoveryPolicy;
    let client = create_role_client(alias, providers, default_auth)?;
    let model_name = resolve_entry_model_name(&client, alias, entry);
    let model = client.completion_model(model_name);
    let tool_defs: Vec<rig::completion::ToolDefinition> = loop_tools
        .iter()
        .map(|t| loop_tool_to_rig_definition(t.as_ref()))
        .collect();
    // Wrap with retry (dirge-dppc): the escalation route fires exactly
    // once after repair-exhaustion, so a transient 503/rate-limit on
    // that single call would surface immediately and waste the call the
    // user paid a second provider for. Mirror the default route's
    // `RecoveryPolicy::default()` wrapping.
    Ok(retrying_stream_fn(
        model.build_stream_fn(tool_defs, chunk_timeout, Some(alias.to_string())),
        RecoveryPolicy::default(),
    ))
}

/// F6 tier 3: build a one-shot judge callback (`CriticFn`) over a shared
/// client + model. Bakes `preamble` into the system role and `label` into
/// retry/telemetry. Used for BOTH the in-loop critic (resolved preamble)
/// and the goal gate (its own `GOAL_PREAMBLE`) — the two share one
/// connection while judging under independent preambles. No tools — the
/// judge only reads a transcript and returns a verdict.
fn build_judge_fn(
    client: std::sync::Arc<AnyClient>,
    model_name: String,
    label: &'static str,
    preamble: std::sync::Arc<str>,
) -> crate::agent::agent_loop::critic::CriticFn {
    std::sync::Arc::new(move |prompt: String| {
        let client = client.clone();
        let model_name = model_name.clone();
        let preamble = preamble.clone();
        Box::pin(async move {
            let model = client.completion_model(model_name);
            summarize::oneshot_with_model(model, label, &preamble, prompt).await
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>>
    })
}

/// Resolve the model for non-blocking UI compaction side-LLM calls.
///
/// When `summarization_provider` is configured AND resolves to a DIFFERENT
/// alias than the default role, build a dedicated client/model for it (so a
/// cheaper/faster summarizer can be pointed at compaction); otherwise reuse the
/// active session model via `main_client`. Resolution failure for an explicitly
/// configured provider falls back to the main model only when that fallback is
/// safe; Anthropic OAuth fallback is refused so compaction side calls do not
/// trip the Claude-Code classifier.
pub(crate) fn build_compaction_model(
    cfg: &Config,
    main_client: &AnyClient,
    main_model_name: &str,
) -> anyhow::Result<AnyModel> {
    resolve_summarization_model(
        cfg,
        main_client.completion_model(main_model_name.to_string()),
    )
}

/// dirge-yzmy: single source for the model compaction routes to — shared by
/// the non-blocking UI compaction path ([`build_compaction_model`]) and the
/// in-loop summarizer ([`build_summarize_fn`]), which used to hand-roll this
/// same routing and had already diverged on failure (one `bail!`d, the other
/// silently substituted a disabled fn).
///
/// When `summarization_provider` is configured AND resolves to a DIFFERENT
/// alias than the default role, build a dedicated client for it (so a
/// cheaper/faster summarizer can be pointed at compaction); otherwise reuse
/// `fallback` (the active session model). Anthropic OAuth is refused for either
/// route — the OAuth/Claude-Code classifier is intended for the main CLI turn
/// shape, not side summarizer requests — surfaced as
/// `ANTHROPIC_OAUTH_COMPACTION_DISABLED`, which both callers adapt.
pub(crate) fn resolve_summarization_model(
    cfg: &Config,
    fallback: AnyModel,
) -> anyhow::Result<AnyModel> {
    if cfg.summarization_provider.is_some() {
        let default_role = cfg.resolve_role(crate::config::ConfigRole::Default);
        let summ_role = cfg.resolve_role(crate::config::ConfigRole::Summarization);
        if let (Some((default_alias, _)), Some((alias, entry))) = (default_role, summ_role)
            && !default_alias.eq_ignore_ascii_case(&alias)
        {
            match create_role_client(&alias, &cfg.providers_map(), cfg.auth) {
                Ok(client) => {
                    if matches!(&client, AnyClient::AnthropicOauth(_)) {
                        anyhow::bail!(ANTHROPIC_OAUTH_COMPACTION_DISABLED);
                    }
                    let model_name = resolve_entry_model_name(&client, &alias, &entry);
                    tracing::info!(
                        target: "dirge::provider",
                        alias = %alias,
                        "summarization_provider active for compaction",
                    );
                    return Ok(client.completion_model(model_name));
                }
                Err(e) => {
                    eprintln!(
                        "warning: summarization_provider '{alias}' failed to build ({e}); \
                         falling back to the main model for compaction if safe"
                    );
                }
            }
        }
    }
    if matches!(&fallback, AnyModel::AnthropicOauth(_)) {
        anyhow::bail!(ANTHROPIC_OAUTH_COMPACTION_DISABLED);
    }
    Ok(fallback)
}

fn anthropic_oauth_compaction_disabled_fn() -> crate::agent::compression::SummarizeFn {
    std::sync::Arc::new(|_prompt: String| {
        Box::pin(async { anyhow::bail!(ANTHROPIC_OAUTH_COMPACTION_DISABLED) })
    })
}

/// dirge-008x + dirge-nw25: build the in-loop compaction summarizer.
///
/// Uses the same summarization-provider routing and Anthropic OAuth guard as
/// [`build_compaction_model`], but adapts the resolved model into the
/// `SummarizeFn` callback consumed by the agent loop.
pub(crate) fn build_summarize_fn(
    cfg: &Config,
    main_model: AnyModel,
) -> crate::agent::compression::SummarizeFn {
    let from_model = |model: AnyModel| -> crate::agent::compression::SummarizeFn {
        std::sync::Arc::new(move |prompt: String| {
            let m = model.clone();
            Box::pin(async move { summarize::summarize_with_model(m, prompt).await })
        })
    };

    // dirge-yzmy: same routing as the UI compaction path via the shared
    // resolver; the OAuth-disabled error is adapted into the disabled-fn shape
    // the loop expects (an Err-returning SummarizeFn), instead of a
    // separately-maintained copy of the routing that had drifted.
    match resolve_summarization_model(cfg, main_model) {
        Ok(model) => from_model(model),
        Err(_) => anthropic_oauth_compaction_disabled_fn(),
    }
}

/// dirge-nw25: resolve the model that `task`-spawned subagents default to,
/// from `subagent_provider`. Returns `Some(model)` only when the key is
/// explicitly set AND resolves to a DIFFERENT alias than the default
/// route; otherwise `None` (the caller keeps the main model). A profile
/// route on a specific `task(agent=…)` call still overrides this — it is
/// the fallback default, matching `task.rs`'s `route_model.unwrap_or`.
fn resolve_subagent_model(cfg: &Config) -> Option<AnyModel> {
    cfg.subagent_provider.as_ref()?;
    let (default_alias, _) = cfg.resolve_role(crate::config::ConfigRole::Default)?;
    let (alias, entry) = cfg.resolve_role(crate::config::ConfigRole::Subagent)?;
    if default_alias.eq_ignore_ascii_case(&alias) {
        return None;
    }
    match create_role_client(&alias, &cfg.providers_map(), cfg.auth) {
        Ok(client) => {
            let model_name = resolve_entry_model_name(&client, &alias, &entry);
            tracing::info!(
                target: "dirge::provider",
                alias = %alias,
                "subagent_provider active for task-spawned subagents",
            );
            Some(client.completion_model(model_name))
        }
        Err(e) => {
            eprintln!(
                "warning: subagent_provider '{alias}' failed to build ({e}); \
                 falling back to the main model for subagents"
            );
            None
        }
    }
}

/// dirge-0g6i: build the LLM auto-approval evaluator from a resolved
/// `approval_provider`. Mirrors [`build_judge_fn`] — same client + model
/// resolution and the SAME shared one-shot helper
/// (`summarize::oneshot_with_model`) — but with the approval system
/// preamble and a verdict parser. Returns an `ApprovalFn` the permission
/// chokepoint calls instead of prompting the human.
pub fn build_approval_fn(
    alias: &str,
    entry: &ProviderEntry,
    providers: &HashMap<String, ProviderEntry>,
    default_auth: Option<ProviderAuth>,
) -> anyhow::Result<crate::permission::approval::ApprovalFn> {
    use crate::permission::approval::{
        ApprovalDecision, ApprovalRequest, EVALUATOR_PREAMBLE, build_evaluator_prompt,
        parse_decision,
    };
    let client = std::sync::Arc::new(create_role_client(alias, providers, default_auth)?);
    let model_name = resolve_entry_model_name(&client, alias, entry);
    Ok(std::sync::Arc::new(move |req: ApprovalRequest| {
        let client = client.clone();
        let model_name = model_name.clone();
        Box::pin(async move {
            let model = client.completion_model(model_name);
            let prompt = build_evaluator_prompt(&req);
            let raw = summarize::oneshot_with_model(model, "approval", EVALUATOR_PREAMBLE, prompt)
                .await?;
            Ok::<ApprovalDecision, anyhow::Error>(parse_decision(&raw))
        })
            as std::pin::Pin<
                Box<dyn std::future::Future<Output = anyhow::Result<ApprovalDecision>> + Send>,
            >
    }))
}

/// dirge-z73i: build a stream_fn for the background-review path,
/// routed through `ConfigRole::Review`. Only the memory + skill tools
/// are baked into the request — the review fork's `loop_tools` is
/// filtered to the same set in `spawn_review_runner_with_cache`,
/// so the model sees a tool catalog that matches what the dispatcher
/// will actually accept. Returns `(stream_fn, model_name)` so the
/// caller can stash the model identifier alongside the stream_fn for
/// telemetry (`LoopConfig.model_name`).
fn build_review_stream_fn(
    alias: &str,
    entry: &ProviderEntry,
    providers: &HashMap<String, ProviderEntry>,
    default_auth: Option<ProviderAuth>,
    chunk_timeout: std::time::Duration,
    loop_tools: &[std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>],
) -> anyhow::Result<(crate::agent::agent_loop::StreamFn, String)> {
    use crate::agent::agent_loop::loop_tool_to_rig_definition;
    let client = create_role_client(alias, providers, default_auth)?;
    let model_name = resolve_entry_model_name(&client, alias, entry);
    let model = client.completion_model(model_name.clone());
    // Review path uses ONLY memory + skill — match what
    // `spawn_review_runner_with_cache` puts in `cfg.tools` so
    // the request body and the dispatcher agree.
    let tool_defs: Vec<rig::completion::ToolDefinition> = loop_tools
        .iter()
        .filter(|t| {
            let n = t.name();
            n == "memory" || n == "skill"
        })
        .map(|t| loop_tool_to_rig_definition(t.as_ref()))
        .collect();
    let stream_fn = model.build_stream_fn(tool_defs, chunk_timeout, Some(alias.to_string()));
    Ok((stream_fn, model_name))
}

#[cfg(test)]
mod nw25_tests {
    use super::*;
    use crate::config::{Config, ProviderAuth};
    use clap::Parser;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static CODEX_AUTH_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "dirge_provider_build_{tag}_{}_{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set_path(key: &'static str, value: &Path) -> Self {
            let old = std::env::var(key).ok();
            // SAFETY: CODEX_AUTH_ENV_LOCK serializes all mutations in this module.
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }

        fn remove(key: &'static str) -> Self {
            let old = std::env::var(key).ok();
            // SAFETY: CODEX_AUTH_ENV_LOCK serializes all mutations in this module.
            unsafe { std::env::remove_var(key) };
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: CODEX_AUTH_ENV_LOCK serializes all mutations in this module.
            unsafe {
                match &self.old {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// dirge-nw25: with no `subagent_provider` configured, the resolver
    /// returns `None` (so no extra client is built and the task tool keeps
    /// the main model). Guards the "don't touch unset config" path; the
    /// configured-and-different path mirrors the tested `build_judge_fn`.
    #[test]
    fn resolve_subagent_model_none_when_unset() {
        let cfg = Config::default();
        assert!(cfg.subagent_provider.is_none());
        assert!(
            resolve_subagent_model(&cfg).is_none(),
            "unset subagent_provider must yield no override model"
        );
    }

    #[test]
    fn api_billing_fallback_prefers_resolved_api_key_file_or_stdin_key() {
        let mut cli = Cli::parse_from(["dirge", "--api-key", "argv-key"]);
        cli.resolved_api_key = Some("resolved-key".to_string());

        assert_eq!(openai_api_billing_fallback_key(&cli), Some("resolved-key"));
    }

    #[test]
    fn role_clients_use_top_level_chatgpt_auth() {
        let _lock = CODEX_AUTH_ENV_LOCK.lock().unwrap();
        let dir = TestDir::new("codex_auth");
        std::fs::write(
            dir.path().join("auth.json"),
            r#"{"access_token":"FAKE-CODEX-TOKEN","chatgpt_account_id":"acct-test"}"#,
        )
        .unwrap();
        let _home = EnvGuard::set_path("CODEX_HOME", dir.path());
        let _access = EnvGuard::remove("CODEX_ACCESS_TOKEN");
        let _account = EnvGuard::remove("CHATGPT_ACCOUNT_ID");

        let client =
            create_role_client("openai", &HashMap::new(), Some(ProviderAuth::ChatGpt)).unwrap();

        assert!(matches!(client, AnyClient::ChatGptOpenAI(_)));
    }
}
