//! Value types for the agent loop. Faithful port of `pi/packages/agent/src/types.ts`.
//!
//! Phase 0: enums + plain shape structs. No behavior yet — phase 1+
//! consume these.

use serde::{Deserialize, Serialize};

/// How a batch of tool calls from one assistant message is executed.
///
/// Port of pi `ToolExecutionMode` (types.ts:36):
///   `"sequential" | "parallel"`
///
/// - `Sequential`: each tool call is prepared, executed, and finalized
///   before the next one starts.
/// - `Parallel`: tool calls are prepared sequentially, then allowed
///   tools execute concurrently. `tool_execution_end` events emit in
///   completion order; tool-result message artifacts emit later in
///   assistant source order.
///
/// Wire format is lowercase to match pi's TypeScript literal union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    Sequential,
    /// Default per pi: `toolExecution?: ToolExecutionMode` defaults to
    /// `"parallel"` when omitted (types.ts:252 comment).
    #[default]
    Parallel,
}

/// How many queued user messages are injected at a queue drain point.
///
/// Port of pi `QueueMode` (types.ts:44):
///   `"all" | "one-at-a-time"`
///
/// - `All`: drain and inject every queued message at the drain point.
/// - `OneAtATime`: drain only the oldest queued message; the rest
///   stay queued for later drain points.
///
/// Wire format is kebab-case ("one-at-a-time") to match pi exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    #[default]
    All,
    OneAtATime,
}

/// Reasoning effort / thinking budget for models that support it.
///
/// Port of pi `ThinkingLevel` (types.ts:284):
///   `"off" | "minimal" | "low" | "medium" | "high" | "xhigh"`
///
/// Note from pi: `"xhigh"` is only supported by selected model
/// families. Pi recommends checking model thinking-level metadata
/// from `@earendil-works/pi-ai` to detect support for a concrete
/// model. Dirge will mirror this once provider plumbing lands in
/// phase 1.
///
/// Wire format is lowercase to match pi's literals exactly,
/// including `"xhigh"` (one word, no separator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    /// Reasoning disabled. Pi's `prepareNextTurn` snapshot maps
    /// `"off"` to `reasoning: undefined` on the next request
    /// (agent-loop.ts:235-237).
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

/// Per-level token budgets for thinking/reasoning. Token-based
/// providers (Anthropic budget-mode, etc.) consume this to size
/// the reasoning allocation per turn. Effort-based providers
/// (OpenAI Responses, Anthropic adaptive models like Opus 4.6+)
/// ignore it in favor of the `ThinkingLevel` mapping.
///
/// Port of pi `ThinkingBudgets` (types.ts:67-72). Missing
/// fields default to provider-specific sensible values.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ThinkingBudgets {
    pub minimal: Option<u32>,
    pub low: Option<u32>,
    pub medium: Option<u32>,
    pub high: Option<u32>,
}

/// Conversation context passed to the loop and threaded through
/// hooks. Port of pi `AgentContext` (types.ts:387).
///
/// `messages` is `Vec<serde_json::Value>` as a phase-0 placeholder;
/// phase 4 will substitute a typed `LoopMessage` enum once the
/// message vocabulary is finalized. We avoid choosing the final
/// shape here because rig's message types and dirge's existing
/// `session::Message` need to be reconciled — that's phase 1 work,
/// not phase 0.
///
/// `tools` is held as `Arc<dyn LoopTool>` so the same tool registry
/// can be shared across turns without cloning. Pi uses
/// `tools?: AgentTool<any>[]` — optional, defaulting to an empty
/// list when no tools are configured.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// System prompt sent with each model request. Pi field
    /// `systemPrompt: string`.
    pub system_prompt: String,
    /// Transcript visible to the model. Pi field `messages:
    /// AgentMessage[]`. Phase 0 placeholder type — see module doc.
    pub messages: Vec<serde_json::Value>,
    /// Tools available for this run. Pi field `tools?:
    /// AgentTool<any>[]`. Empty by default rather than `Option<Vec>`
    /// because empty-vs-absent has no semantic difference for pi's
    /// loop (both produce the same lookup misses).
    pub tools: Vec<std::sync::Arc<dyn super::tool::LoopTool>>,
}

