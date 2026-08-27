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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
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
    /// Absolute maximum capability, distinct from `Xhigh`. OpenAI's
    /// `reasoning_effort` and Anthropic's `effort` both expose `xhigh`
    /// and `max` as separate wire tiers (`xhigh` < `max`). Providers that
    /// lack an `xhigh` tier — DeepSeek and GLM-5.3 (`low`/`high`/`max`
    /// only), per the user's "rounds up" rule — fold `Xhigh` to `max`;
    /// Cerebras (`low`/`medium`/`high` only) folds both to `high`.
    Max,
}

impl ThinkingLevel {
    /// Parse a human effort string into a `ThinkingLevel`. Accepts the
    /// wire names `off` / `minimal` / `low` / `medium` / `high` / `xhigh`
    /// / `max`. Case-insensitive. Returns `None` on an unknown value so a
    /// bad config key or typo fails soft (keeps the default) rather than
    /// aborting a build. `max` is its own tier above `xhigh` (OpenAI and
    /// Anthropic expose both); it is NOT a friendly alias for `xhigh`.
    pub fn from_effort_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    /// The wire-level name (reverse of [`from_effort_str`]). `/effort`
    /// reports the current level to the user using this label.
    pub fn effort_label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
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
    pub xhigh: Option<u32>,
    pub max: Option<u32>,
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

/// Per-request control over whether the model may call tools (dirge-e31n.6).
///
/// Per REQUEST, not per session: it describes one turn, and a value that stuck
/// would silently disarm the model for the rest of the run.
/// There is deliberately NO `Auto` variant. `Option::<ToolChoice>::None`
/// already means "say nothing, let the model decide", and a second spelling of
/// the same thing is a redundant encoding that drifts — the two would have to
/// be kept behaving identically forever, for no gain, since the wire result is
/// the same either way. An enum rather than a bool so `Required` (force a
/// call) can land later without touching every call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolChoice {
    /// Tools are forbidden for this ONE request; the model must answer in
    /// prose. Used where the harness has already TOLD it that calling another
    /// tool cannot help, so the instruction is enforced rather than merely
    /// stated.
    None,
}

impl ToolChoice {
    /// Wire value for OpenAI-compatible and Anthropic request bodies. Both
    /// spell the key `tool_choice` and both read `none` the same way.
    pub fn as_wire(self) -> &'static str {
        match self {
            ToolChoice::None => "none",
        }
    }
}

/// Tri-state gate mode reused by code-review (dirge-iyf5), open-issues
/// (dirge-ksjl), and any future opt-in finalization gates that need an
/// on/off/nagging toggle.
///
/// Advisory vs Blocking is a POLICY choice, not a delivery mechanism: both
/// may inject a model-visible `LoopMessage::User` (tagged, so the TUI
/// attributes it to the system and `emit_harness_notices` mirrors it to a
/// `SystemNotice`). The difference is how hard the gate pushes back.
///
/// - `Off` — the gate is not armed: zero cost.
/// - `Advisory` *(default)* — non-blocking and one-shot: fires at most once
///   per run (per its own budget), never spends a react budget, and never
///   repeatedly re-enters the loop on the same finding, so a tight debug
///   loop is never held up waiting on it. Whether the one shot is a
///   model-visible message is a SEPARATE decision: if the gate's text is an
///   imperative steering what the model does next, it must be a tagged
///   `LoopMessage::User` — a display-only `SystemNotice` the model never
///   sees changes nothing (dirge-1elu.4, arXiv:2604.25850v4 §C.1.4/§C.2.4).
///   Only FYI-for-the-human text belongs in a bare notice.
/// - `Blocking` — await the gate and re-enter the loop on relevant
///   findings, bounded by a per-gate react cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GateMode {
    Off,
    #[default]
    Advisory,
    Blocking,
}

