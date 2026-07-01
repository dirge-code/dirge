pub(crate) mod anthropic_http;
pub(crate) mod anthropic_oauth;
pub(crate) mod auth;
mod billing_fallback;
mod build;
pub mod client;
pub(crate) mod codex_http;
mod dispatch;
mod resolve;
mod run;
mod spawn;
mod stream_dispatch;
pub mod summarize;
pub mod wire;

pub use build::*;
pub use dispatch::*;
pub use resolve::*;

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use rig::agent::Agent;
use rig::providers::{anthropic, chatgpt, gemini, ollama, openai, openrouter};

use crate::agent::tools::ToolCache;

#[derive(Clone)]
pub struct AnyAgent {
    inner: AnyAgentInner,
    cache: ToolCache,
    /// Per-chunk read timeout resolved at build_agent time from
    /// config (custom_providers.<n>.stream_chunk_timeout_secs >
    /// providers.<n>.stream_chunk_timeout_secs > top-level
    /// stream_chunk_timeout_secs > 300s default). Carried on the
    /// agent so spawn_runner / run_print don't need to thread it
    /// through every call site.
    chunk_timeout: std::time::Duration,
    /// Phase 4.5h-6: LoopTool registry the new agent_loop path
    /// dispatches against. Built once at `build_agent` time via
    /// `agent::builder::build_loop_tools`. `Vec<Arc<...>>` is
    /// clone-cheap (Arc bump).
    loop_tools: Vec<std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>>,
    /// Phase 4.5h-6: system prompt for the new loop path.
    /// Extracted from the rig Agent's preamble field at build
    /// time (every variant exposes `Agent.preamble: Option<String>`).
    preamble: String,
    /// Model identifier — the same string the user passed via
    /// `--model` or pulled from config. Carried so `spawn_runner`
    /// can forward it into `LoopSpawnConfig::model_name` for the
    /// `tool_input_repair` telemetry's `(model, tool, repair_kind)`
    /// triple. `String::new()` is acceptable — telemetry falls back
    /// to `"unknown"` when the field is empty.
    model_name: String,
    /// Phase-3: dynamic-tool-search opt-in. Resolved from
    /// `config.dynamic_tool_search` at `build_agent` time.
    /// When `true`, `spawn_runner` wires the shared
    /// `tool_def_filter` Arc into both the stream factory (for
    /// per-turn filtering) and (already) into the
    /// `ToolSearchTool` instance in `loop_tools`. Default
    /// `false` — the untouched-by-this-feature path.
    dynamic_tool_search: bool,
    /// Phase-3: per-session loaded-tool set. Allocated by
    /// `build_agent` when `dynamic_tool_search` is on, and
    /// shared with the `ToolSearchTool` instance registered in
    /// `loop_tools`. `spawn_runner` forwards this Arc to the
    /// stream factory so the filter sees the same set the tool
    /// mutates. `None` when the feature is off.
    tool_def_filter: Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
    /// dirge-tpx6: the live `tool_search` registry — the SAME Arc held by
    /// the `ToolSearchTool` in `loop_tools`. `extend_loop_tools` appends
    /// background-injected MCP tools' meta here so they stay search-gated
    /// (discoverable via `tool_search`, hidden until requested) rather
    /// than always-visible. `None` when dynamic_tool_search is off. Only
    /// read on the MCP-injection path.
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    tool_search_registry:
        Option<std::sync::Arc<std::sync::Mutex<Vec<crate::agent::tools::tool_search::ToolMeta>>>>,
    /// Phase 4 part 1: alternate stream function for dual-client
    /// escalation. Constructed at `build_agent` time when
    /// `ConfigRole::Escalation` resolves to a DIFFERENT provider
    /// than `ConfigRole::Default`. `None` keeps the legacy single-
    /// provider behaviour byte-for-byte identical.
    escalation_stream_fn: Option<crate::agent::agent_loop::StreamFn>,
    /// Phase 4 part 1: provider alias for the escalation route.
    /// Forwarded to `LoopConfig.escalation_provider_name` so the
    /// UI's `EscalationActivated` line can show the user which
    /// provider is taking over. `None` when escalation is off.
    escalation_provider_name: Option<String>,
    /// F6 tier 3: bounded LLM critic callback, built at `build_agent`
    /// time when `ConfigRole::Critic` resolves (i.e. `critic_provider`
    /// is configured). Forwarded to `LoopConfig.critic_fn`. `None` = off.
    critic_fn: Option<crate::agent::agent_loop::critic::CriticFn>,
    /// Diff-aware code reviewer judge (dirge-iyf5). Built at `build_agent`
    /// time from the same critic provider as `critic_fn` but baking
    /// `code_review::REVIEW_PREAMBLE`; forwarded to
    /// `LoopConfig.code_review_fn`. `None` = off.
    code_review_fn: Option<crate::agent::agent_loop::critic::CriticFn>,
    /// Goal gate's judge callback. Built at `build_agent` time from the
    /// same critic provider as `critic_fn` but baking its own
    /// `GOAL_PREAMBLE`; forwarded to `LoopConfig.goal_fn`. `None` = off.
    goal_fn: Option<crate::agent::agent_loop::critic::CriticFn>,
    /// Goal gate: optional natural-language stop condition for autonomous
    /// runs (`--goal`). Forwarded to `LoopConfig.goal`; active only when a
    /// `goal_fn` (the judge) is also present. `None` = off (default).
    goal: Option<String>,
    /// dirge-008x: in-loop LLM compaction summarizer. Built at
    /// `build_agent` time from the main model and forwarded to
    /// `LoopSpawnConfig.summarize_fn`, so the proactive folds in
    /// `run_agent_loop` actually call a model instead of degrading to a
    /// prune-only pass. `None` only in test agents built without it. (A
    /// dedicated `summarization_provider` route is dirge-nw25.)
    summarize_fn: Option<crate::agent::compression::SummarizeFn>,
    /// Phase 4 part 2: optional context-depth reminder threshold.
    /// Forwarded to `spawn_runner`, which constructs a fresh
    /// `FileTouchTracker` for each session because the tracker is
    /// per-prompt (`active_task` is the initial prompt).
    context_depth_reminder_threshold: Option<usize>,
    /// dirge-nqr: hard cap on assistant turns per run. Set via
    /// `with_max_turns`. Forwarded to `LoopSpawnConfig.max_turns`
    /// at spawn time. `None` = unlimited (legacy).
    max_turns: Option<usize>,
    /// dirge-z73i: alternate stream_fn for the background-review
    /// path. Built at `build_agent` time when `ConfigRole::Review`
    /// resolves to a different provider than `ConfigRole::Default`.
    /// `None` falls back to the main agent's stream_fn (legacy
    /// behavior; matches the original `spawn_review_runner`).
    review_stream_fn: Option<crate::agent::agent_loop::StreamFn>,
    /// dirge-z73i: provider alias for the review route, surfaced in
    /// the review runner's `LoopConfig.provider_name` so telemetry
    /// records the right backend.
    review_provider_name: Option<String>,
    /// dirge-z73i: model identifier for the review route, surfaced
    /// in the review runner's `LoopConfig.model_name`.
    review_model_name: Option<String>,
    /// dirge-9tfq: per-session background-task store, forwarded into
    /// `LoopSpawnConfig.bg_store` at spawn time so the loop's
    /// `get_followup_messages` hook surfaces subagent completions
    /// without needing the user to re-prompt. `None` when no store
    /// was supplied (tests, `--no-tools`); the followup path stays
    /// disabled in that case (legacy behaviour byte-identical).
    bg_store: Option<crate::agent::tools::background::BackgroundStore>,
    /// dirge-7tvq: memory provider held alongside the agent so
    /// session-lifecycle hooks (`on_session_end`, `on_pre_compress`)
    /// can dispatch through the trait. `None` when no provider was
    /// built (test agents, --no-tools, build failure). The provider
    /// is shared with `MemoryTool` via `Arc` — same instance.
    memory_provider: Option<std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>>,
    /// Optional OpenAI API-key model used only after native OpenAI/Codex OAuth
    /// reports subscription quota/model-access exhaustion and the user confirms
    /// switching this request to API-key billing.
    openai_api_key_fallback_model: Option<AnyModel>,
    api_billing_ask_tx: Option<crate::permission::ask::AskSender>,
    /// dirge-ygm3: a memory tool with the background-review actions
    /// (`mark`/`supersede`) enabled, kept OUT of `loop_tools` so the
    /// interactive agent never sees them. The review runner swaps this in
    /// place of the main memory tool. `None` when no store loaded.
    review_memory_tool: Option<std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>>,
}

