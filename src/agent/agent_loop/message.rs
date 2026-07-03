//! Assistant message + stream event types.
//!
//! Ports of pi's message + stream-event vocabulary at the boundary
//! between the LLM stream function and the agent loop. Faithful
//! to pi's discriminated unions; field names follow Rust conventions
//! (snake_case) with serde `rename_all = "camelCase"` where the wire
//! format is pi's TypeScript camelCase.
//!
//! Pi references:
//!   - `AssistantMessage` / `Message` shape from `@earendil-works/pi-ai`
//!     (used throughout agent-loop.ts)
//!   - `AssistantMessageEvent` discriminated union (consumed in
//!     agent-loop.ts:313-356 switch)
//!
//! Phase 1 ports the MINIMAL surface needed for the three tests at
//! pi/test/agent-loop.test.ts:84,131,186. Fields irrelevant to
//! those tests (usage, api, provider, timestamp metadata) are
//! deferred to later phases that actually consume them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Why the assistant turn ended. Port of pi's
/// `AssistantMessage.stopReason` literal union. Pi's exact
/// vocabulary (`"stop" | "toolUse" | "length" | "error" |
/// "aborted"`) preserved as Rust enum variants with camelCase
/// serde rename to match pi's wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// Natural end of the assistant response (no tool calls
    /// pending, no length cap hit).
    Stop,
    /// Model requested one or more tool calls; the loop will
    /// dispatch them and continue.
    ToolUse,
    /// Hit `maxTokens` mid-response.
    Length,
    /// Provider-side error.
    Error,
    /// User-side abort signal (Ctrl+C, /quit, Esc-Esc).
    Aborted,
}

/// One block of content in an `AssistantMessage`. Port of pi's
/// `AssistantMessage.content` block types — text, thinking, and
/// toolCall are the three pi recognizes (`agent-loop.ts:203`).
///
/// `arguments` on the ToolCall variant is `serde_json::Value`
/// rather than pi's typed `Static<TParameters>` because the loop
/// handles tools generically — schema validation happens at
/// dispatch time (`prepareToolCall` / `validateToolArguments`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

/// Final assistant message returned by `stream_assistant_response`.
///
/// Port of pi `AssistantMessage` (used throughout agent-loop.ts).
/// Phase 1 keeps only the fields the three ported tests touch:
/// `content`, `stop_reason`, `error_message`. Later phases will
/// add usage/provider/timestamp metadata as they're needed.
///
/// `role` is implicit (always `"assistant"` in pi's typed union);
/// no need for a Rust field.
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    /// Set when `stop_reason == Error` or `Aborted`. None
    /// otherwise.
    pub error_message: Option<String>,
}

impl AssistantMessage {
    pub fn new(content: Vec<ContentBlock>, stop_reason: StopReason) -> Self {
        Self {
            content,
            stop_reason,
            error_message: None,
        }
    }

    /// Iterate just the toolCall blocks. Used by the loop's
    /// `executeToolCalls` site (agent-loop.ts:203:
    /// `message.content.filter((c) => c.type === "toolCall")`).
    #[allow(dead_code)]
    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Some((id.as_str(), name.as_str(), arguments)),
            _ => None,
        })
    }
}

/// One event from the LLM stream function. Port of pi's
/// `AssistantMessageEvent` discriminated union (consumed in
/// agent-loop.ts:313-356).
///
/// Each non-terminal variant (`*Start`/`*Delta`/`*End`) carries
/// the running `partial` message — pi pushes the partial into
/// `context.messages` at `Start` and replaces the last context
/// entry on each subsequent variant. We carry the partial by
/// value (clones on each emission); in the hot path a future
/// optimization could box it.
///
/// `Done` and `Error` are terminal — the stream emits one and
/// then closes. `streamAssistantResponse` returns the final
/// message on either.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Stream opened; `partial` is the empty-content starting
    /// shape with role/api/provider metadata already populated
    /// by the provider adapter.
    Start { partial: AssistantMessage },

    /// One of the streaming-content lifecycle ticks. Pi has 9
    /// variants in three families (text/thinking/toolcall) ×
    /// (start/delta/end). We collapse those to one variant
    /// carrying a `phase` discriminator so the consumer's match
    /// is flat. The dispatcher in `stream_assistant_response`
    /// treats all 9 identically anyway (just updates the partial
    /// and emits a `MessageUpdate` event).
    Delta {
        partial: AssistantMessage,
        phase: DeltaPhase,
    },

    /// Terminal: stream ended naturally with a final assistant
    /// message. Pi field `{ reason, message }`.
    Done {
        reason: StopReason,
        message: AssistantMessage,
        /// Token usage from the API response, if reported by the provider.
        usage: Option<TokenUsage>,
    },

    /// Terminal: stream ended with a provider-side error.
    Error { error: String },

    /// Non-terminal: the retry layer is about to re-attempt the
    /// stream after sleeping. `attempt` is 1-indexed (this is the
    /// N-th retry about to start). Consumers can surface a banner
    /// so the user isn't staring at silence during backoff.
    Retry {
        attempt: u32,
        delay_ms: u64,
        error: String,
    },
}