/// Replacement runtime state returned by `prepareNextTurn`.
///
/// Port of pi `AgentLoopTurnUpdate` (types.ts:124):
///   `{ context?, model?, thinkingLevel? }`
///
/// All fields optional; omitted fields keep the current value
/// (loop.rs phase 4 will mirror pi's `?? config.X` fallback).
///
/// `model` is `Option<String>` here as the phase-0 placeholder.
/// Phase 4 will substitute the rig `CompletionModel` trait object
/// or an opaque model handle once the model-swap mechanism lands.
/// We don't pick the type now because the rig API for runtime
/// model swap may require its own wrapper type.
#[derive(Debug, Clone, Default)]
pub struct TurnUpdate {
    pub context: Option<Context>,
    pub model: Option<String>,
    pub thinking_level: Option<ThinkingLevel>,
}

/// Loop configuration. Port of pi `AgentLoopConfig` (types.ts:135).
///
/// Phase 1 lands the subset of hooks `stream_assistant_response`
/// consumes: `convert_to_llm` (required), `transform_context`
/// (optional), `get_api_key` (optional), `api_key` (fallback).
///
/// Subsequent phases extend this struct with `prepare_next_turn`,
/// `should_stop_after_turn`, `get_steering_messages`,
/// `get_followup_messages`, `before_tool_call`, `after_tool_call`.
/// The struct is intentionally non-exhaustive at this stage —
/// builders / constructors will land alongside the hooks that
/// need them.
///
/// The hook closures are stored as `Arc<dyn Fn …>` so the struct
/// stays `Clone` (loops re-clone the config across retry
/// boundaries) and so the same hook can be installed in multiple
/// places without ownership games. Async hooks return
/// `Pin<Box<dyn Future>>` for the same dyn-compatibility reason
/// `LoopTool` does (no `async_trait` dep).
pub struct LoopConfig {
    /// Required. Port of pi `convertToLlm` (types.ts:164).
    /// Maps the agent-level transcript to the LLM-compatible
    /// shape. Phase 1's placeholder type uses `Vec<Value>` →
    /// `Vec<Value>`; phase 4 will substitute typed messages.
    ///
    /// Pi contract: "must not throw or reject. Return a safe
    /// fallback value instead." We mirror this by NOT making the
    /// hook fallible; callers convert their errors to a sentinel
    /// value (e.g. empty Vec) themselves.
    pub convert_to_llm: ConvertToLlmFn,

    /// Optional. Port of pi `transformContext?` (types.ts:186).
    /// Runs BEFORE `convertToLlm` to give the caller a chance
    /// to prune / rewrite at the AgentMessage level (context
    /// window management). Same no-throw contract as
    /// `convertToLlm`.
    pub transform_context: Option<TransformContextFn>,

    /// dirge-jia8: optional plugin compaction hooks fired around the
    /// auto-fold / `/compress` compaction pass. `on_before` is an
    /// observe-only notification (cannot cancel — cancelling an
    /// emergency fold would overflow the context); `on_compact` lets
    /// a plugin supply a custom summary instead of the LLM
    /// summarizer. `None` (default) = no plugin compaction
    /// involvement.
    pub compaction_hooks: Option<CompactionHooks>,

    /// Optional. Port of pi `getApiKey?` (types.ts:196).
    /// Resolves an API key dynamically per request — useful for
    /// short-lived OAuth tokens. `None` means "use `api_key`
    /// fallback".
    ///
    /// Argument: provider name (pi: `config.model.provider`).
    /// We pass the model identifier string for now;
    /// phase 4 may substitute a richer model handle.
    pub get_api_key: Option<GetApiKeyFn>,

    /// Static API key fallback. Used when `get_api_key` is None
    /// OR when `get_api_key` returns None. Pi field
    /// `config.apiKey` (inherited from `SimpleStreamOptions`).
    pub api_key: Option<String>,

    /// Tool execution mode. Pi field `toolExecution?:
    /// ToolExecutionMode` (types.ts:254). Default `Parallel`
    /// per pi's docs. Per-tool `execution_mode` can FORCE
    /// sequential per pi at agent-loop.ts:381-383.
    pub tool_execution: ToolExecutionMode,

    /// Phase 2 hook — fires before tool dispatch. May mutate
    /// args or block the call. Port of pi `beforeToolCall?`
    /// (types.ts:262).
    pub before_tool_call: Option<super::hooks::BeforeToolCallFn>,

    /// Phase 2 hook — fires after tool execution. May override
    /// content / details / isError / terminate. Port of pi
    /// `afterToolCall?` (types.ts:276).
    pub after_tool_call: Option<super::hooks::AfterToolCallFn>,

