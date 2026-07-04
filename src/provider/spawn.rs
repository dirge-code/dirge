//! Runner-spawning and stream-fn construction for [`AnyAgent`]. Split out of
//! `provider/mod.rs` (dirge-4y4l stage 8): the methods that turn a built
//! `AnyAgent` into a live `AgentRunner` (main session, background review,
//! curator forks) plus the `build_stream_fn*` helpers those paths rely on.
//!
//! Child module of `provider`, so it reaches `AnyAgent`'s private fields and
//! the `AnyAgentInner` variants directly (privacy = defining module +
//! descendants) — no `pub(crate)` field bumps or accessors needed.

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use rig::completion::Message;

use super::{AnyAgent, AnyAgentInner};
use crate::agent::runner::AgentRunner;
use crate::agent::tools::ToolCache;

/// Filter a loop-tool registry down to a caller-supplied allow-list, preserving
/// registration order. Single tested place for the hard tool restriction every
/// forked-agent path relies on — the background review/curator forks and the
/// phased-plan phase agents (explore/plan/reviewer). A tool not in `allowed` is
/// literally absent from the fork, so a prompt-level guard slip can't reach it.
pub(crate) fn filter_loop_tools(
    tools: &[std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>],
    allowed: &[&str],
) -> Vec<std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>> {
    tools
        .iter()
        .filter(|t| allowed.contains(&t.name()))
        .cloned()
        .collect()
}

/// dirge-ygm3: replace the `memory`-named tool in a filtered set with the
/// review-enabled instance (`mark`/`supersede`). Used only for the background
/// review fork, so those actions are reachable there and nowhere else. No-op
/// when the set has no `memory` tool.
pub(crate) fn swap_in_review_memory(
    tools: &mut [std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>],
    review_tool: &std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>,
) {
    for slot in tools.iter_mut() {
        if slot.name() == "memory" {
            *slot = review_tool.clone();
        }
    }
}

impl AnyAgent {
    /// Map a tool slice to rig `ToolDefinition`s for the per-turn request
    /// builder. Shared by the main (`spawn_runner`) and fork
    /// (`spawn_filtered_runner_with_cache`) builders, which built this
    /// identically.
    fn tool_defs_for(
        tools: &[std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>],
    ) -> Vec<rig::completion::ToolDefinition> {
        tools
            .iter()
            .map(|t| crate::agent::agent_loop::loop_tool_to_rig_definition(t.as_ref()))
            .collect()
    }

    /// `model_name` as an `Option`, treating empty as "unset" — the
    /// normalization both spawn builders need for `LoopSpawnConfig.model_name`.
    /// Takes the field by ref (not `&self`) so it's callable after `self` is
    /// partially moved (`cfg.tools = self.loop_tools`).
    fn model_name_opt(model_name: &str) -> Option<String> {
        if model_name.is_empty() {
            None
        } else {
            Some(model_name.to_string())
        }
    }