/// Token usage from the API response. Carried on the terminal
/// [`StreamEvent::Done`] so the compaction decision engine can read
/// `prompt_tokens` without a separate channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    /// Prompt (input) tokens consumed by this request.
    pub input_tokens: u64,
    /// Completion (output) tokens produced by this request.
    pub output_tokens: u64,
    /// Input tokens served from a provider-managed prefix cache
    /// (DeepSeek `prompt_tokens_details.cached_tokens`, Anthropic
    /// `cache_read_input_tokens`). These are billed at a steep
    /// discount (DeepSeek ~1/10, Anthropic ~1/10) so the cache-hit
    /// ratio is the headline cost lever for cheaper models. A
    /// subset of `input_tokens`.
    pub cached_input_tokens: u64,
    /// Input tokens written to a provider-managed cache this
    /// request (Anthropic `cache_creation_input_tokens`; DeepSeek
    /// reports 0). Billed at a premium but amortized over later
    /// cache hits.
    pub cache_creation_input_tokens: u64,
}

/// Sub-discriminator for `StreamEvent::Delta`. Mirrors pi's nine
/// individual variants in a compact form. The pi-faithful order
/// (text → thinking → toolcall, each in start/delta/end order)
/// is preserved so a side-by-side reader can spot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaPhase {
    TextStart,
    TextDelta,
    /// Matched defensively in the bridge; never constructed by
    /// current providers but kept for protocol completeness.
    #[allow(dead_code)]
    TextEnd,
    ThinkingStart,
    ThinkingDelta,
    ThinkingEnd,
    ToolCallStart,
    ToolCallDelta,
    ToolCallEnd,
}

impl DeltaPhase {
    /// Whether this phase represents observable downstream content
    /// the consumer has already seen. Once any content phase has
    /// streamed we don't retry — re-running would duplicate output
    /// the user already received.
    ///
    /// PROV-5: tool-call deltas are deliberately NOT content. The
    /// model emitting a tool-call JSON fragment is not the same as
    /// the tool actually running — dispatch happens AFTER the stream
    /// ends, downstream of this layer. A 503 mid-tool-call emission
    /// is therefore retryable; the consumer resets its partial
    /// assistant state on `StreamEvent::Retry` (see `stream.rs`) so
    /// the second attempt's tool calls don't accumulate on the
    /// first attempt's.
    ///
    /// Exhaustive by design: a new `DeltaPhase` variant is a compile
    /// error here, so the retry wrapper and the billing-fallback
    /// safety net can't silently disagree about what counts as
    /// committed content.
    pub(crate) fn is_content(&self) -> bool {
        match self {
            DeltaPhase::TextStart
            | DeltaPhase::TextDelta
            | DeltaPhase::ThinkingStart
            | DeltaPhase::ThinkingDelta => true,
            DeltaPhase::TextEnd
            | DeltaPhase::ThinkingEnd
            | DeltaPhase::ToolCallStart
            | DeltaPhase::ToolCallDelta
            | DeltaPhase::ToolCallEnd => false,
        }
    }
}

/// Events the agent loop emits to consumers. Port of pi's
/// `AgentEvent` (types.ts:403). Phase 1 introduces only the
/// `message_*` family that `stream_assistant_response` produces;
/// turn / agent / tool-execution events come in later phases.
///
/// Naming: pi uses `message_start` / `message_end` / `message_update`
/// — those map to `MessageStart` / `MessageEnd` / `MessageUpdate`
/// Opaque handle to a stored image asset. The inner string is a
/// UUID stem that resolves to `<session>/assets/<id>.png` at the
/// rig boundary (the single byte-reader). Server-generated, never
/// user-supplied, so it carries no path-traversal surface.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AssetId(pub String);

impl AssetId {
    pub fn new() -> Self {
        Self(crate::agent::runner::uuid_v4_simple())
    }
}

impl Default for AssetId {
    fn default() -> Self {
        Self::new()
    }
}