impl GateMode {
    /// Lowercase wire name matching `resolve_code_review_mode`'s vocabulary.
    pub fn as_str(self) -> &'static str {
        match self {
            GateMode::Off => "off",
            GateMode::Advisory => "advisory",
            GateMode::Blocking => "blocking",
        }
    }

    /// Parse a wire value (case-insensitive, trimmed). Empty string is
    /// treated as `Advisory` (the default). Returns `None` for an
    /// unrecognized non-empty value so callers can warn + fall back.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "advisory" => Some(GateMode::Advisory),
            "off" => Some(GateMode::Off),
            "blocking" => Some(GateMode::Blocking),
            _ => None,
        }
    }
}

/// Backwards-compatible alias — every existing `CodeReviewMode::…`
/// reference resolves to `GateMode::…`.
pub type CodeReviewMode = GateMode;

/// Two-mode engagement for the safe-state abort rung (dirge-uw2l.4).
///
/// Unlike [`GateMode`], the default is `Off`: this rung rewrites a failing
/// plan, which is intrusive, so it stays dark until asked for. There is
/// deliberately no `Blocking`/`Auto` variant — an automatic file restore is
/// destructive behind the model's back and is blocked on snapshot coverage
/// for `bash`-mutated files (dirge-uw2l.6), so a config value of `auto` (or
/// `blocking`, or anything else unrecognized) fails to parse and falls back
/// to `Off` with a warning rather than silently doing something other than
/// what its name says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SafeStateMode {
    #[default]
    Off,
    Advisory,
    /// The harness restores the tree itself before asking for a re-plan
    /// (dirge-uw2l.6). It does so ONLY after proving, against git, that the
    /// snapshot store can put back every file that changed since the green
    /// point; if anything changed that the store never captured — a `sed
    /// -i`, a formatter, any `bash` write — it declines and behaves exactly
    /// like [`SafeStateMode::Advisory`]. Never restores a partially-covered
    /// tree, because a half-reverted tree is worse than the broken one.
    Auto,
}

impl SafeStateMode {
    /// Parse a wire value (case-insensitive, trimmed). Empty string and
    /// `"off"` resolve to `Off` (the opt-in default — unlike [`GateMode`],
    /// absence means "do nothing"). `"advisory"` and `"auto"` resolve to
    /// themselves; anything else returns `None` so the resolver can warn +
    /// fall back.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "off" => Some(SafeStateMode::Off),
            "advisory" => Some(SafeStateMode::Advisory),
            "auto" => Some(SafeStateMode::Auto),
            _ => None,
        }
    }
}

/// How the ingestion-time injection scanner handles untrusted tool results.
/// Resolved from `config.injection_scan` — see
/// [`crate::config::Config::resolve_injection_scan_mode`].
///
/// - `Off` — no scanning, results pass through unchanged. Only use when you
///   are certain every tool result is trusted (e.g. a sandboxed internal
///   codebase).
/// - `Advisory` *(default)* — scan every untrusted result and fence positive
///   hits with a `<system-reminder>` warning. The body is still shown so the
///   model sees the content but is structurally warned.
/// - `Block` — same as Advisory, but when ≥2 high-severity findings are
///   present the body is withheld entirely (replaced with a quarantine
///   notice). The tool result still succeeds — the model knows the tool
///   executed but the output was quarantined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InjectionScanMode {
    Off,
    #[default]
    Advisory,
    Block,
}