#[derive(Clone)]
pub(crate) enum AnyAgentInner {
    OpenRouter(Agent<openrouter::completion::CompletionModel>),
    OpenAI(Agent<openai::completion::CompletionModel>),
    ChatGptOpenAI(
        Agent<openai::responses_api::ResponsesCompletionModel<codex_http::CodexHttpClient>>,
    ),
    OpenAICodex(Agent<chatgpt::ResponsesCompletionModel>),
    Anthropic(Agent<anthropic::completion::CompletionModel>),
    AnthropicOauth(
        Agent<anthropic::completion::CompletionModel<anthropic_http::AnthropicHttpClient>>,
    ),
    Gemini(Agent<gemini::completion::CompletionModel>),
    DeepSeek(Agent<openai::completion::CompletionModel>),
    Glm(Agent<openai::completion::CompletionModel>),
    Ollama(Agent<ollama::CompletionModel>),
    Custom(Agent<openai::completion::CompletionModel>),
}

impl AnyAgent {
    pub fn new(
        inner: AnyAgentInner,
        cache: ToolCache,
        chunk_timeout: std::time::Duration,
        loop_tools: Vec<std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>>,
        preamble: String,
        model_name: String,
    ) -> Self {
        AnyAgent {
            inner,
            cache,
            chunk_timeout,
            loop_tools,
            preamble,
            model_name,
            dynamic_tool_search: false,
            tool_def_filter: None,
            tool_search_registry: None,
            escalation_stream_fn: None,
            escalation_provider_name: None,
            critic_fn: None,
            code_review_fn: None,
            goal_fn: None,
            goal: None,
            summarize_fn: None,
            context_depth_reminder_threshold: None,
            max_turns: None,
            review_stream_fn: None,
            review_provider_name: None,
            review_model_name: None,
            bg_store: None,
            memory_provider: None,
            openai_api_key_fallback_model: None,
            api_billing_ask_tx: None,
            review_memory_tool: None,
        }
    }