/// A reference to a stored image. Carries no bytes — the asset is
/// re-read from disk at the rig boundary every turn it remains in
/// context. `media_type` is always `image/png` in v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRef {
    pub asset_id: AssetId,
    pub media_type: String,
}

/// One part of an ordered, multipart user message. A single message
/// may interleave text and image parts; the provider sees them in
/// order. `tag = "type"` serializes each part as a tagged object so
/// the forward converter can walk a `Vec<Value>` uniformly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserPart {
    Text {
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Image {
        asset_id: AssetId,
        media_type: String,
    },
}

impl UserPart {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    pub fn image(r: ImageRef) -> Self {
        Self::Image {
            asset_id: r.asset_id,
            media_type: r.media_type,
        }
    }
}

/// Plain user-side message. Port of pi `UserMessage` (from
/// `@earendil-works/pi-ai`). `content` is an ordered list of parts
/// so a single user turn can carry interleaved text and images.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: Vec<UserPart>,
}

impl UserMessage {
    /// A single text part — the common case. Lets the ~25 historical
    /// construction sites stay one-liners after the `String` →
    /// `Vec<UserPart>` migration.
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: vec![UserPart::text(s)],
        }
    }

    /// Concatenate every text part with `"\n"`, skipping image parts.
    /// For sites that previously read the single-string `content` as
    /// one blob (logging, task-query extraction, the file-touch tracker).
    pub fn text_joined(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                UserPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// True iff no part is an image (the message is plain text).
    pub fn is_text_only(&self) -> bool {
        self.content
            .iter()
            .all(|p| matches!(p, UserPart::Text { .. }))
    }
}

/// Tool-result message appended to the transcript after a tool
/// dispatches. Port of pi `ToolResultMessage` (used at
/// agent-loop.ts:727-737).
///
/// Pi shape:
///   `{ role: "toolResult", toolCallId, toolName, content, details,
///      isError, timestamp }`
///
/// `role` is implicit. `timestamp` deferred until needed (pi uses
/// it for log ordering; our event sequence already orders things).
/// `content` and `details` carry the LLM-visible payload and the
/// structured metadata respectively — same split as `LoopToolResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    pub details: Value,
    pub is_error: bool,
}

/// Unified message vocabulary the loop transcripts and events
/// reference. Port of pi `AgentMessage = Message |
/// CustomAgentMessages[keyof CustomAgentMessages]` (types.ts:309).
///
/// `Custom` is the extension point — pi lets apps add their own
/// message variants via TypeScript declaration merging; we
/// reserve a `Custom(Value)` variant for the same purpose. Phase
/// 7 finalises the custom-message integration.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopMessage {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    /// App-defined message that `convertToLlm` filters out before
    /// the LLM call. Constructed by plugin_hooks when the `plugin`
    /// feature is active; matched defensively otherwise.
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    Custom(Value),
}

impl LoopMessage {
    /// String role for downstream filters that need to switch on
    /// role without matching the variant. Matches pi's literal
    /// roles: `"user"`, `"assistant"`, `"toolResult"`, plus our
    /// `"custom"` for the extension variant.
    #[allow(dead_code)]
    pub fn role(&self) -> &'static str {
        match self {
            LoopMessage::User(_) => "user",
            LoopMessage::Assistant(_) => "assistant",
            LoopMessage::ToolResult(_) => "toolResult",
            LoopMessage::Custom(_) => "custom",
        }
    }
}

/// Serialise an [`AssistantMessage`] to the placeholder `Value` shape used in
/// `Context.messages`. The `Vec<Value>` transcript is a phase-1 stopgap that
/// phase 7 swaps for typed messages; until then this is the single source of
/// truth for the assistant JSON shape (previously copied into stream.rs,
/// run.rs, and integration.rs).
pub fn assistant_to_value(a: &AssistantMessage) -> Value {
    serde_json::json!({
        "role": "assistant",
        "content": a.content,
        "stopReason": a.stop_reason,
        "errorMessage": a.error_message,
    })
}

/// Serialise a [`ToolResultMessage`] to its `Value` transcript shape. Single
/// source of truth (was duplicated in run.rs and integration.rs).
pub fn tool_result_to_value(t: &ToolResultMessage) -> Value {
    serde_json::json!({
        "role": "toolResult",
        "toolCallId": t.tool_call_id,
        "toolName": t.tool_name,
        "content": t.content,
        "details": t.details,
        "isError": t.is_error,
    })
}