impl InjectionScanMode {
    /// Lowercase wire name matching `resolve_injection_scan_mode`'s vocabulary.
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            InjectionScanMode::Off => "off",
            InjectionScanMode::Advisory => "advisory",
            InjectionScanMode::Block => "block",
        }
    }

    /// Parse a wire value (case-insensitive, trimmed). Empty string is
    /// treated as `Advisory` (the default). Returns `None` for an
    /// unrecognized non-empty value so callers can warn + fall back.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "advisory" => Some(InjectionScanMode::Advisory),
            "off" => Some(InjectionScanMode::Off),
            "block" => Some(InjectionScanMode::Block),
            _ => None,
        }
    }
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
    ///
    /// NOT WIRED TO ANYTHING. The resolved key reaches
    /// `StreamOptions::api_key` and stops there: it used to be flattened into
    /// the request body, which neither authenticated nor was safe (dirge-
    /// vpma.25), and that path is gone with no replacement. Setting it logs a
    /// warning once per process and changes no request. Auth is owned by the
    /// HTTP client layer — see `provider::client` and the per-provider
    /// transports.
    ///
    /// Argument: provider name (pi: `config.model.provider`).
    /// We pass the model identifier string for now;
    /// phase 4 may substitute a richer model handle.
    pub get_api_key: Option<GetApiKeyFn>,

    /// Static API key fallback for [`Self::get_api_key`] — and, like it, NOT
    /// WIRED TO ANYTHING. Pi field `config.apiKey` (inherited from
    /// `SimpleStreamOptions`).
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

    /// Polled at the finalization boundary: when set and it returns `true`,
    /// the run ends cleanly WITHOUT invoking the critic or any lower "are we
    /// done?" gate, so a parent turn waiting on still-running coordinated
    /// subagents isn't judged prematurely. Wired to
    /// `BackgroundStore::coordinator_generation_running`; the UI re-wakes the
    /// parent once the batch is deliverable.
    pub should_defer_finalization: Option<super::hooks::ShouldDeferFinalizationFn>,

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
    /// GH #816: `max_tokens` to pin on non-reasoning requests — the user's
    /// explicitly configured cap, or dirge's default only for Anthropic
    /// model ids rig has no per-model default for. Seeded from the agent at
    /// spawn time and forwarded to `StreamOptions.max_tokens` per call. The
    /// stream factory applies it to non-reasoning requests on providers
    /// that require the field (Anthropic); a reasoning turn's budget
    /// ceiling always wins over it. `None` leaves the request field unset
    /// so the provider's own default applies.
    pub max_tokens: Option<u64>,
    /// Custom HTTP headers merged with provider defaults. Pi
    /// `StreamOptions.headers?` (types.ts:120). Some rig
    /// providers honor at request build time; others at client
    /// config time only.
    pub headers: std::collections::HashMap<String, String>,
    /// Provider-specific metadata (e.g. Anthropic `user_id` for
    /// abuse / rate-limit tracking). Pi `StreamOptions.metadata?`
    /// (types.ts:142).
    pub metadata: std::collections::HashMap<String, serde_json::Value>,

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

    /// Session asset dir, threaded from `spawn_runner` →
    /// `LoopSpawnConfig.asset_dir` → here, then copied into each
    /// `LlmContext.asset_dir` the loop builds. The rig boundary
    /// reads image assets from here. `None` for sessionless paths.
    pub asset_dir: Option<std::path::PathBuf>,

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

    /// dirge-61sv: per-run transient-tool-retry counters. Same shape and same
    /// reason as `repair_stats` — the dispatch has no access to the tally, so
    /// the count rides here and is latched at run end.
    pub retry_stats: std::sync::Arc<super::tool_retry::RetryStats>,

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

    /// dirge-e31n.2: emit a per-turn `<turn_envelope>` carrying the volatile
    /// session facts (cwd, OS, shell, git branch) instead of freezing them
    /// into the system prompt. Mirrors the `turn_envelope` config knob. When
    /// on, `builder::agent_inner` omits those four lines from the preamble,
    /// so exactly one of the two paths states them.
    pub turn_envelope: bool,

    /// dirge-e31n.6: prompt-recitation detector mode. Mirrors the
    /// `prompt_leak_detect` config knob.
    pub prompt_leak_detect: GateMode,

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
    /// Progress monitor (dirge-uw2l.3) — stall + turn-budget signals.
    /// `None` disables it and the loop behaves exactly as before.
    pub progress: Option<std::sync::Arc<super::progress::ProgressTracker>>,

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
    /// `code_review::REVIEW_PREAMBLE`) AND the resolved
    /// [`CodeReviewMode`] is not `Off`. At finalization, on a run that left
    /// uncommitted changes, it reviews the diff and surfaces severity-ranked
    /// findings. `None` = no reviewer (default).
    pub code_review_fn: Option<super::critic::CriticFn>,

    /// How the armed code reviewer engages at finalization. Only meaningful
    /// when [`code_review_fn`](Self::code_review_fn) is `Some`. Resolved at
    /// `build_agent` time from the prompt-level `code_review` front-matter
    /// override (if any) else `Config::resolve_code_review_mode`; the
    /// default is [`CodeReviewMode::Advisory`].
    pub code_review_mode: CodeReviewMode,

    /// Repository root for the finalization reviewer's diff capture. `None`
    /// (production default) falls back to the process working directory. Set
    /// explicitly so a caller — or a test — can capture the diff of a specific
    /// tree without depending on the process-global CWD (which parallel tests
    /// race).
    pub code_review_repo: Option<std::path::PathBuf>,

    /// How the open-issues finalization gate engages. Default `Off`
    /// (opt-in). Resolved at `build_agent` time from
    /// `Config::resolve_open_issues_gate_mode`.
    pub open_issues_gate_mode: GateMode,
    /// How tiered verification engages (dirge-uw2l.2). `Off` is
    /// byte-identical to the untiered gate. Set by `build_agent` from
    /// `Config::resolve_verification_tiers_mode`.
    pub verification_tiers_mode: GateMode,

    /// dirge-69oe.4: restate loaded skills' anchors every N boundaries.
    /// `0` is off and is the default.
    pub skill_anchor_interval: u32,
    /// How the safe-state abort rung engages (dirge-uw2l.4). `Off` *(default)*
    /// is byte-identical to the loop without the rung. `Advisory` adds a third
    /// failure-ladder rung that replaces a boundary's recovery checkpoint with
    /// a single "abort this approach, re-plan" message when the failure streak
    /// reaches 2× the checkpoint threshold AND unverified edits sit on the tree
    /// AND a verified-green point exists behind the run; it performs NO file
    /// writes. Set by `build_agent` from `Config::resolve_safe_state_abort_mode`.
    pub safe_state_abort_mode: SafeStateMode,

    /// How the publish-state guard engages (dirge-1elu.1). `Off` *(default)*
    /// is byte-identical to the loop without the guard. `Advisory` injects a
    /// model-visible warning (bounded at 2 per run) when a command would
    /// discard verified-green work; `Blocking` suppresses the call pre-dispatch
    /// and returns an error naming the protected paths. Set by `build_agent`
    /// from `Config::resolve_publish_guard_mode`.
    pub publish_guard_mode: GateMode,

    /// dirge-d0e5.2: the deterministic claim/evidence gate's engagement mode
    /// (`off`/`advisory`/`blocking`). `off` *(default)* is byte-identical to
    /// the loop without the gate. Both `advisory` and `blocking` deliver the
    /// same one-shot model-visible nudge — the gate only ever speaks, it
    /// cannot block finalization, so the tri-state is really "on or off" with
    /// room for a future hard mode. Set from `Config::resolve_claim_gate_mode`.
    pub claim_gate_mode: GateMode,

    /// dirge-2m68: deterministic completeness gate mode. `Advisory`
    /// (default) fires one model-visible nudge per run when the final
    /// answer states first-person work the model still intended to do;
    /// `Blocking` re-enters up to three times; `Off` is byte-identical to
    /// the pre-gate loop. Set from `Config::resolve_completeness_gate_mode`.
    pub completeness_gate_mode: GateMode,

    /// dirge-lavc GAP 1: artifact-scope sourcing gate's engagement mode
    /// (`off`/`advisory`/`blocking`). `off` *(default)* is byte-identical to
    /// the loop without the gate. The gate scans ADDED comment lines in the
    /// run's diff for unsupported external-sourcing claims; unlike
    /// `claim_gate_mode` it defaults to `Off` because its false-positive
    /// surface is larger and it must be an explicit opt-in. Set from
    /// `Config::resolve_source_gate_mode`.
    pub source_gate_mode: GateMode,

    /// Active session id for the open-issues gate and tools that need
    /// session-scoping. `None` in review/curator sub-runners and most
    /// tests — the gate is inert without it.
    pub session_id: Option<String>,

    /// Goal gate's judge callback. Decoupled from `critic_fn`: built at
    /// `build_agent` time from the same critic provider but baking its OWN
    /// `GOAL_PREAMBLE`, so a critic preamble override or a `critic: false`
    /// prompt does not steer goal judgements. `None` = no judge (default).
    pub goal_fn: Option<super::critic::CriticFn>,

    /// dirge-5mtx.3: classify judge — returns the INDEX of a chosen option
    /// from a closed answer set, never prose. Built at `build_agent` time from
    /// the same critic provider/client as `critic_fn`/`goal_fn`, but under
    /// [`super::critic::CLASSIFY_PREAMBLE`] and a constrained prompt. The first
    /// consumer is dirge-5mtx.4's blocked-vs-next-step gate; until then nothing
    /// reads it, so it defaults to `None`.
    #[allow(dead_code)] // dirge-5mtx.4: first consumer not yet wired
    pub classify_fn: Option<super::critic::ClassifyFn>,

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
            .field(
                "compaction_hooks",
                &self.compaction_hooks.as_ref().map(|_| "<hooks>"),
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
            .field("max_tokens", &self.max_tokens)
            .field("headers", &self.headers)
            .field("metadata", &self.metadata)
            .field("provider_name", &self.provider_name)
            .field("model_name", &self.model_name)
            .field("compact_model", &self.compact_model)
            .field("storm_mutating_tools", &self.storm_mutating_tools)
            .field("storm_exempt_tools", &self.storm_exempt_tools)
            .field("repair_stats", &self.repair_stats)
            .field("retry_stats", &self.retry_stats)
            .field("truncation_notes", &self.truncation_notes)
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
            .field(
                "code_review_fn",
                &self.code_review_fn.as_ref().map(|_| "<reviewer>"),
            )
            .field("code_review_mode", &self.code_review_mode)
            .field("open_issues_gate_mode", &self.open_issues_gate_mode)
            .field("verification_tiers_mode", &self.verification_tiers_mode)
            .field("safe_state_abort_mode", &self.safe_state_abort_mode)
            .field("publish_guard_mode", &self.publish_guard_mode)
            .field("progress", &self.progress.is_some())
            .field("session_id", &self.session_id)
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
            should_defer_finalization: self.should_defer_finalization.clone(),
            reasoning: self.reasoning,
            thinking_budgets: self.thinking_budgets.clone(),
            max_tokens: self.max_tokens,
            headers: self.headers.clone(),
            metadata: self.metadata.clone(),
            provider_name: self.provider_name.clone(),
            model_name: self.model_name.clone(),
            asset_dir: self.asset_dir.clone(),
            compact_model: self.compact_model.clone(),
            storm_mutating_tools: self.storm_mutating_tools.clone(),
            storm_exempt_tools: self.storm_exempt_tools.clone(),
            repair_stats: self.repair_stats.clone(),
            retry_stats: self.retry_stats.clone(),
            truncation_notes: self.truncation_notes.clone(),
            tool_def_filter: self.tool_def_filter.clone(),
            dynamic_tool_search: self.dynamic_tool_search,
            turn_envelope: false,
            prompt_leak_detect: GateMode::Off,
            escalation_stream_fn: self.escalation_stream_fn.clone(),
            escalation_provider_name: self.escalation_provider_name.clone(),
            escalation_pending: self.escalation_pending.clone(),
            escalation_max_per_session: self.escalation_max_per_session,
            escalation_remaining: self.escalation_remaining.clone(),
            file_touch_tracker: self.file_touch_tracker.clone(),
            verifier: self.verifier.clone(),
            critic_fn: self.critic_fn.clone(),
            code_review_fn: self.code_review_fn.clone(),
            code_review_mode: self.code_review_mode,
            code_review_repo: self.code_review_repo.clone(),
            open_issues_gate_mode: self.open_issues_gate_mode,
            verification_tiers_mode: self.verification_tiers_mode,
            skill_anchor_interval: self.skill_anchor_interval,
            safe_state_abort_mode: self.safe_state_abort_mode,
            publish_guard_mode: self.publish_guard_mode,
            claim_gate_mode: self.claim_gate_mode,
            completeness_gate_mode: self.completeness_gate_mode,
            source_gate_mode: self.source_gate_mode,
            progress: self.progress.clone(),
            session_id: self.session_id.clone(),
            goal_fn: self.goal_fn.clone(),
            goal: self.goal.clone(),
            classify_fn: self.classify_fn.clone(),
            max_turns: self.max_turns,
        }
    }
}