    /// dirge-ygm3: attach the review-enabled memory tool (see the field doc).
    pub fn with_review_memory_tool(
        mut self,
        tool: std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>,
    ) -> Self {
        self.review_memory_tool = Some(tool);
        self
    }

    /// dirge-x949: append tools to the live loop registry. Background
    /// MCP loading uses this to inject server tools after the agent was
    /// built (and the UI drawn) without them — the next
    /// `clone().spawn_runner` forwards the grown registry to the loop
    /// dispatch and the request's tool-definition list. Cheap: each
    /// entry is an `Arc` bump.
    ///
    /// dirge-ffwa/tpx6: when `dynamic_tool_search` is on, the request only
    /// ships tool defs whose names are in the shared loaded-set, and the
    /// model discovers the rest via `tool_search` over a registry snapshot
    /// taken at BUILD time — before MCP connected. A late-injected tool is
    /// in neither place, so it would be both undiscoverable and filtered
    /// out of every request (uncallable). Fix: append its meta to the live
    /// `tool_search` registry so the model can DISCOVER it via
    /// `tool_search` (and `tool_search` then marks it loaded on demand) —
    /// keeping it search-gated, exactly like a build-time MCP tool, rather
    /// than force-loading it into every request. No-op when
    /// dynamic_tool_search is off (registry is `None`).
    #[cfg(feature = "mcp")]
    pub fn extend_loop_tools(
        &mut self,
        more: Vec<std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>>,
    ) {
        if let Some(registry) = &self.tool_search_registry {
            let mut reg = registry.lock_ignore_poison();
            for t in &more {
                reg.push(crate::agent::tools::tool_search::meta_from_loop_tool(
                    t.as_ref(),
                ));
            }
        }
        self.loop_tools.extend(more);
    }

    /// dirge-7tvq: install the `MemoryProvider` used for this session
    /// so lifecycle hooks (`on_session_end`, `on_pre_compress`) can
    /// dispatch through the trait. Called by `build_agent` once the
    /// provider has been constructed. Idempotent — repeated calls
    /// replace the held Arc.
    pub fn with_memory_provider(
        mut self,
        provider: std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>,
    ) -> Self {
        self.memory_provider = Some(provider);
        self
    }