/// Serialise any [`LoopMessage`] to the placeholder `Value` shape. `Custom`
/// values pass through verbatim (the application chose their shape). Single
/// source of truth (was copied verbatim in run.rs and integration.rs).
pub fn loop_message_to_value(msg: &LoopMessage) -> Value {
    match msg {
        // Backward-compat shaping: a text-only message serializes its
        // `content` as a plain string — the historical shape every reader
        // of user content (retry de-dup, steering mocks, heal, etc.)
        // expects. Only a multipart message (one carrying an image part)
        // serializes `content` as an array of part objects.
        LoopMessage::User(u) if u.is_text_only() => serde_json::json!({
            "role": "user",
            "content": u.text_joined(),
        }),
        LoopMessage::User(u) => serde_json::json!({
            "role": "user",
            "content": serde_json::to_value(&u.content).unwrap_or(Value::Null),
        }),
        LoopMessage::Assistant(a) => assistant_to_value(a),
        LoopMessage::ToolResult(t) => tool_result_to_value(t),
        LoopMessage::Custom(v) => v.clone(),
    }
}

/// Canonical string form of a JSON value, for stable tool-call dedup
/// signatures. Sorts object keys and normalizes integers-stored-as-floats
/// (`1.0` ≡ `1`) so two encodings of the same logical call hash equal,
/// regardless of key order or numeric representation.
///
/// Single source of truth for tool-call signatures — used by the scavenge
/// dedup (run.rs) and the storm repeat-loop detector (storm.rs), which
/// previously diverged: storm relied on `serde_json::to_string` (sorted only
/// while `serde_json`'s `preserve_order` feature stays off, and not normalizing
/// `1` vs `1.0`) [dirge-ark9].
pub fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut s = String::from("{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&serde_json::to_string(k).unwrap_or_default());
                s.push(':');
                s.push_str(&canonical_json(&m[*k]));
            }
            s.push('}');
            s
        }
        Value::Array(a) => {
            let mut s = String::from("[");
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&canonical_json(e));
            }
            s.push(']');
            s
        }
        Value::Number(n) => {
            // Normalize integers-stored-as-floats (`1.0` ≡ `1`) so reps match.
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.is_finite() {
                    (f as i64).to_string()
                } else {
                    f.to_string()
                }
            } else {
                n.to_string()
            }
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Pi's `AgentEvent` is plain JSON-serializable in TypeScript;
/// here we keep `LoopEvent` as a Rust-only enum. The fields hold
/// in-memory `AssistantMessage` instances, not their JSON form,
/// so consumers (UI / ACP) get the structured types directly.
/// Wire-format serialization isn't needed yet — phase 6 will add
/// it if cross-process loop hosting becomes a thing.
#[derive(Debug, Clone)]
pub enum LoopEvent {
    /// A new message has appeared in the transcript. Pi field
    /// `{ message: AgentMessage }`. Carries any LoopMessage
    /// variant (user / assistant / toolResult / custom).
    MessageStart { message: LoopMessage },

    /// A streaming assistant message has advanced. Pi carries
    /// the stream event alongside the updated message; phase 1
    /// carries the `phase` discriminator instead. ASSISTANT-only
    /// (other message types never stream).
    MessageUpdate {
        message: AssistantMessage,
        phase: DeltaPhase,
    },

    /// A message has finalized. Pi field
    /// `{ message: AgentMessage }`. Carries any LoopMessage
    /// variant.
    MessageEnd { message: LoopMessage },