#[cfg(test)]
impl LoopConfig {
    /// Test-only constructor: every field at its default / `None`,
    /// except `convert_to_llm` which tests must always supply. Keeps
    /// the stream.rs test suite (and any other module) from
    /// hand-maintaining a ~45-field struct literal that drifts every
    /// time `LoopConfig` gains a field. dirge-6bcu.
    pub fn for_tests(convert: ConvertToLlmFn) -> Self {
        LoopConfig {
            convert_to_llm: convert,
            transform_context: None,
            compaction_hooks: None,
            get_api_key: None,
            api_key: None,
            tool_execution: ToolExecutionMode::Parallel,
            before_tool_call: None,
            after_tool_call: None,
            prepare_next_turn: None,
            should_stop_after_turn: None,
            get_steering_messages: None,
            get_followup_messages: None,
            should_defer_finalization: None,
            reasoning: None,
            thinking_budgets: None,
            max_tokens: None,
            headers: std::collections::HashMap::new(),
            metadata: std::collections::HashMap::new(),
            provider_name: None,
            model_name: None,
            asset_dir: None,
            compact_model: None,
            storm_mutating_tools: None,
            storm_exempt_tools: None,
            repair_stats: std::sync::Arc::new(super::tool_input_repair::RepairStats::new()),
            retry_stats: std::sync::Arc::new(super::tool_retry::RetryStats::new()),
            truncation_notes: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            tool_def_filter: None,
            dynamic_tool_search: false,
            turn_envelope: false,
            prompt_leak_detect: GateMode::Off,
            escalation_stream_fn: None,
            escalation_provider_name: None,
            escalation_pending: std::sync::Arc::new(std::sync::Mutex::new(None)),
            escalation_max_per_session: 3,
            escalation_remaining: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(3)),
            file_touch_tracker: None,
            verifier: None,
            critic_fn: None,
            code_review_fn: None,
            code_review_mode: CodeReviewMode::Advisory,
            code_review_repo: None,
            open_issues_gate_mode: GateMode::Off,
            verification_tiers_mode: GateMode::Off,
            skill_anchor_interval: 0,
            safe_state_abort_mode: SafeStateMode::Off,
            publish_guard_mode: GateMode::Off,
            claim_gate_mode: GateMode::Off,
            completeness_gate_mode: GateMode::Off,
            source_gate_mode: GateMode::Off,
            progress: None,
            session_id: None,
            goal_fn: None,
            goal: None,
            classify_fn: None,
            max_turns: None,
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
    /// uses this exact spelling and we must match it. `max` is its
    /// own tier (distinct from `xhigh`) for OpenAI/Anthropic.
    #[test]
    fn thinking_level_wire_format() {
        let pairs = [
            (ThinkingLevel::Off, "\"off\""),
            (ThinkingLevel::Minimal, "\"minimal\""),
            (ThinkingLevel::Low, "\"low\""),
            (ThinkingLevel::Medium, "\"medium\""),
            (ThinkingLevel::High, "\"high\""),
            (ThinkingLevel::Xhigh, "\"xhigh\""),
            (ThinkingLevel::Max, "\"max\""),
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

    /// dirge-6bcu: the manual `Debug` impl must enumerate EVERY
    /// field of `LoopConfig`. Several fields were silently dropped,
    /// so debug logs under-reported the config. This guards
    /// completeness — if a future field is added to the struct, add
    /// its name here so it can't quietly disappear from `{:?}`.
    #[test]
    fn loop_config_debug_includes_all_fields() {
        let convert: ConvertToLlmFn = std::sync::Arc::new(|m: &[serde_json::Value]| m.to_vec());
        let config = LoopConfig::for_tests(convert);
        let rendered = format!("{config:?}");
        // Fields that were historically omitted from the Debug impl.
        for field in [
            "code_review_fn",
            "compaction_hooks",
            "storm_mutating_tools",
            "storm_exempt_tools",
            "truncation_notes",
            "repair_stats",
            "retry_stats",
        ] {
            assert!(
                rendered.contains(field),
                "Debug impl omits `{field}`\noutput: {rendered}"
            );
        }
    }

    /// `CodeReviewMode` is a type alias for `GateMode` — they are the same
    /// type, not a newtype wrapper (dirge-hsvw).
    #[test]
    fn code_review_mode_is_gate_mode() {
        // The alias means these are the SAME type — this compiles only if
        // CodeReviewMode = GateMode.
        let _: GateMode = CodeReviewMode::Off;
        let _: GateMode = CodeReviewMode::Advisory;
        let _: GateMode = CodeReviewMode::Blocking;
        let _: CodeReviewMode = GateMode::Off;
        let _: CodeReviewMode = GateMode::Advisory;
        let _: CodeReviewMode = GateMode::Blocking;
        // as_str and from_wire are defined on GateMode.
        assert_eq!(CodeReviewMode::Advisory.as_str(), "advisory");
        assert_eq!(CodeReviewMode::from_wire("off"), Some(GateMode::Off));
    }

    #[test]
    fn from_effort_str_parses_seven_levels_with_distinct_max() {
        assert_eq!(
            ThinkingLevel::from_effort_str("off"),
            Some(ThinkingLevel::Off)
        );
        assert_eq!(
            ThinkingLevel::from_effort_str("minimal"),
            Some(ThinkingLevel::Minimal)
        );
        assert_eq!(
            ThinkingLevel::from_effort_str("low"),
            Some(ThinkingLevel::Low)
        );
        assert_eq!(
            ThinkingLevel::from_effort_str("medium"),
            Some(ThinkingLevel::Medium)
        );
        assert_eq!(
            ThinkingLevel::from_effort_str("high"),
            Some(ThinkingLevel::High)
        );
        assert_eq!(
            ThinkingLevel::from_effort_str("xhigh"),
            Some(ThinkingLevel::Xhigh)
        );
        // `max` is its OWN tier above `xhigh` (OpenAI and Anthropic expose both),
        // not a friendly alias for `Xhigh`.
        assert_eq!(
            ThinkingLevel::from_effort_str("max"),
            Some(ThinkingLevel::Max)
        );
        // case-insensitive + whitespace-tolerant.
        assert_eq!(
            ThinkingLevel::from_effort_str("  High "),
            Some(ThinkingLevel::High)
        );
        assert_eq!(
            ThinkingLevel::from_effort_str("MAX"),
            Some(ThinkingLevel::Max)
        );
        // Xhigh and Max must not collapse — the core invariant of the split.
        assert_ne!(
            ThinkingLevel::from_effort_str("xhigh"),
            ThinkingLevel::from_effort_str("max")
        );
        // unknown fails soft → None (config typo must not abort a build).
        assert_eq!(ThinkingLevel::from_effort_str("turbo"), None);
        assert_eq!(ThinkingLevel::from_effort_str(""), None);
    }

    #[test]
    fn effort_label_round_trips_via_from_effort_str() {
        for level in [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max,
        ] {
            // Every level labels as its own wire name and round-trips through
            // from_effort_str — including Xhigh ("xhigh") and Max ("max")
            // distinctly.
            let label = level.effort_label();
            assert_eq!(ThinkingLevel::from_effort_str(label), Some(level));
        }
    }

    /// The split's whole point: Xhigh and Max are ordered xhigh < max, so a
    /// provider exposing both can keep them distinct while a 3-tier provider
    /// can fold Xhigh up. Verifies the Ord derive putting Max above Xhigh.
    #[test]
    fn max_orders_above_xhigh() {
        assert!(ThinkingLevel::Xhigh < ThinkingLevel::Max);
        assert_eq!(
            ThinkingLevel::from_effort_str("xhigh").unwrap(),
            ThinkingLevel::Xhigh
        );
        assert_eq!(
            ThinkingLevel::from_effort_str("max").unwrap(),
            ThinkingLevel::Max
        );
    }
}