    /// Phase 4 hook — fires between turns. May swap model /
    /// thinking / context for the next turn. Port of pi
    /// `prepareNextTurn?` (types.ts:215).
    pub prepare_next_turn: Option<super::hooks::PrepareNextTurnFn>,

    /// Phase 4 hook — fires between turns. Return true to stop
    /// the loop after the current turn finishes. Port of pi
    /// `shouldStopAfterTurn?` (types.ts:208).
    pub should_stop_after_turn: Option<super::hooks::ShouldStopAfterTurnFn>,

    /// Phase 4 hook — polled for messages to inject mid-run. Port
    /// of pi `getSteeringMessages?` (types.ts:230).
    pub get_steering_messages: Option<super::hooks::GetSteeringMessagesFn>,

    /// Phase 4 hook — polled at outer-loop boundary for
    /// continuation messages. Port of pi `getFollowUpMessages?`
    /// (types.ts:243).
    pub get_followup_messages: Option<super::hooks::GetFollowupMessagesFn>,

    // ============================================================
    // Phase 4.6 — provider stream options (pi parity)
    // ============================================================
    /// Reasoning / thinking level. Threaded to the stream factory
    /// per-call; provider-specific mapping (Anthropic effort or
    /// budget tokens; OpenAI Responses `reasoning.effort`) lives
    /// in `provider::AnyAgent::build_stream_fn`. Other providers
    /// ignore. Port of pi `SimpleStreamOptions.reasoning?`
    /// (types.ts:193).
    pub reasoning: Option<ThinkingLevel>,
    /// Per-level token budgets. Honored by token-based providers
    /// (Anthropic budget mode). Effort-based providers ignore.
    /// Port of pi `SimpleStreamOptions.thinkingBudgets?`
    /// (types.ts:195).
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Custom HTTP headers merged with provider defaults. Pi
    /// `StreamOptions.headers?` (types.ts:120). Some rig
    /// providers honor at request build time; others at client
    /// config time only.
    pub headers: std::collections::HashMap<String, String>,
    /// Provider-specific metadata (e.g. Anthropic `user_id` for
    /// abuse / rate-limit tracking). Pi `StreamOptions.metadata?`
    /// (types.ts:142).
    pub metadata: std::collections::HashMap<String, serde_json::Value>,
    /// Request-level timeout (full HTTP request). Separate from
    /// dirge's per-chunk timeout (`chunk_timeout` on `AnyAgent`)
    /// which guards individual stream chunks. Pi
    /// `StreamOptions.timeoutMs?` (types.ts:125). Rig clients
    /// expose this at client-construction time today; per-request
    /// override is not yet wired — field present so future
    /// commits can honor it without another LoopConfig change.
    pub request_timeout: Option<std::time::Duration>,

    /// Provider name passed to the `getApiKey` hook so a single
    /// hook implementation can resolve keys for multiple
    /// providers (matches pi `getApiKey(provider)` contract).
    /// Set at run construction (`spawn_runner` from
    /// `AnyAgentInner` variant name). Code review #2 — earlier
    /// code passed `""` here, breaking any provider-aware hook.
    pub provider_name: Option<String>,

    /// Model identifier carried through the loop so cross-cutting
    /// telemetry (notably the `tool_input_repair` log) can record
    /// `(model, tool, repair_kind)` and surface per-(model, tool)
    /// regression rates. Set at run construction by the same caller
    /// that fills `provider_name`. `None` is acceptable —
    /// telemetry falls back to an `unknown` placeholder.
    pub model_name: Option<String>,

    /// Port of Reasonix flash-first: hard-code a cheap model for
    /// mechanical auxiliary calls (fold summaries, healing
    /// truncation). When `Some`, summarisation and related tasks
    /// use this model instead of the session model. Reasonix uses
    /// `deepseek-v4-flash` for all auxiliary work.
    ///
    /// **Status**: deferred. Wiring requires a second `StreamFn`
    /// constructed from a separate model + provider, which needs
    /// `LoopSpawnConfig` / `provider.rs` plumbing. Until then this
    /// field is accepted but not acted on.
    pub compact_model: Option<String>,

    /// Additional tool names to treat as mutating (clears read-only
    /// entries from the storm breaker window). Built-in defaults
    /// (`write`, `edit`, `bash`, `apply_patch`) are always included.
    pub storm_mutating_tools: Option<Vec<String>>,