    /// Tool dispatch is about to begin. Port of pi
    /// `tool_execution_start` (types.ts:416).
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },

    /// Tool emitted an in-flight partial result via the
    /// `on_update` callback. Port of pi `tool_execution_update`
    /// (types.ts:417).
    ToolExecutionUpdate {
        #[allow(dead_code)]
        tool_call_id: String,
        #[allow(dead_code)]
        tool_name: String,
        #[allow(dead_code)]
        args: Value,
        #[allow(dead_code)]
        partial_result: super::result::LoopToolResult,
    },

    /// Tool dispatch finished (successfully or with error). Port
    /// of pi `tool_execution_end` (types.ts:418).
    ToolExecutionEnd {
        tool_call_id: String,
        #[allow(dead_code)]
        tool_name: String,
        result: super::result::LoopToolResult,
        #[allow(dead_code)]
        is_error: bool,
    },

    /// Agent run started. Phase 4 — emitted by `run_agent_loop`
    /// and `run_agent_loop_continue` once at the top. Port of pi
    /// `agent_start` (types.ts:405).
    AgentStart,

    /// Agent run finished — carries the new messages produced
    /// during the run. Phase 4 — emitted exactly once at the
    /// end of every code path that exits the loop. Port of pi
    /// `agent_end` (types.ts:406).
    AgentEnd { messages: Vec<LoopMessage> },

    /// One turn started. Pi semantics (agent-loop.ts:176): NOT
    /// emitted on the very first iteration (the outer wrapper
    /// already fired `turn_start` after `agent_start`). Subsequent
    /// turns emit this.
    TurnStart,

    /// One turn ended. Carries the assistant message that
    /// completed the turn + the tool results dispatched in this
    /// turn. Port of pi `turn_end` (types.ts:409).
    TurnEnd {
        #[allow(dead_code)]
        message: AssistantMessage,
        #[allow(dead_code)]
        tool_results: Vec<ToolResultMessage>,
    },

    /// Provider-reported token usage for the turn that just
    /// finished. Emitted from `stream_assistant_response` whenever
    /// the provider reported usage on the terminal stream event, so
    /// the host can fold real input/cached counts into the session's
    /// cumulative cache stats (see `Session::record_token_usage`).
    /// Separate from `TurnEnd` because not every provider reports
    /// usage and the host accumulates it independently of turn
    /// bracketing.
    Usage { usage: TokenUsage },

    /// A destructive compaction fold is about to run its summarizer —
    /// emitted before the (slow) LLM call so the UI can show a
    /// "compacting…" indicator during the wait. `ContextCompacted`
    /// follows on completion.
    CompactionStarted { tokens_before: u64 },

    /// Context compression fired — middle turns were summarized
    /// and the session id rotated. The UI should show a status
    /// line. Carries the new session id for lineage tracking.
    /// Port of Hermes's compression event.
    ContextCompacted {
        new_session_id: String,
        tokens_before: u64,
        tokens_after: u64,
        /// Hermes-style summary body the consumer should push into
        /// `Session::compress_reporting`. Empty if the LLM-summary
        /// path didn't run (pruner-only fallback).
        summary: String,
        /// Index of the first message KEPT in the compacted
        /// transcript. Passed straight through to
        /// `Session::compress_reporting` so it knows which middle
        /// turns were folded.
        first_kept_index: usize,
        /// Pruning-only vs prune+summary vs prune+failed-summary
        /// (IMPROVEMENTS_PLAN #5). Bridged to the AgentEvent so the UI
        /// / telemetry can distinguish — and flag a failing summarizer.
        compaction_kind: crate::event::CompactionKind,
        /// Summary model name, if known (`None` today — the summarizer
        /// closure doesn't expose it; threading it is a follow-up).
        summary_model: Option<String>,
    },

    /// Incremental checkpoint: a background summary was generated at a
    /// MiMo-style usage threshold (20/40/60/80% …) WITHOUT folding the
    /// live context. The consumer writes it to the durable session
    /// checkpoint (origin-keyed) so a later resume/overflow recovers a
    /// fresh state, but must NOT rotate the session or drop messages.
    CheckpointRefresh { summary: String },

    /// PROV-2: the retry layer is about to re-attempt the stream.
    /// Carries the attempt number (1-indexed), the backoff delay,
    /// and the original error. The UI can show a banner instead
    /// of freezing during the backoff.
    RetryNotice {
        attempt: u32,
        delay_ms: u64,
        error: String,
    },

    /// A dirge-originated log/notice line for the user (NOT model
    /// output and NOT a transcript message) — e.g. "max agent turns
    /// reached". Distinct from a `MessageStart { User }` so the UI can
    /// render it as a `<system>` log line in the warning color rather
    /// than echoing it as if the user had typed it.
    SystemNotice { content: String },

    /// Phase-1 telemetry (docs/AGENTIC_LOOP_PLAN.md): per-run
    /// aggregate of the input-repair counters, emitted just
    /// before `AgentEnd`. The UI prints a one-line summary when
    /// `!snapshot.is_empty()` so users see at session close
    /// "repaired 3 inputs (1 md-link, 2 null-strip), 0 invalid".
    /// Empty snapshots are not emitted — the run-finish path
    /// only sends this when at least one repair fired.
    RepairStats {
        snapshot: super::tool_input_repair::RepairStatsSnapshot,
    },

    /// Phase 4 part 1: dual-client escalation has been activated for
    /// the next LLM call. Emitted just before the swap fires, so the
    /// UI can surface the change-of-model to the user (avoids
    /// surprise token spend per `docs/AGENTIC_LOOP_PLAN.md` §"Risk").
    EscalationActivated {
        /// Provider alias the escalation routes to (e.g.
        /// `"anthropic"`, `"deepseek-pro"`). Empty string is
        /// permitted but the UI will surface a generic label.
        provider: String,
        /// What triggered the escalation. Carried so the UI / log
        /// can show the cause inline with the activation.
        reason: EscalationReason,
    },
}