    /// dirge-7tvq: accessor for the held memory provider. Used by
    /// lifecycle call sites (session swap, compaction) to fire the
    /// trait hooks. Returns `None` for test agents and `--no-tools`
    /// runs where no provider was constructed.
    pub fn memory_provider(
        &self,
    ) -> Option<&std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>> {
        self.memory_provider.as_ref()
    }

    /// The diff-aware code reviewer judge (dirge-iyf5), if a
    /// `critic_provider` was configured. Used by the `/code-review` slash
    /// command to run an on-demand review; `None` = reviewer not wired.
    pub fn code_review_fn(&self) -> Option<&crate::agent::agent_loop::critic::CriticFn> {
        self.code_review_fn.as_ref()
    }

    /// dirge-9tfq: install the per-session background-task store so
    /// `spawn_runner` can wire the subagent-completion follow-up
    /// hook into the agent loop. Called by `build_agent` whenever a
    /// `BackgroundStore` was provided (production interactive paths;
    /// not test / `--no-tools`). Idempotent — repeated calls replace
    /// the stored handle but keep the Arc-internal state in the
    /// shared store unchanged.
    pub fn with_bg_store(
        mut self,
        store: crate::agent::tools::background::BackgroundStore,
    ) -> Self {
        self.bg_store = Some(store);
        self
    }

    pub(crate) fn with_openai_api_key_billing_fallback(
        mut self,
        model: AnyModel,
        ask_tx: Option<crate::permission::ask::AskSender>,
    ) -> Self {
        self.openai_api_key_fallback_model = Some(model);
        self.api_billing_ask_tx = ask_tx;
        self
    }

    /// dirge-z73i: install a dedicated stream_fn for the
    /// background-review path. Called from `build_agent` only when
    /// `ConfigRole::Review` resolves to a different alias than
    /// `ConfigRole::Default`. `spawn_review_runner` picks this up
    /// and routes review work through the alternate provider/model.
    pub fn with_review_route(
        mut self,
        stream_fn: crate::agent::agent_loop::StreamFn,
        provider_name: String,
        model_name: String,
    ) -> Self {
        self.review_stream_fn = Some(stream_fn);
        self.review_provider_name = Some(provider_name);
        self.review_model_name = Some(model_name);
        self
    }

    /// dirge-nqr: install the per-run assistant-turn cap. `None`
    /// clears any previous cap (unlimited). Forwarded to
    /// `LoopSpawnConfig.max_turns` at spawn time.
    pub fn with_max_turns(mut self, max_turns: Option<usize>) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Phase 4 part 1: wire the dual-client escalation route.
    /// Called by `build_agent` only when `ConfigRole::Escalation`
    /// resolves to a different provider than `ConfigRole::Default`.
    /// Pass both the StreamFn and the provider alias so
    /// `spawn_runner` can plumb them through to `LoopSpawnConfig`.
    pub fn with_escalation(
        mut self,
        stream_fn: crate::agent::agent_loop::StreamFn,
        provider_name: String,
    ) -> Self {
        self.escalation_stream_fn = Some(stream_fn);
        self.escalation_provider_name = Some(provider_name);
        self
    }

    /// F6 tier 3: attach the bounded LLM critic. Called by `build_agent`
    /// only when `ConfigRole::Critic` resolves (`critic_provider` set).
    pub fn with_critic(mut self, critic_fn: crate::agent::agent_loop::critic::CriticFn) -> Self {
        self.critic_fn = Some(critic_fn);
        self
    }

    /// Attach the diff-aware code reviewer judge (dirge-iyf5). Built from
    /// the same critic provider as the critic but baking
    /// `code_review::REVIEW_PREAMBLE`.
    pub fn with_code_review_fn(
        mut self,
        code_review_fn: crate::agent::agent_loop::critic::CriticFn,
    ) -> Self {
        self.code_review_fn = Some(code_review_fn);
        self
    }

    /// F6 tier 3: attach the goal gate's judge. Built from the same critic
    /// provider as the critic but baking its own `GOAL_PREAMBLE`, so it's
    /// independent of any critic preamble override or `critic: false` prompt.
    pub fn with_goal_fn(mut self, goal_fn: crate::agent::agent_loop::critic::CriticFn) -> Self {
        self.goal_fn = Some(goal_fn);
        self
    }