    pub fn spawn_runner(
        self,
        prompt: String,
        history: Vec<Message>,
        steering_queue: Option<
            std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
        >,
    ) -> AgentRunner {
        use crate::agent::agent_loop::{
            LoopSpawnConfig, retrying_stream_fn, retrying_stream_fn_with_non_retryable,
            rig_history_system_prompt, rig_history_to_loop_messages, spawn_loop_runner,
        };
        use crate::agent::recovery::RecoveryPolicy;

        self.cache.clear();

        let provider_name = self.provider_name().to_string();

        // Convert tool registry → rig ToolDefinitions for the
        // request builder, and keep the registry itself for the
        // loop's dispatch.
        let tool_defs = Self::tool_defs_for(&self.loop_tools);

        // Phase-3: per-session loaded-tool set was allocated at
        // `build_agent` time (when `dynamic_tool_search` is on)
        // and the SAME Arc was passed both to the
        // `ToolSearchTool` registered in `self.loop_tools` and
        // stored on `self.tool_def_filter`. The factory reads it
        // per-request; the tool inserts into it on execute.
        // `None` keeps the legacy path.
        let tool_def_filter = self.tool_def_filter.clone();

        // Build the StreamFn (4.5h-2 + 4.5h-3 chunk timeout).
        let inner_stream_fn =
            self.build_stream_fn_with_filter(tool_defs.clone(), tool_def_filter.clone());
        // Wrap with retry (4.5g) so transient Network / RateLimit
        // errors auto-retry with exponential backoff + Retry-After.
        let policy = RecoveryPolicy::default();
        let stream_fn = if let Some(fallback_model) = self.openai_api_key_fallback_model.clone() {
            let primary_stream_fn = retrying_stream_fn_with_non_retryable(
                inner_stream_fn,
                policy.clone(),
                std::sync::Arc::new(
                    crate::provider::billing_fallback::is_openai_subscription_exhausted_error,
                ),
            );
            let fallback_inner = fallback_model.build_stream_fn_with_filter(
                tool_defs,
                self.chunk_timeout,
                Some("openai".to_string()),
                tool_def_filter.clone(),
            );
            let fallback_stream_fn = retrying_stream_fn(fallback_inner, policy);
            crate::provider::billing_fallback::with_openai_api_billing_fallback(
                primary_stream_fn,
                fallback_stream_fn,
                crate::provider::billing_fallback::prompt_from_ask_sender(
                    self.api_billing_ask_tx.clone(),
                ),
            )
        } else {
            retrying_stream_fn(inner_stream_fn, policy)
        };

        // Merge any system-message content from the history
        // (e.g. compaction summary) into the loop's
        // Context.system_prompt. The Agent's preamble (model
        // identity + tool docs) is the base; session-side
        // system messages append.
        let history_preamble = rig_history_system_prompt(&history);
        // `mut` is consumed only by the plugin-gated append below.
        #[cfg_attr(not(feature = "plugin"), allow(unused_mut))]
        let mut system_prompt = if history_preamble.is_empty() {
            self.preamble.clone()
        } else {
            format!("{}\n\n{}", self.preamble, history_preamble)
        };

        // dirge-wqxj: fire the `before-agent-start` plugin hook with
        // the assembled system prompt. A plugin may call
        // `harness/append-system-prompt` to add project/team context
        // to the preamble before the agent starts. Append-only — the
        // model-identity + tool-docs preamble is preserved.
        #[cfg(feature = "plugin")]
        if let Some(pm) = crate::plugin::hook::global() {
            let mut mgr = pm.lock_ignore_poison();
            let ctx = format!(
                "@{{:system-prompt \"{}\"}}",
                crate::plugin::escape_janet_string(&system_prompt)
            );
            match mgr.dispatch("before-agent-start", &ctx) {
                Ok(_) => {
                    if let Some(append) = mgr.take_system_prompt_append() {
                        let append = append.trim();
                        if !append.is_empty() {
                            system_prompt = format!("{system_prompt}\n\n{append}");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "dirge::plugin",
                        error = %e,
                        "before-agent-start hook error — system prompt left unchanged",
                    );
                }
            }
        }

        // Convert rig history → loop messages (Session-side
        // user/assistant/toolResult shapes).
        let loop_history = rig_history_to_loop_messages(history);

        let mut cfg = LoopSpawnConfig::minimal(stream_fn, prompt.clone());
        cfg.system_prompt = system_prompt;
        cfg.history = loop_history;
        cfg.tools = self.loop_tools;
        cfg.provider_name = Some(provider_name);
        cfg.model_name = Self::model_name_opt(&self.model_name);
        cfg.steering_queue = steering_queue;
        cfg.tool_def_filter = tool_def_filter;
        cfg.dynamic_tool_search = self.dynamic_tool_search;
        // Phase 4 part 1: thread the escalation route — when set,
        // the loop's `stream_assistant_response` swaps to this
        // StreamFn for the call immediately following a repair or
        // tree-sitter failure. `escalation_stream_fn=None` keeps
        // the legacy single-provider path byte-for-byte identical.
        cfg.escalation_stream_fn = self.escalation_stream_fn.clone();
        cfg.escalation_provider_name = self.escalation_provider_name.clone();
        // Phase 4 part 2: build a fresh `FileTouchTracker` per
        // session seeded with the current prompt as the active
        // task. `None` keeps the feature off — byte-identical to
        // today.
        cfg.file_touch_tracker = self
            .context_depth_reminder_threshold
            .map(|t| crate::agent::agent_loop::context_depth::FileTouchTracker::new(t, prompt));
        // F6: pre-finalization verifier gate, always on (baked-in). Nudges
        // to verify before finishing when code was edited but not run.
        cfg.verifier = Some(crate::agent::agent_loop::verifier::VerifierGate::new());
        // F6 tier 3: thread the bounded critic (only Some when
        // critic_provider is configured). `None` → no critic.
        cfg.critic_fn = self.critic_fn.clone();
        // dirge-iyf5: diff-aware code reviewer (only Some when
        // critic_provider is configured and the active prompt didn't
        // disable it). `None` → no reviewer.
        cfg.code_review_fn = self.code_review_fn.clone();
        // dirge-iyf5: engagement mode for the armed reviewer above
        // (Blocking = legacy sync re-entry, Advisory = background notice).
        cfg.code_review_mode = self.code_review_mode;
        cfg.goal_fn = self.goal_fn.clone();
        // Goal gate stop condition (`--goal`). Engages only when
        // `goal_fn` above is also present (it's the judge).
        cfg.goal = self.goal.clone();
        // dirge-008x: thread the in-loop compaction summarizer so the
        // proactive folds run LLM summarization (built in `build_agent`).
        cfg.summarize_fn = self.summarize_fn.clone();
        // dirge-nqr: forward the per-run turn cap. `None` keeps the
        // legacy unlimited behavior.
        cfg.max_turns = self.max_turns;
        // dirge-9tfq: forward the BackgroundStore so the spawn pipeline
        // installs a `get_followup_messages` hook that drains pending
        // subagent completions at the outer-loop boundary. `None`
        // (no-tools / test paths) leaves the hook unset and the loop
        // behaves byte-identically to pre-9tfq.
        cfg.bg_store = self.bg_store.clone();
        // dirge-h5tv: thread the memory provider into the loop so
        // auto-compaction can fire on_pre_compress. `None` paths
        // (no provider attached) keep legacy no-op behavior.
        cfg.memory_provider = self.memory_provider.clone();
        #[cfg(feature = "plugin")]
        {
            cfg.plugin_mgr = crate::plugin::hook::global();
        }

        let loop_runner = spawn_loop_runner(cfg);
        loop_runner.into_agent_runner()
    }

    /// Spawn a review runner with only memory + skill tools.
    /// Used by background review (Phase 4) to create a restricted
    /// agent that can only write to project memory and skills.
    ///
    /// dirge-7ls: the review runner gets its OWN `ToolCache` rather
    /// than reusing the main agent's. Even though today's
    /// memory/skill tools don't touch the cache directly, any
    /// future tool added to the review allow-list (or any future
    /// invalidation hook like `cache.clear()` on memory writes)
    /// must not pollute the main agent's cache mid-session.
    /// `subagents/task` is deliberately NOT changed — subagents
    /// share with their parent by design.
    pub fn spawn_review_runner(
        &self,
        prompt: String,
        transcript: String,
    ) -> crate::agent::runner::AgentRunner {
        let (runner, _isolated_cache) =
            self.spawn_review_runner_with_cache(prompt, transcript, ToolCache::new());
        runner
    }

    /// dirge-yai1 — skill-only fork used by the curator's
    /// umbrella-consolidation pass. The curator prompt instructs
    /// the model to only use `skill`, but a tool-level filter is
    /// stronger than a prompt-level guard. Same isolation /
    /// retry / stream-fn selection as `spawn_review_runner`.
    pub fn spawn_curator_runner(
        &self,
        prompt: String,
        transcript: String,
    ) -> crate::agent::runner::AgentRunner {
        let (runner, _isolated_cache) = self.spawn_filtered_runner_with_cache(
            prompt,
            transcript,
            ToolCache::new(),
            &["skill"],
            false,
        );
        runner
    }

    /// P3a (dirge-crrh): fork a phase agent for the phased-plan workflow — a
    /// separate runner with a frozen `transcript`, the given phase `prompt`,
    /// and ONLY the `allowed` tools (a hard whitelist: e.g. read-only for
    /// explore/plan, read+bash for the reviewer). Isolated cache, same
    /// retry/stream-fn machinery as the review fork. The cornerstone of the
    /// explore→plan→review→execute orchestration (P3c) and the reviewer loop
    /// (P3d).
    #[allow(dead_code)] // wired by the phased-plan orchestrator (P3c/P3d)
    pub fn spawn_phase_runner(
        &self,
        prompt: String,
        transcript: String,
        allowed: &[&str],
    ) -> crate::agent::runner::AgentRunner {
        let (runner, _isolated_cache) = self.spawn_filtered_runner_with_cache(
            prompt,
            transcript,
            ToolCache::new(),
            allowed,
            false,
        );
        runner
    }

    /// dirge-mo0w PR-2: memory-only forked runner for the memory
    /// curator's LLM consolidation pass. Inverse of
    /// `spawn_curator_runner` — same forked-runner pattern, but
    /// the tool allow-list is `&["memory"]` so the consolidation
    /// pass can ONLY add/replace/remove memory entries, not write
    /// skills. The model literally cannot reach skill-write tools
    /// even if the prompt-level guard slips.
    pub fn spawn_memory_curator_runner(
        &self,
        prompt: String,
        transcript: String,
    ) -> crate::agent::runner::AgentRunner {
        let (runner, _isolated_cache) = self.spawn_filtered_runner_with_cache(
            prompt,
            transcript,
            ToolCache::new(),
            &["memory"],
            // The consolidation curator only add/replace/remove/promotes — it
            // has no transcript to infer outcomes/contradictions from, so it
            // does NOT get mark/supersede.
            false,
        );
        runner
    }

    /// Internal review-runner constructor with an explicit
    /// caller-supplied cache. Returns the cache alongside the
    /// runner so tests can assert cache isolation via
    /// `ToolCache::shares_storage_with` against `self.cache()`
    /// (dirge-7ls regression test). Callers in production code
    /// should use `spawn_review_runner`, which passes
    /// `ToolCache::new()` here.
    pub(crate) fn spawn_review_runner_with_cache(
        &self,
        prompt: String,
        transcript: String,
        review_cache: ToolCache,
    ) -> (crate::agent::runner::AgentRunner, ToolCache) {
        // dirge-yai1: delegate to the parameterized helper so the
        // curator can reuse the same machinery with a skill-only
        // filter without duplicating the body.
        self.spawn_filtered_runner_with_cache(
            prompt,
            transcript,
            review_cache,
            &["memory", "skill"],
            // The review pass is the one that records outcomes (`mark`) and
            // supersedes contradicted facts — give it the review-enabled tool.
            true,
        )
    }

    /// dirge-yai1: forked-runner factory parameterized by the tool
    /// allow-list. `spawn_review_runner_with_cache` calls in with
    /// `&["memory", "skill"]`; the curator pass calls in with
    /// `&["skill"]` so the model literally cannot write memory
    /// entries even if the prompt-level guard slips. Same cache
    /// isolation, same retry policy, same stream-fn selection as
    /// the original review runner.
    pub(crate) fn spawn_filtered_runner_with_cache(
        &self,
        prompt: String,
        transcript: String,
        review_cache: ToolCache,
        allowed_tools: &[&str],
        // dirge-ygm3: when true, swap in the review-enabled memory tool
        // (`mark`/`supersede`). Only the background REVIEW pass passes true —
        // it's the one that infers outcomes and contradictions from the
        // transcript. The consolidation curator and phase runners pass false.
        review_memory: bool,
    ) -> (crate::agent::runner::AgentRunner, ToolCache) {
        use crate::agent::agent_loop::{LoopSpawnConfig, retrying_stream_fn, spawn_loop_runner};
        use crate::agent::recovery::RecoveryPolicy;

        // Hard guard against accidental sharing: if a caller
        // somehow passes the parent's cache, the regression test
        // would fail — but defense-in-depth, debug_assert that
        // the passed cache is distinct from the parent's.
        debug_assert!(
            !review_cache.shares_storage_with(&self.cache),
            "spawn_filtered_runner_with_cache: review cache must not share storage with the main agent's cache (dirge-7ls)"
        );

        // Filter to the caller-supplied allow-list (shared, tested helper).
        let mut review_tools = filter_loop_tools(&self.loop_tools, allowed_tools);

        // dirge-ygm3: for the review pass, replace the main (non-review) memory
        // tool with the review-enabled one so `mark`/`supersede` are reachable
        // here but nowhere else. No-op if the store didn't load or "memory"
        // wasn't in the allow-list.
        if review_memory && let Some(review_tool) = &self.review_memory_tool {
            swap_in_review_memory(&mut review_tools, review_tool);
        }

        let tool_defs = Self::tool_defs_for(&review_tools);

        // dirge-z73i: prefer the explicit review_stream_fn when the
        // user configured `review_provider` to point at a different
        // alias than `provider`. Falls back to the main agent's
        // stream_fn so unconfigured sessions keep the legacy behavior
        // byte-for-byte.
        let (inner_stream_fn, provider_name_for_review, model_name_for_review) =
            if let Some(rfn) = self.review_stream_fn.clone() {
                (
                    rfn,
                    self.review_provider_name
                        .clone()
                        .unwrap_or_else(|| self.provider_name().to_string()),
                    self.review_model_name.clone(),
                )
            } else {
                (
                    self.build_stream_fn(tool_defs),
                    self.provider_name().to_string(),
                    Self::model_name_opt(&self.model_name),
                )
            };
        let stream_fn = retrying_stream_fn(inner_stream_fn, RecoveryPolicy::default());

        let full_prompt = format!(
            "{}\n\n<session_transcript>\n{}\n</session_transcript>",
            prompt, transcript
        );

        let mut cfg = LoopSpawnConfig::minimal(stream_fn, full_prompt);
        cfg.system_prompt = self.preamble.clone();
        cfg.tools = review_tools;
        cfg.provider_name = Some(provider_name_for_review);
        cfg.model_name = model_name_for_review;

        let loop_runner = spawn_loop_runner(cfg);
        (loop_runner.into_agent_runner(), review_cache)
    }

    /// Phase 4.5h-2: produce a `StreamFn` from this agent's
    /// underlying `CompletionModel`, threading the supplied tool
    /// definitions. Used by the new loop path (`spawn_loop_runner`)
    /// to drive a real LLM through the ported agent_loop.
    ///
    /// Dispatch is a match over `AnyAgentInner`; each variant
    /// extracts its provider-specific `Arc<M>` and threads it
    /// through `rig_stream_fn_from_model::<M>`. The Arc deref +
    /// clone is cheap (refcount bump on the inner Arc, then a
    /// CompletionModel clone — rig's models are themselves
    /// Arc-internal in most provider impls).
    ///
    /// Tool definitions are passed in (not extracted from
    /// `agent.tools`) because the new path uses the LoopTool
    /// registry as the source of truth — phase 4.5h-4 builds
    /// that registry alongside the rig Agent. Callers convert
    /// each `Arc<dyn LoopTool>` to a rig `ToolDefinition` via
    /// `agent_loop::loop_tool_to_rig_definition` before calling
    /// this method.
    pub fn build_stream_fn(
        &self,
        tools: Vec<rig::completion::ToolDefinition>,
    ) -> crate::agent::agent_loop::StreamFn {
        self.build_stream_fn_with_filter(tools, None)
    }

    /// Phase-3 dynamic-tool-search variant. When
    /// `tool_def_filter` is `Some`, the per-request tool list is
    /// filtered to the always-on set + names present in the
    /// shared loaded set (plus `tool_search`). When `None`, the
    /// behavior is byte-for-byte identical to the legacy
    /// `build_stream_fn`.
    pub fn build_stream_fn_with_filter(
        &self,
        tools: Vec<rig::completion::ToolDefinition>,
        tool_def_filter: Option<
            std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
        >,
    ) -> crate::agent::agent_loop::StreamFn {
        let chunk_timeout = self.chunk_timeout;
        let provider = Some(self.provider_name().to_string());
        let model_name = Self::model_name_opt(&self.model_name);
        // dirge-iy20: single provider list in `stream_dispatch`. Each
        // arm clones `tools`/passes `tool_def_filter` by move — only
        // one arm runs, so the moves are exclusive.
        crate::provider::stream_dispatch::dispatch_stream_fn! {
            match &self.inner;
            AnyAgentInner(a) => (*a.model).clone(),
            tools = tools.clone(),
            timeout = Some(chunk_timeout),
            provider = provider,
            model_name = model_name,
            filter = tool_def_filter,
        }
    }
}