/// Phase 4 part 1: cause of an escalation. Surfaced in the
/// `LoopEvent::EscalationActivated` event and forwarded into the UI
/// so the user sees WHY the escalation fired without having to
/// cross-reference tracing logs.
#[derive(Debug, Clone)]
pub enum EscalationReason {
    /// Tool-input repair attempted every available kind and the
    /// final args still failed schema validation. Carries the
    /// tool name for the UI / log.
    RepairExhausted { tool: String },
    /// Tree-sitter syntactic validation rejected the model's
    /// generated code in a `write` / `edit` / `apply_patch` tool
    /// call. Carries the tool name and the path the failure
    /// targeted.
    SyntacticFailure { tool: String, path: String },
}

impl EscalationReason {
    /// One-line human-readable summary for the UI. Kept compact so
    /// it fits on a status line.
    pub fn summary(&self) -> String {
        match self {
            EscalationReason::RepairExhausted { tool } => {
                format!("repair exhausted for {tool}")
            }
            EscalationReason::SyntacticFailure { tool, path } => {
                format!("syntax check failed in {tool} ({path})")
            }
        }
    }
}

impl LoopEvent {
    /// Quick discriminant string for tests (`"message_start"`,
    /// etc.) without going through serde. Lets the phase-1
    /// ported tests assert event sequences cheaply.
    #[allow(dead_code)]
    pub fn kind(&self) -> &'static str {
        match self {
            LoopEvent::MessageStart { .. } => "message_start",
            LoopEvent::MessageUpdate { .. } => "message_update",
            LoopEvent::MessageEnd { .. } => "message_end",
            LoopEvent::ToolExecutionStart { .. } => "tool_execution_start",
            LoopEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
            LoopEvent::ToolExecutionEnd { .. } => "tool_execution_end",
            LoopEvent::AgentStart => "agent_start",
            LoopEvent::AgentEnd { .. } => "agent_end",
            LoopEvent::TurnStart => "turn_start",
            LoopEvent::TurnEnd { .. } => "turn_end",
            LoopEvent::Usage { .. } => "usage",
            LoopEvent::CompactionStarted { .. } => "compaction_started",
            LoopEvent::ContextCompacted { .. } => "context_compacted",
            LoopEvent::CheckpointRefresh { .. } => "checkpoint_refresh",
            LoopEvent::RetryNotice { .. } => "retry_notice",
            LoopEvent::SystemNotice { .. } => "system_notice",
            LoopEvent::RepairStats { .. } => "repair_stats",
            LoopEvent::EscalationActivated { .. } => "escalation_activated",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `DeltaPhase::is_content()` is the single source of truth for
    /// "has downstream output already been emitted?" — it gates both
    /// the retry wrapper (`retry.rs`) and the billing-fallback safety
    /// net (`billing_fallback.rs`). Only text + thinking phases count;
    /// tool-call phases are buffered until the stream ends (PROV-5).
    ///
    /// Exhaustive over the current variants: adding a new one forces a
    /// deliberate classification here (and, because the method is an
    /// exhaustive `match`, a compiler error until it's handled).
    #[test]
    fn delta_phase_is_content_classifies_every_variant() {
        for phase in [
            DeltaPhase::TextStart,
            DeltaPhase::TextDelta,
            DeltaPhase::ThinkingStart,
            DeltaPhase::ThinkingDelta,
        ] {
            assert!(phase.is_content(), "{phase:?} should be content");
        }
        for phase in [
            DeltaPhase::TextEnd,
            DeltaPhase::ThinkingEnd,
            DeltaPhase::ToolCallStart,
            DeltaPhase::ToolCallDelta,
            DeltaPhase::ToolCallEnd,
        ] {
            assert!(!phase.is_content(), "{phase:?} should NOT be content");
        }
    }

    /// Locks the `Value` transcript shape for every `LoopMessage` variant.
    /// This is the single source of truth that stream.rs / run.rs /
    /// integration.rs all route through — a shape change here is a
    /// transcript-format change for `convertToLlm`, so it must be deliberate.
    #[test]
    fn loop_message_to_value_shapes() {
        // Text-only user message → backward-compat plain-string content.
        let user = loop_message_to_value(&LoopMessage::User(UserMessage::text("hello")));
        assert_eq!(
            user,
            serde_json::json!({"role": "user", "content": "hello"})
        );

        // A multipart message (image part present) → array of part objects.
        let multipart = loop_message_to_value(&LoopMessage::User(UserMessage {
            content: vec![
                UserPart::text("see this"),
                UserPart::image(ImageRef {
                    asset_id: AssetId("x".to_string()),
                    media_type: "image/png".to_string(),
                }),
            ],
        }));
        assert_eq!(
            multipart["content"][0],
            serde_json::json!({"type": "text", "text": "see this"})
        );
        assert_eq!(multipart["content"][1]["type"], "image");

        let asst = AssistantMessage {
            content: vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
            stop_reason: StopReason::Stop,
            error_message: None,
        };
        let asst_val = loop_message_to_value(&LoopMessage::Assistant(asst.clone()));
        assert_eq!(asst_val, assistant_to_value(&asst));
        assert_eq!(asst_val["role"], "assistant");
        assert_eq!(asst_val["stopReason"], "stop");
        assert!(asst_val.get("errorMessage").is_some());

        let tr = ToolResultMessage {
            tool_call_id: "c1".to_string(),
            tool_name: "read".to_string(),
            content: vec![],
            details: serde_json::json!({"k": "v"}),
            is_error: false,
        };
        let tr_val = loop_message_to_value(&LoopMessage::ToolResult(tr.clone()));
        assert_eq!(tr_val, tool_result_to_value(&tr));
        assert_eq!(tr_val["role"], "toolResult");
        assert_eq!(tr_val["toolCallId"], "c1");
        assert_eq!(tr_val["isError"], false);

        // Custom passes through verbatim.
        let custom = serde_json::json!({"role": "custom", "x": 1});
        assert_eq!(
            loop_message_to_value(&LoopMessage::Custom(custom.clone())),
            custom
        );
    }

    /// `canonical_json` produces the same signature regardless of object key
    /// order and numeric representation — the property storm + scavenge dedup
    /// both rely on (dirge-ark9).
    #[test]
    fn canonical_json_is_order_and_number_stable() {
        // Key order doesn't matter.
        let a = serde_json::json!({"a": 1, "b": 2});
        let b = serde_json::json!({"b": 2, "a": 1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(canonical_json(&a), "{\"a\":1,\"b\":2}");

        // `1` and `1.0` collapse to the same signature (the storm/run
        // divergence this unifies).
        let int = serde_json::json!({"limit": 1});
        let float = serde_json::json!({"limit": 1.0});
        assert_eq!(canonical_json(&int), canonical_json(&float));

        // Genuine fractionals are preserved; nested objects/arrays recurse.
        let nested = serde_json::json!({"z": [{"y": 2.5, "x": 1}], "a": "s"});
        assert_eq!(
            canonical_json(&nested),
            "{\"a\":\"s\",\"z\":[{\"x\":1,\"y\":2.5}]}"
        );
    }

    /// `StopReason` round-trips at pi's exact wire format.
    /// `ToolUse` is camelCase (one word in wire form). Caught
    /// here so a future enum reshuffle can't break ACP / external
    /// consumers.
    #[test]
    fn stop_reason_wire_format() {
        for (variant, wire) in [
            (StopReason::Stop, "\"stop\""),
            (StopReason::ToolUse, "\"toolUse\""),
            (StopReason::Length, "\"length\""),
            (StopReason::Error, "\"error\""),
            (StopReason::Aborted, "\"aborted\""),
        ] {
            assert_eq!(serde_json::to_string(&variant).unwrap(), wire);
            assert_eq!(serde_json::from_str::<StopReason>(wire).unwrap(), variant);
        }
    }

    /// `ContentBlock` uses `type` discriminator + camelCase. A
    /// toolCall has fields nested under the variant.
    #[test]
    fn content_block_wire_format() {
        let text = ContentBlock::Text {
            text: "hi".to_string(),
        };
        let encoded = serde_json::to_string(&text).unwrap();
        assert!(encoded.contains("\"type\":\"text\""), "got: {encoded}");

        let tool = ContentBlock::ToolCall {
            id: "call_1".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({"path": "/tmp/x"}),
        };
        let encoded = serde_json::to_string(&tool).unwrap();
        assert!(encoded.contains("\"type\":\"toolCall\""), "got: {encoded}");
        assert!(encoded.contains("\"id\":\"call_1\""));
        assert!(encoded.contains("\"name\":\"read\""));
    }

    /// `AssistantMessage::tool_calls()` filters the toolCall
    /// blocks and yields (id, name, args) tuples. Matches pi's
    /// `message.content.filter((c) => c.type === "toolCall")`
    /// at agent-loop.ts:203.
    #[test]
    fn assistant_message_tool_calls_iterator() {
        let msg = AssistantMessage::new(
            vec![
                ContentBlock::Text {
                    text: "thinking…".to_string(),
                },
                ContentBlock::ToolCall {
                    id: "c1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({}),
                },
                ContentBlock::Text {
                    text: "more text".to_string(),
                },
                ContentBlock::ToolCall {
                    id: "c2".to_string(),
                    name: "write".to_string(),
                    arguments: serde_json::json!({"path": "x"}),
                },
            ],
            StopReason::ToolUse,
        );
        let calls: Vec<_> = msg.tool_calls().map(|(id, name, _)| (id, name)).collect();
        assert_eq!(calls, vec![("c1", "read"), ("c2", "write")]);
    }

    /// `LoopEvent::kind()` returns the snake_case discriminator
    /// pi tests compare against. Covers all phase 1-2 variants.
    #[test]
    fn loop_event_kind_strings() {
        let empty = AssistantMessage::new(vec![], StopReason::Stop);
        let assistant_msg = LoopMessage::Assistant(empty.clone());
        assert_eq!(
            LoopEvent::MessageStart {
                message: assistant_msg.clone(),
            }
            .kind(),
            "message_start"
        );
        assert_eq!(
            LoopEvent::MessageEnd {
                message: assistant_msg,
            }
            .kind(),
            "message_end"
        );
        assert_eq!(
            LoopEvent::MessageUpdate {
                message: empty,
                phase: DeltaPhase::TextDelta,
            }
            .kind(),
            "message_update"
        );
        assert_eq!(
            LoopEvent::ToolExecutionStart {
                tool_call_id: "1".to_string(),
                tool_name: "echo".to_string(),
                args: Value::Null,
            }
            .kind(),
            "tool_execution_start"
        );
        assert_eq!(
            LoopEvent::ToolExecutionUpdate {
                tool_call_id: "1".to_string(),
                tool_name: "echo".to_string(),
                args: Value::Null,
                partial_result: Default::default(),
            }
            .kind(),
            "tool_execution_update"
        );
        assert_eq!(
            LoopEvent::ToolExecutionEnd {
                tool_call_id: "1".to_string(),
                tool_name: "echo".to_string(),
                result: Default::default(),
                is_error: false,
            }
            .kind(),
            "tool_execution_end"
        );
    }

    /// `LoopMessage::role()` strings match pi's literal roles.
    #[test]
    fn loop_message_role_strings() {
        assert_eq!(LoopMessage::User(UserMessage::text("hi")).role(), "user");
        assert_eq!(
            LoopMessage::Assistant(AssistantMessage::new(vec![], StopReason::Stop)).role(),
            "assistant"
        );
        assert_eq!(
            LoopMessage::ToolResult(ToolResultMessage {
                tool_call_id: "1".to_string(),
                tool_name: "echo".to_string(),
                content: vec![],
                details: Value::Null,
                is_error: false,
            })
            .role(),
            "toolResult"
        );
        assert_eq!(LoopMessage::Custom(Value::Null).role(), "custom");
    }

    #[test]
    fn userpart_text_serde_roundtrip() {
        let part = UserPart::text("hello");
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json, serde_json::json!({"type": "text", "text": "hello"}));
        let back: UserPart = serde_json::from_value(json).unwrap();
        assert_eq!(part, back);
    }

    #[test]
    fn userpart_image_serde_roundtrip() {
        let part = UserPart::image(ImageRef {
            asset_id: AssetId("abc".to_string()),
            media_type: "image/png".to_string(),
        });
        let json = serde_json::to_value(&part).unwrap();
        // asset_id / media_type serialize camelCase; the newtype AssetId
        // collapses to its inner string.
        assert_eq!(
            json,
            serde_json::json!({"type": "image", "assetId": "abc", "mediaType": "image/png"})
        );
        let back: UserPart = serde_json::from_value(json).unwrap();
        assert_eq!(part, back);
    }

    #[test]
    fn user_message_text_helpers() {
        let m = UserMessage::text("hello");
        assert!(m.is_text_only());
        assert_eq!(m.text_joined(), "hello");
        assert_eq!(m.content.len(), 1);

        let multi = UserMessage {
            content: vec![
                UserPart::text("line one"),
                UserPart::image(ImageRef {
                    asset_id: AssetId::new(),
                    media_type: "image/png".to_string(),
                }),
                UserPart::text("line two"),
            ],
        };
        assert!(!multi.is_text_only());
        // image parts carry no text — text_joined skips them.
        assert_eq!(multi.text_joined(), "line one\nline two");
    }
}