    /// Set the goal gate's stop condition. An empty/blank string clears it
    /// (treated as no goal). The gate only engages when a critic provider
    /// is also configured to serve as the judge.
    pub fn with_goal(mut self, goal: Option<String>) -> Self {
        self.goal = goal.filter(|g| !g.trim().is_empty());
        self
    }

    /// dirge-008x: attach the in-loop compaction summarizer. Called by
    /// `build_agent` so the proactive folds can run LLM summarization
    /// instead of degrading to a prune-only pass.
    pub fn with_summarizer(mut self, summarize_fn: crate::agent::compression::SummarizeFn) -> Self {
        self.summarize_fn = Some(summarize_fn);
        self
    }

    /// Phase 4 part 2: enable the context-depth reminder system
    /// with the given consecutive-turn threshold. Called by
    /// `build_agent` only when `config.context_depth_reminder_threshold`
    /// is `Some`. Carrying the threshold (rather than a tracker
    /// instance) lets `spawn_runner` build a fresh tracker per
    /// session seeded with the initial prompt.
    pub fn with_context_depth_reminder(mut self, threshold: usize) -> Self {
        self.context_depth_reminder_threshold = Some(threshold);
        self
    }

    /// Phase-3: enable the dynamic-tool-search path for sessions
    /// spawned from this agent. `filter` is the shared Arc
    /// already wired into the `ToolSearchTool` registered in
    /// `loop_tools` (so the tool's mutations and the request
    /// filter see the SAME set). Caller (build_agent) reads
    /// `config.dynamic_tool_search`; when off, this method
    /// isn't called and the legacy path runs untouched.
    pub fn with_dynamic_tool_search(
        mut self,
        filter: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
        registry: std::sync::Arc<std::sync::Mutex<Vec<crate::agent::tools::tool_search::ToolMeta>>>,
    ) -> Self {
        self.dynamic_tool_search = true;
        self.tool_def_filter = Some(filter);
        self.tool_search_registry = Some(registry);
        self
    }

    /// Phase 4.5h-6 cutover: route through the new agent_loop
    /// path. Composes 4.5a (rig stream), 4.5b (rig tool adapter,
    /// done at build time via build_loop_tools), 4.5c (event
    /// bridge), 4.5d (plugin hooks from the global manager),
    /// 4.5g (retry wrapper around the stream), and emits
    /// `AgentEvent`s on the existing `AgentRunner` shape so UI /
    /// ACP callsites work unchanged.
    ///
    /// Returns immediately with `AgentRunner`; the loop runs on
    /// a spawned tokio task.
    /// Return the provider name as a static string (matches the
    /// CLI / config naming: "openai", "anthropic", ..., "glm",
    /// "ollama", "openrouter", "custom"). Used to populate
    /// `LoopConfig.provider_name` so the `getApiKey` hook
    /// receives the canonical name (code review #2).
    pub fn provider_name(&self) -> &'static str {
        match &self.inner {
            AnyAgentInner::OpenRouter(_) => "openrouter",
            AnyAgentInner::OpenAI(_) => "openai",
            AnyAgentInner::ChatGptOpenAI(_) => "openai",
            AnyAgentInner::OpenAICodex(_) => "openai",
            AnyAgentInner::Anthropic(_) => "anthropic",
            AnyAgentInner::AnthropicOauth(_) => "anthropic",
            AnyAgentInner::Gemini(_) => "gemini",
            AnyAgentInner::DeepSeek(_) => "deepseek",
            AnyAgentInner::Glm(_) => "glm",
            AnyAgentInner::Ollama(_) => "ollama",
            AnyAgentInner::Custom(_) => "custom",
        }
    }

    /// Internal accessor for the agent's tool result cache.
    /// Exposed `pub(crate)` so tests in `provider::mod_tests`
    /// can assert cache-isolation invariants (e.g. dirge-7ls:
    /// the background-review runner must NOT share this Arc).
    #[allow(dead_code)]
    pub(crate) fn cache(&self) -> &ToolCache {
        &self.cache
    }

    /// The LoopTool registry built at `build_agent` time. Read by the
    /// escalation/review stream-fn builders in `provider::build` (a
    /// sibling module) to mirror the default loop's tool set.
    pub(crate) fn loop_tools(&self) -> &[std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>] {
        &self.loop_tools
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