    /// Additional tool names to treat as storm-exempt (never
    /// suppressed regardless of repetition). Built-in defaults
    /// (`read`, `list_dir`, `grep`, etc.) are always included.
    pub storm_exempt_tools: Option<Vec<String>>,

    /// Phase-1 telemetry (docs/AGENTIC_LOOP_PLAN.md): per-run
    /// aggregate counters for the input-repair layer. Increment
    /// happens inside `prepare_tool_call` after a successful repair
    /// (or `record_invalid` when the repair pass exhausts); the
    /// snapshot lands in `LoopEvent::RepairStats` at `AgentEnd` so
    /// the UI can print "repaired 3 inputs (1 md-link unwrap, 2
    /// null-strip), 0 invalid" at session close.
    pub repair_stats: std::sync::Arc<super::tool_input_repair::RepairStats>,

    /// dirge-7bwx review-fix #2: per-call notes from the
    /// loop-level truncation closer (`apply_truncation_repair` in
    /// `run.rs`). Keyed by tool_call_id. `prepare_tool_call`
    /// drains the entry for its call and prepends each note to
    /// the tool result content so the model sees the repair
    /// ("[read_file] closed unterminated string" or
    /// "[read_file] ⚠️ TRUNCATION UNRECOVERABLE: …").
    /// Mirrors Reasonix `repair/index.ts:100-101, :106` which
    /// forwards `r.notes` into `report.notes` → next-turn assistant
    /// context.
    pub truncation_notes:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>>,

    /// Phase-3 dynamic-tool-search: per-session "loaded" tool set.
    /// When `Some`, the request builder filters tool defs sent to
    /// the model to (a) the always-on set
    /// (`tools::tool_search::ALWAYS_ON_TOOLS`), (b) tools whose
    /// names are in this set, and (c) `tool_search` itself.
    ///
    /// `None` (the default) preserves legacy behavior — every
    /// registered tool definition ships every turn. The `tool_search`
    /// meta-tool inserts names into this set when the model
    /// discovers a needed tool; the SAME Arc is shared between
    /// the tool's executor and the filter inside the request
    /// builder, so a tool the model just discovered shows up on
    /// the next turn's request.
    pub tool_def_filter:
        Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,

    /// Phase-3 dynamic-tool-search opt-in. Mirrors the
    /// config.json `dynamic_tool_search` key. When `true` the
    /// session allocates a `tool_def_filter` and includes the
    /// `tool_search` tool in the registry; when `false` (default)
    /// the loop runs in legacy "ship every tool every turn"
    /// mode. Carried on `LoopConfig` so non-loop callers can
    /// inspect the setting without rebuilding the request-side
    /// filter independently.
    pub dynamic_tool_search: bool,

    /// Phase 4 part 1: alternate stream function used for ONE call
    /// after a repair-exhaustion or tree-sitter failure. None when
    /// escalation isn't configured.
    pub escalation_stream_fn: Option<super::stream::StreamFn>,

    /// Phase 4 part 1: name of the escalation provider (for tracing /
    /// UI surfacing). `None` when no escalation configured.
    pub escalation_provider_name: Option<String>,

    /// Phase 4 part 1: shared state — when `Some(reason)`, the NEXT
    /// call to `stream_assistant_response` swaps to `escalation_stream_fn`
    /// and clears the flag. `reason` propagates to the LoopEvent and
    /// to the tool-result Note.
    pub escalation_pending:
        std::sync::Arc<std::sync::Mutex<Option<super::message::EscalationReason>>>,

    /// Phase 4 part 1: per-session cap to prevent ping-ponging. Default 3.
    /// `try_arm_escalation` decrements a per-session counter and refuses
    /// to arm once it hits zero.
    pub escalation_max_per_session: usize,

    /// Phase 4 part 1: remaining escalation budget for this session.
    /// Initialised to `escalation_max_per_session`; decremented by
    /// `try_arm_escalation`. Shared Arc<AtomicUsize> so the
    /// counter survives `LoopConfig::clone()` (the loop re-clones
    /// across retry boundaries).
    pub escalation_remaining: std::sync::Arc<std::sync::atomic::AtomicUsize>,

    /// Phase 4 part 2: per-session file-touch tracker for context-depth
    /// reminders. None when the feature isn't configured.
    pub file_touch_tracker: Option<std::sync::Arc<super::context_depth::FileTouchTracker>>,

    /// F6: per-run verifier gate. Watches for code edits vs. shell runs
    /// and, at the finalization boundary, injects a one-time "verify
    /// before done" nudge when code was changed but nothing was run to
    /// check it. None disables it (loop behaves byte-identically).
    pub verifier: Option<std::sync::Arc<super::verifier::VerifierGate>>,

    /// F6 tier 3: optional bounded LLM critic. `Some` only when a
    /// `critic_provider` is configured; the verifier escalates to it at
    /// finalization on substantive runs. `None` = no critic (default).
    pub critic_fn: Option<super::critic::CriticFn>,

    /// Diff-aware code reviewer judge (dirge-iyf5). `Some` only when a
    /// `critic_provider` is configured (it reuses that judge client with
    /// `code_review::REVIEW_PREAMBLE`). At finalization, on a run that left
    /// uncommitted changes, it reviews the diff and surfaces severity-ranked
    /// findings. `None` = no reviewer (default).
    pub code_review_fn: Option<super::critic::CriticFn>,

    /// Goal gate's judge callback. Decoupled from `critic_fn`: built at
    /// `build_agent` time from the same critic provider but baking its OWN
    /// `GOAL_PREAMBLE`, so a critic preamble override or a `critic: false`
    /// prompt does not steer goal judgements. `None` = no judge (default).
    pub goal_fn: Option<super::critic::CriticFn>,

    /// Goal gate: an opt-in natural-language stop condition for
    /// autonomous runs. When `Some` AND `goal_fn` is configured (the
    /// judge), each finalization is held until the judge rules the
    /// condition met, bounded by [`super::goal::MAX_GOAL_REACT`]. `None`
    /// = no gate (default), so interactive and unparameterized runs are
    /// unaffected.
    pub goal: Option<String>,

    /// dirge-nqr: hard cap on assistant turns within a single run.
    /// `None` = unlimited (matches the legacy behaviour). When set,
    /// the run loop terminates after `max_turns` assistant turns
    /// have completed and emits a system message stating the cap
    /// was hit. Honored by both the interactive and `--print`
    /// paths; the CLI's `--max-agent-turns` / config's
    /// `max_agent_turns` set it via `AnyAgent::with_max_turns`.
    pub max_turns: Option<usize>,
}

/// `convertToLlm` signature. Synchronous in pi (returns
/// `Message[] | Promise<Message[]>` — we narrow to sync here
/// since the typical implementation is pure filter/map and the
/// async case can be polyfilled by awaiting inside the closure
/// before returning).
///
/// Phase 4 may relax to async once a real async caller emerges.
pub type ConvertToLlmFn =
    std::sync::Arc<dyn Fn(&[serde_json::Value]) -> Vec<serde_json::Value> + Send + Sync>;

/// `transformContext` signature. Pi: `(messages, signal?) =>
/// Promise<AgentMessage[]>`. We accept the signal but don't
/// expose it to the closure in phase 1 — the signal-aware
/// variant lands when a real transform implementation needs it.
pub type TransformContextFn = std::sync::Arc<
    dyn Fn(
            Vec<serde_json::Value>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<serde_json::Value>> + Send>>
        + Send
        + Sync,
>;

/// dirge-jia8: observe-only "compaction is about to run" callback.
/// Receives `(message_count, estimated_tokens)`. Cannot cancel — the
/// fold proceeds regardless (cancelling an emergency fold would
/// overflow the context window on the next call).
pub type OnBeforeCompactFn = std::sync::Arc<
    dyn Fn(usize, u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// dirge-jia8: custom-summary callback. Receives the to-be-summarized
/// middle message slice; returns `Some(summary)` to use instead of
/// the LLM summarizer, or `None` to fall through to the LLM. The
/// returned summary is still validated by `validate_summary`; an
/// invalid one falls through.
pub type OnCompactFn = std::sync::Arc<
    dyn Fn(
            Vec<serde_json::Value>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync,
>;

/// dirge-jia8: bundle of plugin compaction hooks. Bundled into one
/// `LoopConfig` field to keep the constructor surface small.
#[derive(Clone)]
pub struct CompactionHooks {
    pub on_before: OnBeforeCompactFn,
    pub on_compact: OnCompactFn,
}

/// `getApiKey` signature. Pi: `(provider: string) =>
/// Promise<string | undefined> | string | undefined`.
pub type GetApiKeyFn = std::sync::Arc<
    dyn Fn(&str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync,
>;

impl std::fmt::Debug for LoopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopConfig")
            .field("convert_to_llm", &"<fn>")
            .field(
                "transform_context",
                &self.transform_context.as_ref().map(|_| "<fn>"),
            )
            .field("get_api_key", &self.get_api_key.as_ref().map(|_| "<fn>"))
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .field("tool_execution", &self.tool_execution)
            .field(
                "before_tool_call",
                &self.before_tool_call.as_ref().map(|_| "<fn>"),
            )
            .field(
                "after_tool_call",
                &self.after_tool_call.as_ref().map(|_| "<fn>"),
            )
            .field(
                "prepare_next_turn",
                &self.prepare_next_turn.as_ref().map(|_| "<fn>"),
            )
            .field(
                "should_stop_after_turn",
                &self.should_stop_after_turn.as_ref().map(|_| "<fn>"),
            )
            .field(
                "get_steering_messages",
                &self.get_steering_messages.as_ref().map(|_| "<fn>"),
            )
            .field(
                "get_followup_messages",
                &self.get_followup_messages.as_ref().map(|_| "<fn>"),
            )
            .field("reasoning", &self.reasoning)
            .field("thinking_budgets", &self.thinking_budgets)
            .field("headers", &self.headers)
            .field("metadata", &self.metadata)
            .field("request_timeout", &self.request_timeout)
            .field("provider_name", &self.provider_name)
            .field("model_name", &self.model_name)
            .field("compact_model", &self.compact_model)
            .field(
                "tool_def_filter",
                &self.tool_def_filter.as_ref().map(|_| "<set>"),
            )
            .field("dynamic_tool_search", &self.dynamic_tool_search)
            .field(
                "escalation_stream_fn",
                &self.escalation_stream_fn.as_ref().map(|_| "<fn>"),
            )
            .field("escalation_provider_name", &self.escalation_provider_name)
            .field(
                "escalation_pending",
                &self.escalation_pending.lock().ok().and_then(|g| g.clone()),
            )
            .field(
                "escalation_max_per_session",
                &self.escalation_max_per_session,
            )
            .field(
                "escalation_remaining",
                &self
                    .escalation_remaining
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .field(
                "file_touch_tracker",
                &self.file_touch_tracker.as_ref().map(|_| "<tracker>"),
            )
            .field("verifier", &self.verifier.as_ref().map(|_| "<gate>"))
            .field("critic_fn", &self.critic_fn.as_ref().map(|_| "<critic>"))
            .field("goal_fn", &self.goal_fn.as_ref().map(|_| "<judge>"))
            .field("goal", &self.goal)
            .field("max_turns", &self.max_turns)
            .finish()
    }
}

impl Clone for LoopConfig {
    fn clone(&self) -> Self {
        Self {
            convert_to_llm: self.convert_to_llm.clone(),
            transform_context: self.transform_context.clone(),
            compaction_hooks: self.compaction_hooks.clone(),
            get_api_key: self.get_api_key.clone(),
            api_key: self.api_key.clone(),
            tool_execution: self.tool_execution,
            before_tool_call: self.before_tool_call.clone(),
            after_tool_call: self.after_tool_call.clone(),
            prepare_next_turn: self.prepare_next_turn.clone(),
            should_stop_after_turn: self.should_stop_after_turn.clone(),
            get_steering_messages: self.get_steering_messages.clone(),
            get_followup_messages: self.get_followup_messages.clone(),
            reasoning: self.reasoning,
            thinking_budgets: self.thinking_budgets.clone(),
            headers: self.headers.clone(),
            metadata: self.metadata.clone(),
            request_timeout: self.request_timeout,
            provider_name: self.provider_name.clone(),
            model_name: self.model_name.clone(),
            compact_model: self.compact_model.clone(),
            storm_mutating_tools: self.storm_mutating_tools.clone(),
            storm_exempt_tools: self.storm_exempt_tools.clone(),
            repair_stats: self.repair_stats.clone(),
            truncation_notes: self.truncation_notes.clone(),
            tool_def_filter: self.tool_def_filter.clone(),
            dynamic_tool_search: self.dynamic_tool_search,
            escalation_stream_fn: self.escalation_stream_fn.clone(),
            escalation_provider_name: self.escalation_provider_name.clone(),
            escalation_pending: self.escalation_pending.clone(),
            escalation_max_per_session: self.escalation_max_per_session,
            escalation_remaining: self.escalation_remaining.clone(),
            file_touch_tracker: self.file_touch_tracker.clone(),
            verifier: self.verifier.clone(),
            critic_fn: self.critic_fn.clone(),
            code_review_fn: self.code_review_fn.clone(),
            goal_fn: self.goal_fn.clone(),
            goal: self.goal.clone(),
            max_turns: self.max_turns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ToolExecutionMode` round-trips as lowercase, matching pi's
    /// TypeScript literal union. Verifies the serde rename rule.
    #[test]
    fn tool_execution_mode_wire_format() {
        assert_eq!(
            serde_json::to_string(&ToolExecutionMode::Sequential).unwrap(),
            "\"sequential\""
        );
        assert_eq!(
            serde_json::to_string(&ToolExecutionMode::Parallel).unwrap(),
            "\"parallel\""
        );
        assert_eq!(
            serde_json::from_str::<ToolExecutionMode>("\"sequential\"").unwrap(),
            ToolExecutionMode::Sequential
        );
        assert_eq!(
            serde_json::from_str::<ToolExecutionMode>("\"parallel\"").unwrap(),
            ToolExecutionMode::Parallel
        );
    }

    /// Default for `ToolExecutionMode` is `Parallel` per pi
    /// (`toolExecution?` defaults to `"parallel"` per types.ts:252).
    #[test]
    fn tool_execution_mode_default_is_parallel() {
        assert_eq!(ToolExecutionMode::default(), ToolExecutionMode::Parallel);
    }

    /// `QueueMode` uses kebab-case for `OneAtATime` to match pi's
    /// literal `"one-at-a-time"`. Easy to break if a future edit
    /// changes the `rename_all` rule.
    #[test]
    fn queue_mode_wire_format() {
        assert_eq!(serde_json::to_string(&QueueMode::All).unwrap(), "\"all\"");
        assert_eq!(
            serde_json::to_string(&QueueMode::OneAtATime).unwrap(),
            "\"one-at-a-time\""
        );
        assert_eq!(
            serde_json::from_str::<QueueMode>("\"one-at-a-time\"").unwrap(),
            QueueMode::OneAtATime
        );
    }

    /// Every `ThinkingLevel` variant round-trips at its expected
    /// lowercase string. `"xhigh"` is one word, no separator — pi
    /// uses this exact spelling and we must match it.
    #[test]
    fn thinking_level_wire_format() {
        let pairs = [
            (ThinkingLevel::Off, "\"off\""),
            (ThinkingLevel::Minimal, "\"minimal\""),
            (ThinkingLevel::Low, "\"low\""),
            (ThinkingLevel::Medium, "\"medium\""),
            (ThinkingLevel::High, "\"high\""),
            (ThinkingLevel::Xhigh, "\"xhigh\""),
        ];
        for (variant, wire) in pairs {
            let encoded = serde_json::to_string(&variant).unwrap();
            assert_eq!(encoded, wire, "encode mismatch: {variant:?}");
            let decoded: ThinkingLevel = serde_json::from_str(wire).unwrap();
            assert_eq!(decoded, variant, "decode mismatch: {wire}");
        }
    }

    /// Default for `ThinkingLevel` is `Off`. Aligns with pi's
    /// AgentState default `thinkingLevel: "off"` (agent.ts:75).
    #[test]
    fn thinking_level_default_is_off() {
        assert_eq!(ThinkingLevel::default(), ThinkingLevel::Off);
    }

    /// `Context::default()` produces an empty transcript and empty
    /// tool list. Matches pi's "no context yet" starting state.
    #[test]
    fn context_default_is_empty() {
        let ctx = Context::default();
        assert!(ctx.system_prompt.is_empty());
        assert!(ctx.messages.is_empty());
        assert!(ctx.tools.is_empty());
    }

    /// `TurnUpdate::default()` is the "no-op" snapshot — every
    /// field None. Pi's `if (nextTurnSnapshot)` check at
    /// agent-loop.ts:227 treats this case (technically `undefined`
    /// in pi, but a struct of all-None matches the semantics) as
    /// "keep current state for the next turn".
    #[test]
    fn turn_update_default_is_no_op() {
        let upd = TurnUpdate::default();
        assert!(upd.context.is_none());
        assert!(upd.model.is_none());
        assert!(upd.thinking_level.is_none());
    }
}
