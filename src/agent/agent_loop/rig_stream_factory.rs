//! Phase 4.5f-2 — build a `StreamFn` from a real rig
//! `CompletionModel`. Plugs into `LoopSpawnConfig.stream_fn`
//! at the composition site, completing the integration between
//! the new loop and an actual LLM.
//!
//! ## What this provides
//!
//! - `rig_stream_fn_from_model(model, tools)` — produces a
//!   `StreamFn` that, per LLM call, builds a rig
//!   `CompletionRequest` from the supplied `LlmContext`, calls
//!   `model.stream(request)`, and wraps the response stream via
//!   `wrap_rig_stream` (4.5a).
//!
//! ## What it does NOT
//!
//! - Recovery / retry around the stream call. Lives in
//!   phase 4.5g — wrappers compose around this `StreamFn` from
//!   the outside.
//! - Permission checking / pre-flight. Tool definitions reach
//!   rig as-is; the loop's `before_tool_call` hook handles
//!   permission decisions at dispatch time, not provider time.
//!
//! ## Message conversion
//!
//! `LlmContext.messages: Vec<Value>` (the placeholder shape
//! phase 0 chose) carries our own message variants serialized
//! as JSON. This module converts each `Value` to a rig
//! `Message`:
//!
//! | Our `role` | rig `Message`                         |
//! |------------|---------------------------------------|
//! | "user"     | `Message::user(content_string)`       |
//! | "assistant"| `Message::Assistant { content: …}`    |
//! | "toolResult"| `Message::tool_result_with_call_id`  |
//! | other      | skipped (custom messages are UI-only) |
//!
//! Assistant content blocks (text / thinking / toolCall) map to
//! rig's `AssistantContent` variants. ToolResult content is
//! flattened to a single text body (rig's helper takes
//! `impl Into<String>`).
//!
//! ## Conversion is lossy by design
//!
//! Our `AssistantMessage.stop_reason` / `error_message` are
//! loop-internal; rig doesn't model them on the wire (the
//! provider derives stop reason from its own stream). They're
//! dropped in conversion.

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::sync::Arc;

use rig::OneOrMany;
#[cfg(test)]
use rig::completion::CompletionError;
use rig::completion::message::{
    AssistantContent, DocumentSourceKind, Image, ImageMediaType, Message, Reasoning, Text,
    ToolCall, ToolFunction, UserContent,
};
use rig::completion::{CompletionModel, CompletionRequestBuilder, GetTokenUsage, ToolDefinition};
use serde_json::Value;

use super::message::{ContentBlock, StreamEvent};
use super::rig_stream::wrap_rig_stream;
use super::stream::{LlmContext, StreamFn};
use super::tool::LoopTool;

use futures::Stream;
use std::pin::Pin;

/// Build a `StreamFn` that drives a rig `CompletionModel`. Each
/// invocation of the returned closure builds a
/// `CompletionRequest` from the supplied `LlmContext`, calls
/// `model.stream(request).await`, and wraps the result via
/// `wrap_rig_stream`.
///
/// `tools` is captured at construction — rig wants tool
/// definitions in the request, and the loop's tool registry is
/// stable across turns. If tools ever need to vary per-call
/// (e.g. dynamic tool sets), pass an empty `tools` here and
/// have the caller inject definitions via a different
/// mechanism.
///
/// The model is cloned per-call so the closure can be `Fn`
/// (multi-call). `CompletionModel: Clone` is part of the trait
/// bounds so this is always cheap (Arc-internally in most rig
/// impls).
#[cfg(test)]
pub fn rig_stream_fn_from_model<M>(
    model: M,
    tools: Vec<ToolDefinition>,
    chunk_timeout: Option<std::time::Duration>,
) -> StreamFn
where
    M: CompletionModel + Clone + Send + Sync + 'static,
    M::StreamingResponse: Clone + Unpin + Send + Sync + GetTokenUsage + 'static,
{
    rig_stream_fn_from_model_with_provider(model, tools, chunk_timeout, None, None)
}

/// Provider-aware variant: takes the provider name (e.g.
/// "anthropic", "openai") so reasoning options get mapped to the
/// shape the specific provider expects. When `provider_name`
/// is `None`, falls back to generic additional_params keys
/// (which most providers will ignore — useful for tests or
/// debugging only).
///
/// Production callers should always pass `Some(name)`.
#[allow(dead_code)]
pub fn rig_stream_fn_from_model_with_provider<M>(
    model: M,
    tools: Vec<ToolDefinition>,
    chunk_timeout: Option<std::time::Duration>,
    provider_name: Option<String>,
    model_name: Option<String>,
) -> StreamFn
where
    M: CompletionModel + Clone + Send + Sync + 'static,
    M::StreamingResponse: Clone + Unpin + Send + Sync + GetTokenUsage + 'static,
{
    rig_stream_fn_from_model_with_filter(
        model,
        tools,
        chunk_timeout,
        provider_name,
        model_name,
        None,
    )
}

/// Phase-3 dynamic-tool-search variant: takes an optional
/// `tool_def_filter` Arc shared with `LoopConfig.tool_def_filter`.
///
/// When `Some`, the per-request tool list is filtered to
/// `tools::tool_search::ALWAYS_ON_TOOLS` + names present in the
/// set (plus `tool_search` itself). When `None`, the full
/// `tools` Vec ships every turn — byte-for-byte identical to the
/// pre-Phase-3 path.
///
/// The filter is read fresh per request (Arc + Mutex), so a
/// `tool_search` call that inserts a name into the set is
/// visible on the very next turn's request.
pub fn rig_stream_fn_from_model_with_filter<M>(
    model: M,
    tools: Vec<ToolDefinition>,
    chunk_timeout: Option<std::time::Duration>,
    provider_name: Option<String>,
    model_name: Option<String>,
    tool_def_filter: Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
) -> StreamFn
where
    M: CompletionModel + Clone + Send + Sync + 'static,
    M::StreamingResponse: Clone + Unpin + Send + Sync + GetTokenUsage + 'static,
{
    let tools = Arc::new(tools);
    let provider_name = Arc::new(provider_name);
    let model_name = Arc::new(model_name);
    let filter = Arc::new(tool_def_filter);
    // The previous request's prefix fingerprint, so a change can be reported
    // against it. Lives HERE — one per `StreamFn`, which is one per agent —
    // rather than in a process global, because subagents and forked reviewers
    // each build their own and a shared slot would report every switch between
    // them as drift.
    let last_prefix = Arc::new(std::sync::Mutex::new(
        None::<super::prefix::PrefixFingerprint>,
    ));
    Arc::new(move |ctx: LlmContext, opts: super::stream::StreamOptions| {
        let model = model.clone();
        let tools = tools.clone();
        let provider_name = provider_name.clone();
        let model_name = model_name.clone();
        let filter = filter.clone();
        let last_prefix = last_prefix.clone();
        invoke_one_stream(
            model,
            tools,
            ctx,
            chunk_timeout,
            opts,
            provider_name,
            model_name,
            filter,
            last_prefix,
        )
    })
}

/// Build a stream that, when polled, performs the model.stream
/// call asynchronously and forwards the wrapped events. Returns
/// a `Pin<Box<dyn Stream<Item = StreamEvent> + Send>>` directly
/// — no outer Future indirection, matches the `StreamFn`
/// signature.
///
/// Errors from message conversion / the `model.stream` call
/// surface as a single `Error` event so the caller's loop
/// observes them uniformly.
#[allow(clippy::too_many_arguments)]
fn invoke_one_stream<M>(
    model: M,
    tools: Arc<Vec<ToolDefinition>>,
    ctx: LlmContext,
    chunk_timeout: Option<std::time::Duration>,
    opts: super::stream::StreamOptions,
    provider_name: Arc<Option<String>>,
    model_name: Arc<Option<String>>,
    tool_def_filter: Arc<
        Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
    >,
    last_prefix: Arc<std::sync::Mutex<Option<super::prefix::PrefixFingerprint>>>,
) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>>
where
    M: CompletionModel + Clone + Send + Sync + 'static,
    M::StreamingResponse: Clone + Unpin + Send + Sync + GetTokenUsage + 'static,
{
    Box::pin(async_stream::stream! {
        // 1. Convert our messages to rig messages.
        let provider: Option<&str> = provider_name.as_ref().as_deref();
        // GH #832: the reasoning wire is not uniform across a provider's own
        // generations (Gemini 2.5 and Gemini 3 take mutually exclusive thinking
        // knobs), so every reasoning decision below is keyed by the model too.
        let model_id: Option<&str> = model_name.as_ref().as_deref();
        // GH #821: how a stored thinking block replays to THIS request —
        // Anthropic schema-requires the block's original signature (minted
        // per model), everyone else keeps the unsigned echo unchanged.
        let thinking_replay = thinking_replay_for(
            provider,
            model_id,
            turn_reasoning_enabled(provider, model_id, &opts),
        );
        // GH #833: decided over the WHOLE history first, because a tool call
        // that cannot be replayed takes its paired result with it — a decision
        // no per-message pass can make.
        let tool_call_replay = tool_call_replay_for(provider, model_name.as_ref().as_deref());
        let dropped_tool_calls = unreplayable_tool_calls(&ctx.messages, tool_call_replay);
        if !dropped_tool_calls.is_empty() {
            tracing::warn!(
                target: "dirge::provider",
                model = %model_name.as_ref().as_deref().unwrap_or("default"),
                pairs = dropped_tool_calls.len(),
                "dropping tool calls (with their results) that carry no signature this model can replay",
            );
        }
        let rig_messages: Vec<Message> = ctx
            .messages
            .iter()
            .filter_map(|message| value_to_rig_message_with_replay(message, provider, thinking_replay, tool_call_replay, &dropped_tool_calls, ctx.asset_dir.as_deref()))
            .collect();

        // 2. Split: last is prompt; rest is chat_history.
        let (prompt, history) = if rig_messages.is_empty() {
            yield StreamEvent::Error {
                error: "rig_stream_fn: empty message list — no prompt to send".to_string(),
            };
            return;
        } else {
            let mut messages = rig_messages;
            let last = messages.pop().unwrap();
            (last, messages)
        };

        // 3. Build the rig CompletionRequest. Phase 4.6: pack
        //    reasoning + headers + metadata into the request's
        //    `additional_params` so providers that know about
        //    these fields can read them. Rig's underlying
        //    provider implementations vary in which they honor;
        //    unsupported fields are silently ignored downstream.
        let mut builder = CompletionRequestBuilder::new(model.clone(), prompt);
        let system_prompt = ctx.system_prompt;
        let history_len = history.len();

        // Phase-3: filter tool defs to the always-on set + loaded
        // set + `tool_search`. When no filter is installed, ship
        // the full `tools` Vec unchanged — preserves legacy
        // behavior byte-for-byte.
        let outgoing_tools: Vec<ToolDefinition> =
            filter_tool_defs(&tools, tool_def_filter.as_ref().as_ref());

        // Phase-3 part 3: emit cache-prefix telemetry so external
        // analysis can detect unexpected drift in the cacheable
        // (system + tools) prefix across turns. See
        // docs/PROMPT_CACHE_AUDIT.md.
        emit_cache_prefix_event(
            provider,
            &system_prompt,
            &outgoing_tools,
            history_len,
            &last_prefix,
        );

        // Build additional_params using a per-provider mapper
        // (phase 4.6 follow-up). Each provider has its own
        // shape for reasoning configuration — Anthropic wants
        // `thinking: { type: "enabled", budget_tokens | effort }`,
        // OpenAI Responses wants `reasoning: { effort }`, etc.
        // The mapper produces the right shape; rig's
        // additional_params is opaque so it forwards whatever
        // we give it. Computed before the builder moves `system_prompt` /
        // `outgoing_tools` so the wire dump below can read them.
        let additional = build_provider_additional_params(provider, model_id, &opts);
        // dirge-wire: opt-in dump of the outgoing agent request (turn /
        // escalation / subagent / forked review), so secondary calls are
        // visible alongside the one-shot side-LLM dumps. No-op unless
        // DIRGE_DUMP_REQUESTS is set.
        if crate::provider::wire::enabled() {
            let model = model_name.as_ref().as_deref().unwrap_or("default");
            let tool_names: Vec<String> = outgoing_tools.iter().map(|t| t.name.clone()).collect();
            let messages_bytes: usize = ctx.messages.iter().map(|m| m.to_string().len()).sum();
            crate::provider::wire::dump_turn(
                provider,
                model,
                &system_prompt,
                history_len,
                messages_bytes,
                &tool_names,
                // dirge-vpma.26: NOT `additional.is_some()` — a tool gate or a
                // metadata map fills that too, and labelled turns with thinking
                // off as reasoning-enabled.
                turn_reasoning_enabled(provider, model_id, &opts),
            );
        }

        if !system_prompt.is_empty() {
            builder = builder.preamble(system_prompt);
        }
        builder = builder.messages(history);
        if !outgoing_tools.is_empty() {
            builder = builder.tools(outgoing_tools);
        }
        if let Some(v) = additional {
            builder = builder.additional_params(v);
        }
        // Pin `max_tokens` when this request needs one — the reasoning
        // ceiling on thinking turns, the resolved `max_tokens` config on
        // non-reasoning Anthropic turns (GH #816). The rules live in
        // `request_max_tokens`.
        if let Some(max_tokens) = request_max_tokens(provider, model_id, &opts) {
            builder = builder.max_tokens(max_tokens);
        }
        let request = builder.build();

        // 4. Call model.stream, bounded by the request-establish deadline.
        //    This await covers the connection/handshake and the wait for the
        //    first response event; the per-chunk timeout only guards gaps
        //    AFTER the stream is live, so a connection that stalls here would
        //    otherwise hang the run with no bound (dirge-u44q). Read from the
        //    process-wide resolved timeouts, the same source every other
        //    consumer uses. The "timed out" wording classifies as a
        //    retryable Network error so the retry wrapper reconnects.
        let establish = crate::timeout::Timeouts::get().request_establish;
        match tokio::time::timeout(establish, model.stream(request)).await {
            Ok(Ok(response)) => {
                // dirge-vzsy: the runaway cap is derived from THIS request's thinking
                // level and budgets, so it can't cut off reasoning the same
                // request just asked for.
                let reasoning_budget = crate::agent::agent_loop::thinking_budget::budget_for_turn(
                    opts.reasoning,
                    opts.thinking_budgets.as_ref(),
                );
                let mut wrapped = wrap_rig_stream(
                    response,
                    chunk_timeout,
                    Some(opts.signal.clone()),
                    reasoning_budget,
                );
                use futures::stream::StreamExt;
                while let Some(evt) = wrapped.next().await {
                    // GH #821: a thinking-block signature is only valid for
                    // the model that minted it, and the capture layer
                    // (rig_stream) doesn't know which model it is streaming
                    // from. Stamp it here — the one place that knows — so
                    // the replay path can tell a same-model echo (attach)
                    // from a cross-model one (drop).
                    let evt = stamp_signature_models(evt, model_name.as_ref().as_deref());
                    yield evt;
                }
            }
            Ok(Err(e)) => {
                // #711: this is where a provider REJECTION lands — the request
                // fails at the response headers, before any event streams. The
                // body carries the actionable reason ("model X is not supported
                // when using Codex with a ChatGPT account"), and downstream it
                // is only ever surfaced as a capped `[task <id>] failed: …`
                // string. Log it verbatim with the provider + model that
                // produced it so the cause is recoverable from the log alone.
                tracing::warn!(
                    target: "dirge::provider",
                    provider = %provider.unwrap_or("default"),
                    model = %model_name.as_ref().as_deref().unwrap_or("default"),
                    error = %e,
                    "llm request rejected before the stream started",
                );
                yield StreamEvent::Error {
                    error: format!("rig stream call failed: {e}"),
                };
            }
            Err(_) => {
                yield StreamEvent::Error {
                    error: format!(
                        "request establish timed out after {}s — the connection/handshake stalled before the first response. Bump `timeouts.request_establish_secs` in config.json if a legitimately slow first response was cut off.",
                        establish.as_secs(),
                    ),
                };
            }
        }
    })
}

/// Phase-3 — pure filter helper. Returns the subset of `tools`
/// to ship in the next request, given the shared loaded-set
/// Arc. When `filter` is `None` the input Vec is returned
/// unchanged (legacy "ship every tool" path). When `Some`, only
/// always-on names (`tools::tool_search::ALWAYS_ON_TOOLS`) and
/// names present in the set survive.
///
/// Names in the set that don't correspond to any registered
/// tool are silently ignored — matches the spec's "if
/// `tool_search` returns names that aren't in the registry,
/// just ignore them" contract.
/// Phase-3 part 3: emit a `prompt_cache_prefix` tracing event
/// carrying stable hashes of the cacheable prefix (system prompt,
/// tool list) plus history length. External analysis can detect
/// unexpected drift across turns of the same session — e.g. a
/// refactor that accidentally moves cwd-injection from session-
/// start to per-turn would surface as a fluctuating system hash.
///
/// Tool list is sorted by name before hashing so unrelated
/// iteration-order differences (e.g. HashMap randomisation in a
/// future MCP backend) don't show up as spurious drift.
///
/// Uses `std::hash::DefaultHasher` (SipHash 1-3) for cheap, stable
/// 64-bit digests. Not cryptographic — telemetry only.
fn emit_cache_prefix_event(
    provider: Option<&str>,
    system_prompt: &str,
    tools: &[ToolDefinition],
    history_len: usize,
    last_prefix: &std::sync::Mutex<Option<super::prefix::PrefixFingerprint>>,
) {
    use crate::sync_util::LockExt;

    let now = super::prefix::PrefixFingerprint::of(system_prompt, tools);

    // Take the comparison and store the new value under one lock, so two
    // concurrent requests on the same agent cannot both read the same
    // predecessor and report the change twice.
    let previous = {
        let mut guard = last_prefix.lock_ignore_poison();
        guard.replace(now)
    };

    tracing::debug!(
        target: "dirge::prompt_cache",
        provider = provider.unwrap_or("unknown"),
        system_hash = format!("{:016x}", now.system_hash()),
        tools_hash = format!("{:016x}", now.tools_hash()),
        tool_count = tools.len(),
        system_bytes = system_prompt.len(),
        history_len = history_len,
        "prompt_cache_prefix"
    );

    // The first request establishes the baseline; there is nothing to have
    // drifted from yet.
    let Some(previous) = previous else { return };
    let change = now.changes_from(&previous);
    if !change.any() {
        return;
    }

    // A prefix change mid-session is not free: the provider caches on a
    // strict byte prefix, so everything from the changed component onward is
    // re-billed at write price. Some changes are deliberate (an `/agent`
    // switch rebuilds the preamble); this says which component moved so the
    // deliberate ones can be told from the accidental ones, and so a
    // `dirge::cache` read-miss on the next turn has a named cause instead of
    // a guess.
    tracing::warn!(
        target: "dirge::prompt_cache",
        provider = provider.unwrap_or("unknown"),
        changed = change.describe(),
        system_changed = change.system,
        tools_changed = change.tools,
        tool_count = tools.len(),
        history_len = history_len,
        "cached request prefix changed mid-session: {} moved, so the cache is \
         invalidated from there on",
        change.describe(),
    );
}

pub fn filter_tool_defs(
    tools: &[ToolDefinition],
    filter: Option<&std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
) -> Vec<ToolDefinition> {
    let denied = crate::permission::PROMPT_DENIED_TOOLS
        .lock_ignore_poison()
        .clone();
    retain_tool_defs(tools, filter, &denied)
}

/// The pure core of [`filter_tool_defs`], with the active prompt's
/// `deny_tools` passed in rather than read off the global.
///
/// Two independent reasons to withhold a definition:
///   - the prompt denies it (dirge-41al) — a hard refusal that holds even
///     under `--yolo`, so its schema can only ever buy a rejected call. This
///     wins over the always-on set: `plan` mode denies `write`, and `write`
///     being always-on must not smuggle it back in.
///   - `dynamic_tool_search` hasn't loaded it yet — the Phase-3 filter.
pub fn retain_tool_defs(
    tools: &[ToolDefinition],
    filter: Option<&std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
    denied: &[String],
) -> Vec<ToolDefinition> {
    let is_denied = |name: &str| {
        denied.iter().any(|entry| {
            let entry = entry.trim();
            entry == name
                || entry
                    .strip_prefix("mcp_tool:")
                    .and_then(|rest| rest.rsplit(':').next())
                    .is_some_and(|bare| bare == name)
        })
    };
    match filter {
        None => tools
            .iter()
            .filter(|td| !is_denied(&td.name))
            .cloned()
            .collect(),
        Some(arc) => {
            let loaded = arc.lock_ignore_poison();
            let always_on: std::collections::HashSet<&str> =
                crate::agent::tools::tool_search::ALWAYS_ON_TOOLS
                    .iter()
                    .copied()
                    .collect();
            tools
                .iter()
                .filter(|td| {
                    !is_denied(&td.name)
                        && (always_on.contains(td.name.as_str()) || loaded.contains(&td.name))
                })
                .cloned()
                .collect()
        }
    }
}

/// Map a MIME media-type string (e.g. `"image/png"`) to rig's
/// `ImageMediaType`. v1 only ever persists `image/png`; the other
/// arms exist so a future non-PNG ref degrades to `None` (rig treats
/// a missing media type as provider-default) instead of panicking.
fn image_media_type(media_type: &str) -> Option<ImageMediaType> {
    match media_type {
        "image/png" => Some(ImageMediaType::PNG),
        "image/jpeg" => Some(ImageMediaType::JPEG),
        "image/gif" => Some(ImageMediaType::GIF),
        "image/webp" => Some(ImageMediaType::WEBP),
        _ => None,
    }
}

/// Build a multipart `Message::User` from serialized `UserPart`
/// objects. Text parts become `UserContent::Text`; image parts are
/// reified from the asset dir as base64 `UserContent::Image`. A part
/// whose asset can't be read (missing file or no asset dir) degrades
/// to a `UserContent::Text` placeholder so the turn still flows.
/// Returns `None` only when there are no usable parts at all.
fn build_user_content(parts: &[Value], asset_dir: Option<&std::path::Path>) -> Option<Message> {
    let mut user_parts: Vec<UserContent> = Vec::new();
    for p in parts {
        let obj = match p.as_object() {
            Some(o) => o,
            None => continue,
        };
        let kind = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "text" => {
                // Skip empty text parts. A caption-less image paste seeds
                // the turn with an empty text part ahead of the image;
                // Anthropic (the flagship vision provider) rejects an
                // empty text content block with a 400, aborting the turn.
                // Mirrors the `!msg.content.is_empty()` guard on the
                // resume path in `runner::convert_history`.
                if let Some(t) = obj.get("text").and_then(|t| t.as_str())
                    && !t.is_empty()
                {
                    user_parts.push(UserContent::Text(Text {
                        text: t.to_string(),
                        additional_params: None,
                    }));
                }
            }
            "image" => {
                let asset_id = obj.get("assetId").and_then(|v| v.as_str()).unwrap_or("");
                let media_type = obj
                    .get("mediaType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("image/png");
                match resolve_image(asset_id, media_type, asset_dir) {
                    Some(img) => user_parts.push(UserContent::Image(img)),
                    None => user_parts.push(UserContent::Text(Text {
                        text: format!("[image unavailable: {asset_id}]"),
                        additional_params: None,
                    })),
                }
            }
            _ => {}
        }
    }
    let content = OneOrMany::many(user_parts).ok()?;
    Some(Message::User { content })
}

/// True iff `id` is a safe asset filename stem: non-empty and only
/// `[A-Za-z0-9_-]`. Asset ids are server-generated UUID stems, but the
/// value round-trips through the durable session JSON, so a tampered id
/// (e.g. `../../etc/secret`) must never reach `Path::join` — it would
/// read an arbitrary `.png`-suffixed file and ship it to the provider.
fn is_safe_asset_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Read `<asset_dir>/<asset_id>.png`, base64-encode it, and wrap in a
/// rig `Image`. `None` if the asset dir is absent, the id is unsafe, or
/// the file is missing/unreadable — the caller degrades to a placeholder.
fn resolve_image(
    asset_id: &str,
    media_type: &str,
    asset_dir: Option<&std::path::Path>,
) -> Option<Image> {
    use base64::Engine;
    let dir = asset_dir?;
    if !is_safe_asset_id(asset_id) {
        return None;
    }
    let path = dir.join(format!("{asset_id}.png"));
    let bytes = std::fs::read(&path).ok()?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(Image {
        data: DocumentSourceKind::Base64(data),
        media_type: image_media_type(media_type),
        detail: None,
        additional_params: None,
    })
}

/// Stand-in body for a tool result that came back empty. Providers reject an
/// empty text content block, and the result can't be dropped without orphaning
/// its tool call — see the call site in `value_to_rig_message_for_provider`.
pub(crate) const EMPTY_TOOL_RESULT_PLACEHOLDER: &str = "(no output)";

/// Legacy 2-knob entry point, kept so the existing tests (and
/// `value_to_rig_message`) stay untouched: replays thinking blocks the way
/// every provider other than Anthropic gets them — unsigned. Production goes
/// through `value_to_rig_message_with_replay`, which the factory
/// calls with the request's model + reasoning state (GH #821).
#[cfg(test)]
fn value_to_rig_message_for_provider(
    value: &Value,
    provider_name: Option<&str>,
    asset_dir: Option<&std::path::Path>,
) -> Option<Message> {
    value_to_rig_message_with_replay(
        value,
        provider_name,
        thinking_replay_for(provider_name, None, false),
        tool_call_replay_for(provider_name, None),
        &std::collections::HashSet::new(),
        asset_dir,
    )
}

fn value_to_rig_message_with_replay(
    value: &Value,
    provider_name: Option<&str>,
    thinking_replay: ThinkingReplay<'_>,
    tool_call_replay: ToolCallReplay<'_>,
    dropped_tool_calls: &std::collections::HashSet<String>,
    asset_dir: Option<&std::path::Path>,
) -> Option<Message> {
    let role = value.get("role").and_then(|r| r.as_str())?;
    match role {
        "user" => {
            // Content is either a legacy/transient string (→ single text
            // part) or an array of serialized `UserPart` objects. Image
            // parts are resolved to a base64 `UserContent::Image` from
            // the asset dir; a missing file/dir degrades to a text
            // placeholder rather than dropping the part.
            let content = value.get("content")?;
            match content {
                // dirge-byun: an empty string would become an empty text
                // content block, which Moonshot/Kimi and GLM reject
                // (`text content is empty`) and Anthropic 400s on. The
                // array shape drops its empty parts in `build_user_content`;
                // this is the same guard for the plain-string shape.
                Value::String(s) if s.is_empty() => None,
                Value::String(s) => Some(Message::user(s.clone())),
                Value::Array(parts) => build_user_content(parts, asset_dir),
                _ => None,
            }
        }
        // dirge-vcu1: a compaction fold replaces the conversation middle
        // with a `role: "system"` summary (and mid-session memory
        // reinjects use the same shape). `default_convert_to_llm`
        // deliberately keeps these, but this converter used to drop them
        // (`_ => None`), so after any fold the summary — and the whole
        // folded middle it stands in for — silently never reached the
        // model. Map it to a `user` message: it stays in the history at
        // its cut-boundary position (the cache-warm system prefix stays
        // stable) and reaches every provider. Mapping to
        // `Message::System` instead would let rig's Anthropic provider
        // hoist the mutable summary into the top-level system field,
        // busting the prompt cache on every post-fold turn.
        "system" => {
            let content = value.get("content").and_then(|c| c.as_str())?;
            // dirge-byun: same empty-text-block guard as the `user` arm.
            if content.is_empty() {
                return None;
            }
            Some(Message::user(content))
        }
        "assistant" => {
            let blocks = value.get("content").and_then(|c| c.as_array())?;
            let include_reasoning = !provider_rejects_reasoning_echo(provider_name);
            let synthesize_call_id = provider_requires_openai_call_ids(provider_name);
            let assistant_contents: Vec<AssistantContent> = blocks
                .iter()
                .filter_map(|block| {
                    value_to_assistant_content(
                        block,
                        include_reasoning,
                        synthesize_call_id,
                        thinking_replay,
                        tool_call_replay,
                    )
                })
                .collect();
            // `OneOrMany::many` errors on empty input; rig
            // returns the error variant rather than constructing
            // an empty OneOrMany. Skip the message entirely if
            // we couldn't extract any usable blocks.
            let content = OneOrMany::many(assistant_contents).ok()?;
            Some(Message::Assistant { id: None, content })
        }
        "tool" | "toolResult" => {
            // Dual convention: loop uses toolCallId, legacy uses
            // tool_call_id. Try both.
            let tool_call_id = value
                .get("toolCallId")
                .or_else(|| value.get("tool_call_id"))
                .and_then(|c| c.as_str())?;
            // GH #833: the call this result belongs to could not be replayed,
            // so the result goes with it. Keeping it alone would orphan it —
            // every backend rejects a result with no matching call.
            if dropped_tool_calls.contains(tool_call_id) {
                return None;
            }
            // Content may be a plain string (legacy `tool` shape)
            // or an array of content blocks (loop `toolResult` shape).
            let text = value
                .get("content")
                .and_then(|c| {
                    if let Some(s) = c.as_str() {
                        Some(s.to_string())
                    } else if let Some(blocks) = c.as_array() {
                        let joined = blocks
                            .iter()
                            .filter_map(|b| {
                                b.as_object().and_then(|o| {
                                    if o.get("type").and_then(|t| t.as_str()) == Some("text") {
                                        o.get("text").and_then(|t| t.as_str()).map(String::from)
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        Some(joined)
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            // dirge-byun: a tool that succeeded silently (`mkdir -p x`,
            // `touch y` — bash returns the merged output verbatim, and appends
            // no exit-code note on success) leaves an empty body. That
            // serializes to an empty text content block, which Moonshot/Kimi
            // and GLM reject with `text content is empty` and Anthropic 400s
            // on; because the result is replayed from history on every later
            // turn, one silent command wedges the session for good. Unlike an
            // empty assistant turn this can't be dropped — Anthropic and
            // OpenAI both reject a tool call with no matching result — so it
            // gets a body that says what happened. opencode substitutes the
            // same placeholder (`tool/shell.ts:576`).
            let text = if text.is_empty() {
                EMPTY_TOOL_RESULT_PLACEHOLDER.to_string()
            } else {
                text
            };
            if provider_requires_openai_call_ids(provider_name) {
                Some(Message::tool_result_with_call_id(
                    tool_call_id,
                    Some(tool_call_id.to_string()),
                    text,
                ))
            } else {
                Some(Message::tool_result(tool_call_id, text))
            }
        }
        _ => None,
    }
}

/// Convert one of our `Value`-shaped messages to a rig
/// `Message`. Returns `None` for unrecognized roles (custom
/// messages get filtered at this boundary — pi calls this
/// out as the `convertToLlm` contract).
///
/// The shapes we recognize match what `run.rs` writes via
/// `loop_message_to_value` and what `stream.rs` writes via
/// `serialize_assistant`:
///
/// - User: `{"role": "user", "content": "<string>"}`
/// - Assistant: `{"role": "assistant", "content": [<blocks>], ...}`
/// - ToolResult: `{"role": "toolResult", "toolCallId": ..., "content": [<blocks>], ...}`
#[cfg(test)]
fn value_to_rig_message(value: &Value) -> Option<Message> {
    value_to_rig_message_for_provider(value, None, None)
}

/// Providers that must NOT receive an echoed-back assistant reasoning block.
///
/// Only `openai` qualifies: its Responses API wants reasoning items keyed by
/// the encrypted ids it issued, which dirge does not retain, so a bare block is
/// rejected and there is no field to rename it to.
///
/// Every other backend keeps its reasoning. Dropping it would lose the model's
/// own chain of thought from the next turn's context, so a backend that spells
/// the field differently is handled by renaming at the wire boundary instead —
/// see `CompressingHttpClient::rewrite_provider_quirks`, which moves
/// `reasoning_content` to `reasoning` for Cerebras. DeepSeek requires the echo
/// on tool-call turns, and llama.cpp/LocalAI chat templates read
/// `message.reasoning_content` back out of the assistant turn.
fn provider_rejects_reasoning_echo(provider_name: Option<&str>) -> bool {
    matches!(provider_name, Some(provider) if provider.eq_ignore_ascii_case("openai"))
}

fn provider_requires_openai_call_ids(provider_name: Option<&str>) -> bool {
    matches!(provider_name, Some(provider) if provider.eq_ignore_ascii_case("openai"))
}

/// GH #821: how a stored thinking block replays to the current request.
///
/// Anthropic signs every thinking block it emits and schema-rejects a
/// replayed block that lacks the signature (`thinking.signature: Field
/// required`), so for that backend the choice is attach-or-drop — there is
/// no unsigned echo. A signature is also only valid for the exact model
/// that minted it (a foreign one is rejected with `Invalid signature in
/// thinking block`), and dirge can replay one session's history to a
/// different model (`/model`, escalation, subagent/review routes), so
/// attaching is gated on the recorded `signatureModel` matching the
/// request's model. Every other provider keeps today's unsigned echo,
/// byte-identical on the wire.
#[derive(Clone, Copy)]
enum ThinkingReplay<'a> {
    /// Replay as an unsigned `Reasoning` block — the pre-#821 behavior,
    /// unchanged for every non-Anthropic provider.
    Unsigned,
    /// Anthropic: attach the stored signature when it was minted by
    /// `model` and this turn has reasoning enabled; otherwise drop the
    /// block (an unsigned or foreign-signed block would 400 either way,
    /// and with reasoning off the echo is not required at all).
    SignedOrDrop {
        model: Option<&'a str>,
        reasoning_enabled: bool,
    },
}

/// Pick the [`ThinkingReplay`] policy for one request.
fn thinking_replay_for<'a>(
    provider_name: Option<&str>,
    model: Option<&'a str>,
    reasoning_enabled: bool,
) -> ThinkingReplay<'a> {
    let anthropic =
        matches!(provider_name, Some(provider) if provider.eq_ignore_ascii_case("anthropic"));
    if anthropic {
        ThinkingReplay::SignedOrDrop {
            model,
            reasoning_enabled,
        }
    } else {
        ThinkingReplay::Unsigned
    }
}

/// GH #833: how a stored tool call replays to the current request.
///
/// Gemini mints a `thought_signature` on every `functionCall` part and rejects
/// a replayed call that lacks one ("Function call is missing a
/// thought_signature in functionCall parts"), so for that backend the choice is
/// attach-or-drop. A signature is only valid for the model that minted it, so
/// attaching is gated on the recorded `signatureModel` matching the request's
/// model — the same gate #821 built for thinking blocks, which stops being a
/// single-provider guard here and becomes the cross-provider one it was for.
///
/// Every other provider keeps today's unsigned replay, byte-identical on the
/// wire.
#[derive(Clone, Copy)]
enum ToolCallReplay<'a> {
    /// Replay with no signature — the pre-#833 behavior, unchanged for every
    /// provider that does not sign tool calls.
    Unsigned,
    /// Gemini: attach the stored signature when it was minted by `model`;
    /// otherwise the call cannot be replayed at all.
    SignedOrDrop { model: Option<&'a str> },
}

/// Pick the [`ToolCallReplay`] policy for one request.
fn tool_call_replay_for<'a>(
    provider_name: Option<&str>,
    model: Option<&'a str>,
) -> ToolCallReplay<'a> {
    if matches!(provider_name, Some(provider) if provider.eq_ignore_ascii_case("gemini")) {
        ToolCallReplay::SignedOrDrop { model }
    } else {
        ToolCallReplay::Unsigned
    }
}

/// The signature a stored `toolCall` block replays with, or `None` when it
/// cannot be replayed to this request at all.
///
/// `Some(None)` and `None` are different answers: the first is "replay it,
/// unsigned", the second is "this call has to go".
fn tool_call_replay_signature(
    obj: &serde_json::Map<String, Value>,
    replay: ToolCallReplay<'_>,
) -> Option<Option<String>> {
    match replay {
        ToolCallReplay::Unsigned => Some(None),
        ToolCallReplay::SignedOrDrop { model } => {
            let signature = obj.get("signature").and_then(|s| s.as_str());
            let minted_by = obj.get("signatureModel").and_then(|s| s.as_str());
            match (signature, minted_by, model) {
                (Some(signature), Some(minted_by), Some(current)) if minted_by == current => {
                    Some(Some(signature.to_string()))
                }
                // No signature (a session saved before #833, or a provider that
                // mints none), a foreign model's signature, or no model id to
                // compare against. Sending it unsigned is the 400 this fixes.
                _ => None,
            }
        }
    }
}

/// GH #833: the ids of tool calls this request cannot replay.
///
/// A tool call cannot be dropped on its own — its paired `tool_result` would be
/// orphaned, which Anthropic and OpenAI reject outright and which leaves the
/// conversation malformed for Gemini too — so the pair goes together. That
/// costs one step of history, against a turn that otherwise cannot be sent at
/// all. Callers use the set to drop both halves in the same pass.
fn unreplayable_tool_calls(
    messages: &[Value],
    replay: ToolCallReplay<'_>,
) -> std::collections::HashSet<String> {
    let mut dropped = std::collections::HashSet::new();
    if matches!(replay, ToolCallReplay::Unsigned) {
        return dropped;
    }
    for message in messages {
        if message.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            let Some(obj) = block.as_object() else {
                continue;
            };
            if obj.get("type").and_then(|t| t.as_str()) != Some("toolCall") {
                continue;
            }
            if tool_call_replay_signature(obj, replay).is_none()
                && let Some(id) = obj.get("id").and_then(|i| i.as_str())
            {
                dropped.insert(id.to_string());
            }
        }
    }
    dropped
}

/// GH #821: record which model minted each captured thinking-block
/// signature. The capture layer (`rig_stream`) sees only the provider's
/// stream, not the model identity, so the factory — which knows the model
/// it was built for — stamps it on every event it forwards. Only blocks
/// that carry a signature and haven't been stamped yet are touched, so
/// everything else in the message is byte-identical.
fn stamp_signature_models(mut evt: StreamEvent, model_name: Option<&str>) -> StreamEvent {
    let Some(model) = model_name else {
        return evt;
    };
    let message = match &mut evt {
        StreamEvent::Start { partial } | StreamEvent::Delta { partial, .. } => partial,
        StreamEvent::Done { message, .. } => message,
        StreamEvent::Error { .. } | StreamEvent::Retry { .. } => return evt,
    };
    for block in &mut message.content {
        // GH #833: tool calls carry a signature too (Gemini's
        // `thought_signature`), and it needs the same stamp for the same
        // reason — a signature is only valid for the model that minted it.
        let stamp = match block {
            ContentBlock::Thinking {
                signature,
                signature_model,
                ..
            }
            | ContentBlock::ToolCall {
                signature,
                signature_model,
                ..
            } => Some((signature, signature_model)),
            _ => None,
        };
        if let Some((signature, signature_model)) = stamp
            && signature.is_some()
            && signature_model.is_none()
        {
            *signature_model = Some(model.to_string());
        }
    }
    evt
}

/// Convert one assistant content block to a rig `AssistantContent`.
/// Recognizes `{type: "text"|"thinking"|"toolCall", ...}`.
fn value_to_assistant_content(
    block: &Value,
    include_reasoning: bool,
    synthesize_call_id: bool,
    thinking_replay: ThinkingReplay<'_>,
    tool_call_replay: ToolCallReplay<'_>,
) -> Option<AssistantContent> {
    let obj = block.as_object()?;
    let kind = obj.get("type").and_then(|t| t.as_str())?;
    match kind {
        "text" => {
            let text = obj.get("text").and_then(|t| t.as_str())?;
            // dirge-byun: last-mile guard, mirroring the user-side skip in
            // `build_user_content`. An empty text block serializes to
            // `{"type": "text", "text": ""}`; Moonshot/Kimi and GLM reject the
            // whole request with `400 invalid_request_error: text content is
            // empty`, and Anthropic 400s on it too. The block can reach here
            // from any turn where the model emitted only reasoning, so the
            // upstream fixes in `convert_history` /
            // `rig_message_to_loop_messages` are not the only entry point.
            // Dropping the sole block of a message drops the message
            // (`OneOrMany::many` errors on empty), which is what we want.
            if text.is_empty() {
                return None;
            }
            Some(AssistantContent::text(text))
        }
        "thinking" => {
            if !include_reasoning {
                return None;
            }
            let text = obj.get("text").and_then(|t| t.as_str())?;
            match thinking_replay {
                ThinkingReplay::Unsigned => Some(AssistantContent::Reasoning(Reasoning::new(text))),
                ThinkingReplay::SignedOrDrop {
                    model,
                    reasoning_enabled,
                } => {
                    // GH #821: attach the block's original signature only
                    // when this request's model is the one that minted it
                    // (recorded as `signatureModel` at capture time) and
                    // reasoning is on this turn. Any other combination —
                    // no signature (legacy/flattened history), a foreign
                    // model's signature, or reasoning off — drops the
                    // block, because Anthropic rejects both an unsigned
                    // and a foreign-signed thinking block.
                    let signature = obj.get("signature").and_then(|s| s.as_str());
                    let signature_model = obj.get("signatureModel").and_then(|s| s.as_str());
                    match (signature, signature_model, model) {
                        (Some(signature), Some(minted_by), Some(current))
                            if reasoning_enabled && minted_by == current =>
                        {
                            Some(AssistantContent::Reasoning(Reasoning::new_with_signature(
                                text,
                                Some(signature.to_string()),
                            )))
                        }
                        _ => None,
                    }
                }
            }
        }
        "toolCall" => {
            let id = obj.get("id").and_then(|t| t.as_str())?.to_string();
            let name = obj.get("name").and_then(|t| t.as_str())?.to_string();
            let arguments = obj.get("arguments").cloned().unwrap_or(Value::Null);
            // GH #833: rig sends this straight back out as the `functionCall`
            // part's `thought_signature`, which Gemini requires. `None` from
            // the helper means the call cannot be replayed to this model at
            // all; the caller has already put its id in the drop set, so its
            // paired result goes too and the conversation stays well-formed.
            let signature = tool_call_replay_signature(obj, tool_call_replay)?;
            Some(AssistantContent::ToolCall(ToolCall {
                call_id: synthesize_call_id.then(|| id.clone()),
                id,
                function: ToolFunction { name, arguments },
                signature,
                additional_params: None,
            }))
        }
        _ => None,
    }
}

/// Build a rig `ToolDefinition` from one of our `LoopTool`s.
/// Returns the trio rig actually consumes (name, description,
/// parameters); label is dropped because rig has no slot for it.
///
/// If the tool has a `flat_parameters` schema (auto-detected via
/// `analyze_schema`), the LLM receives the flat dot-notation
/// variant so it's less likely to drop deeply nested args.
pub fn loop_tool_to_rig_definition(tool: &dyn LoopTool) -> ToolDefinition {
    let parameters = tool
        .flat_parameters()
        .cloned()
        .unwrap_or_else(|| tool.parameters().clone());
    // dirge-tva8: on a small window, ship breadcrumb schemas — first sentence
    // of each description, everything structural untouched. Measured, the full
    // tool surface is 16k tokens built-in and 32.6k with MCP servers, the
    // latter larger than a 32k window in its entirety. Decided here because
    // this is the single point every tool becomes a provider schema, so no
    // caller can be built that forgets. See `compact_schema`.
    if super::compact_schema::in_force() {
        return ToolDefinition {
            name: tool.name().to_string(),
            description: super::compact_schema::compact_description(tool.description()),
            parameters: super::compact_schema::compact_parameters(&parameters),
        };
    }
    ToolDefinition {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        parameters,
    }
}

/// Build the provider-specific `additional_params` Value for a
/// `CompletionRequest` from the user's StreamOptions. Per-provider
/// mapping covers the SHAPE differences between Anthropic
/// (`thinking: { ... }`), OpenAI Responses (`reasoning: {
/// effort }`), and others.
///
/// Returns `None` when there's nothing to send (no reasoning
/// requested, no headers, no metadata) — caller skips
/// `additional_params(...)` to keep the request minimal.
///
/// **Provider mappings**:
///   - "anthropic": `{ "thinking": { "type": "enabled",
///     "budget_tokens": N } }` for budget-based reasoning. Xhigh and
///     Max get distinct (increasing) budgets. Pi's adaptive-thinking
///     effort mode (Opus 4.6+, Sonnet 4.6) is a follow-up — needs
///     model-id sniffing.
///   - "deepseek": `{ "reasoning_effort": "low" | "medium" |
///     "high" | "max" }` — top-level string, not nested inside
///     `reasoning`. DeepSeek has no "xhigh" tier, so Xhigh rounds up
///     to "max" (same ceiling as Max).
///   - "cerebras": `{ "reasoning_effort": "low" | "medium" | "high" }`
///     at the top level. `Minimal` clamps to `low`, `Xhigh`/`Max`
///     clamp to `high`, and `Off` omits the field.
///   - "openai" / "custom" (openai-shaped): `{ "reasoning":
///     { "effort": "low" | "medium" | "high" | "xhigh" | "max" } }`
///     per OpenAI Responses spec — Xhigh and Max pass through
///     distinctly. Maps ThinkingLevel:
///       - Off → omit reasoning
///       - Minimal / Low → "low"
///       - Medium → "medium"
///       - High → "high"
///       - Xhigh → "xhigh"
///       - Max → "max"
///   - "glm": `{ "reasoning_effort": "low" | "high" | "max" }`
///     (top-level). GLM-5.3 has no "xhigh" tier, so Xhigh rounds up
///     to "max"; Minimal/Low → "low", Medium/High → "high".
///   - "openrouter": same as openai (openrouter forwards
///     OpenAI-shape options to the upstream provider).
///   - "gemini": `{ "generationConfig": { "thinkingConfig": { … } } }` —
///     `thinkingBudget: N` on Gemini 2.5 (distinct Xhigh/Max budgets),
///     `thinkingLevel: "low"|"medium"|"high"` on Gemini 3, which takes a depth
///     rather than a budget and rejects a request carrying both. The nesting
///     is what rig's Gemini provider reads; a top-level `thinking_config` is
///     flattened into the request body and rejected outright (GH #832).
///   - "ollama": no reasoning config — local models vary; pass
///     through generic `reasoning_level` key.
///   - None: generic `reasoning_level` key for debugging /
///     ad-hoc consumers.
///
/// **Metadata** is passed through under the conventional `metadata` key
/// regardless of provider — rig's openai-shaped clients merge it into the
/// request body.
///
/// **Headers are not.** They used to be, under a `headers` key, on the belief
/// that "headers are honored where the provider impl reads them". No provider
/// reads them: rig flattens `additional_params` into the request BODY, so the
/// only thing that key ever did was ship `Authorization: Bearer <key>` to the
/// endpoint as body content (dirge-vpma.25).
pub fn build_provider_additional_params(
    provider_name: Option<&str>,
    model_name: Option<&str>,
    opts: &super::stream::StreamOptions,
) -> Option<serde_json::Value> {
    let mut additional = serde_json::Map::new();

    // ----- tool gating (dirge-e31n.6) -----
    // Only `None` is sent. `Auto` is the provider default, and an explicit
    // "auto" is NOT equivalent to omitting the key: some OpenAI-compatible
    // backends reject `tool_choice` when the request carries no `tools` array,
    // so sending it unconditionally would break every tool-less call (the
    // summarizer, the critic, the goal judge) to say something the provider
    // already assumes.
    if let Some(super::types::ToolChoice::None) = opts.tool_choice {
        additional.insert(
            "tool_choice".to_string(),
            serde_json::Value::String(super::types::ToolChoice::None.as_wire().to_string()),
        );
    }

    // ----- reasoning per provider -----
    if let Some(m) = reasoning_params(provider_name, model_name, opts) {
        additional.extend(m);
    } else if opts.reasoning == Some(super::types::ThinkingLevel::Off)
        && let Some(serde_json::Value::Object(m)) =
            crate::provider::adapter::reasoning_profile(provider_name, model_name).disable_params()
    {
        // GH #827: `off` is a level, not an absence. It used to send nothing —
        // even for the providers that have carried a real disable wire all
        // along for tool-less one-shots — and on a model that thinks
        // adaptively when the parameter is missing (Sonnet 5, Opus 5) that
        // left reasoning fully on.
        //
        // Deliberately NOT folded into `reasoning_params`: that function
        // answers "does this turn REQUEST reasoning", which `turn_reasoning_
        // enabled` and #816's `max_tokens` fallback both read, and `off` must
        // stay `false` there.
        additional.extend(m);
    }

    // ----- headers -----
    //
    // dirge-vpma.25: these used to be serialized into `additional_params` under
    // a "headers" key. That could never work and was actively unsafe. rig
    // serde-FLATTENS additional_params into the JSON request BODY, and no
    // provider extracts a body-level "headers" field and promotes it to an HTTP
    // header — so a request built this way (a) did not authenticate and (b)
    // shipped `Authorization: Bearer <key>` inside the body, to an endpoint
    // that may log it.
    //
    // Nothing in production set these (integration.rs passes `api_key: None`
    // and an empty `headers` map on every spawn), so removing the injection
    // changes no live behaviour. It is deliberately NOT replaced with a
    // real-header implementation here: per-request headers belong at the HTTP
    // client layer alongside the transports that already own auth, not in the
    // completion-request builder.
    //
    // Warn rather than drop silently. A knob that quietly does nothing is how
    // this survived — anyone who sets one should learn immediately that it has
    // no consumer, instead of discovering it from an auth failure.
    warn_unapplied_request_overrides(opts);

    // ----- metadata (provider-agnostic) -----
    if !opts.metadata.is_empty() {
        additional.insert(
            "metadata".to_string(),
            serde_json::Value::Object(
                opts.metadata
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        );
    }

    if additional.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(additional))
    }
}

/// The reasoning params this provider wants for `opts`, or `None` when the
/// turn asks for no reasoning — or asks for a level this provider has no wire
/// shape for.
///
/// Single source of truth for "is this a reasoning turn": the params that go on
/// the wire and the flag the wire dump reports are the same decision, so they
/// cannot disagree (dirge-vpma.26).
fn reasoning_params(
    provider_name: Option<&str>,
    model_name: Option<&str>,
    opts: &super::stream::StreamOptions,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let level = opts.reasoning?;
    match crate::provider::adapter::reasoning_profile(provider_name, model_name)
        .effort_params(level, opts.thinking_budgets.as_ref())
    {
        Some(serde_json::Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// Whether this turn puts reasoning params on the wire. See
/// [`reasoning_params`].
pub fn turn_reasoning_enabled(
    provider_name: Option<&str>,
    model_name: Option<&str>,
    opts: &super::stream::StreamOptions,
) -> bool {
    reasoning_params(provider_name, model_name, opts).is_some()
}

/// The `max_tokens` to pin on this request, or `None` to leave the field
/// unset (the provider's own default applies).
///
/// Two independent reasons to set one:
///   - A turn that puts a thinking BUDGET on the wire carries the reasoning
///     ceiling. Anthropic counts thinking against `max_tokens` and rejects
///     the request unless `budget_tokens` is strictly below it — and if we
///     leave `max_tokens` unset rig picks 2048 for any model id it doesn't
///     recognise, which is every Claude 5 id. That combination 400s every
///     turn above `minimal`. The ceiling always wins over the configured
///     value: a user-configured 8192 must never undercut a 16384+ budget.
///   - A NON-reasoning turn on an Anthropic-shaped provider carries the
///     resolved `max_tokens` config (GH #816). rig 0.41 hard-errors with
///     "`max_tokens` must be set for Anthropic" when the request has none
///     and the model id is one it has no default for — again every Claude 5
///     id — so with reasoning off every request failed before the HTTP call.
///
/// Every other case stays unset, byte-identical to before: effort-string
/// providers send no budget, their backends accept an absent `max_tokens`,
/// and forcing a cap on a reasoning turn there could strangle reasoning
/// output (OpenAI counts reasoning tokens against the output limit).
fn request_max_tokens(
    provider: Option<&str>,
    model: Option<&str>,
    opts: &super::stream::StreamOptions,
) -> Option<u64> {
    if let Some(level) = opts.reasoning
        && let Some(ceiling) = crate::provider::adapter::max_tokens_for_reasoning(
            provider,
            model,
            level,
            opts.thinking_budgets.as_ref(),
        )
    {
        return Some(ceiling);
    }
    let anthropic_shaped = matches!(
        crate::provider::adapter::reasoning_profile(provider, model).effort,
        crate::provider::adapter::EffortWire::AnthropicBudget
    );
    if anthropic_shaped && !turn_reasoning_enabled(provider, model, opts) {
        return opts.max_tokens;
    }
    None
}

/// Say so, once per process, when a request-override knob is set but has no
/// consumer (dirge-vpma.25).
///
/// `StreamOptions::headers` and `::api_key` are resolved from `LoopConfig` —
/// including through the `get_api_key` hook — and then reach nothing. They
/// used to be flattened into the request body, which did not authenticate and
/// leaked the bearer; that path is gone and no replacement exists yet.
///
/// Never logs the value of either. The whole point of the bug was a secret
/// going somewhere it should not.
fn warn_unapplied_request_overrides(opts: &super::stream::StreamOptions) {
    use std::sync::Once;
    static WARNED: Once = Once::new();
    let has_key = opts
        .api_key
        .as_deref()
        .map(str::trim)
        .is_some_and(|key| !key.is_empty());
    if !has_key && opts.headers.is_empty() {
        return;
    }
    WARNED.call_once(|| {
        tracing::warn!(
            target: "dirge::provider",
            api_key_set = has_key,
            header_names = %opts.headers.keys().cloned().collect::<Vec<_>>().join(", "),
            "per-request api_key/headers are set but nothing applies them; they \
             reach no HTTP header and are NOT sent. Configure credentials \
             through the provider entry instead",
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::message::UserContent;

    /// User-role value → `Message::User { content: text }`.
    #[test]
    fn user_value_converts_to_user_message() {
        let v = serde_json::json!({"role": "user", "content": "hello"});
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::User { content } => {
                let first = content.first();
                match first {
                    UserContent::Text(t) => assert_eq!(t.text, "hello"),
                    _ => panic!("expected text"),
                }
            }
            _ => panic!("expected User"),
        }
    }

    /// User-role value with the new multipart `content` array (a single
    /// text part) still converts to `Message::User` with that text.
    /// Image parts are resolved to bytes once the asset dir is threaded
    /// into the converter (a later task); text parts must work today.
    #[test]
    fn user_value_multipart_text_array_converts() {
        let v = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "hello world"}],
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::User { content } => match content.first() {
                UserContent::Text(t) => assert_eq!(t.text, "hello world"),
                _ => panic!("expected text"),
            },
            _ => panic!("expected User"),
        }
    }

    /// Multipart user value with an image part resolves the asset from
    /// the asset dir and emits a base64 `UserContent::Image` (PNG) in
    /// order after the text part.
    #[test]
    fn converter_image_part_resolves_to_base64_block() {
        use base64::Engine;
        use rig::completion::message::DocumentSourceKind;
        let dir = std::env::temp_dir().join(format!(
            "dirge-conv-img-{}",
            crate::agent::runner::uuid_v4_simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let png_bytes = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 9, 9, 9];
        std::fs::write(dir.join("abc.png"), png_bytes).unwrap();

        let v = serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "assetId": "abc", "mediaType": "image/png"},
            ],
        });
        let msg = value_to_rig_message_for_provider(&v, None, Some(&dir)).expect("must convert");
        match msg {
            Message::User { content } => {
                let parts: Vec<_> = content.into_iter().collect();
                assert_eq!(parts.len(), 2, "text + image");
                match &parts[0] {
                    UserContent::Text(t) => assert_eq!(t.text, "look"),
                    _ => panic!("expected text first"),
                }
                match &parts[1] {
                    UserContent::Image(img) => match &img.data {
                        DocumentSourceKind::Base64(b64) => {
                            let decoded = base64::engine::general_purpose::STANDARD
                                .decode(b64)
                                .unwrap();
                            assert_eq!(decoded.as_slice(), &png_bytes[..]);
                            assert_eq!(
                                img.media_type,
                                Some(rig::completion::message::ImageMediaType::PNG)
                            );
                        }
                        other => panic!("expected base64, got {other:?}"),
                    },
                    _ => panic!("expected image"),
                }
            }
            _ => panic!("expected User"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An image part whose asset file is missing degrades to a text
    /// placeholder — never panics, never silently drops.
    #[test]
    fn converter_missing_asset_emits_placeholder() {
        let dir = std::env::temp_dir().join(format!(
            "dirge-conv-missing-{}",
            crate::agent::runner::uuid_v4_simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let v = serde_json::json!({
            "role": "user",
            "content": [{"type": "image", "assetId": "nope", "mediaType": "image/png"}],
        });
        let msg = value_to_rig_message_for_provider(&v, None, Some(&dir)).expect("must convert");
        match msg {
            Message::User { content } => match content.first() {
                UserContent::Text(t) => {
                    assert!(
                        t.text.contains("[image unavailable: nope]"),
                        "got: {}",
                        t.text
                    )
                }
                _ => panic!("expected placeholder text"),
            },
            _ => panic!("expected User"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No asset dir available (no session) — image parts degrade to a
    /// placeholder rather than failing the whole message.
    #[test]
    fn converter_no_asset_dir_emits_placeholder() {
        let v = serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "image", "assetId": "x", "mediaType": "image/png"},
            ],
        });
        let msg = value_to_rig_message_for_provider(&v, None, None).expect("must convert");
        match msg {
            Message::User { content } => {
                let parts: Vec<_> = content.into_iter().collect();
                assert_eq!(parts.len(), 2);
                match &parts[1] {
                    UserContent::Text(t) => assert!(t.text.contains("[image unavailable")),
                    _ => panic!("expected placeholder for missing asset dir"),
                }
            }
            _ => panic!("expected User"),
        }
    }

    /// A caption-less image paste seeds an empty text part ahead of the
    /// image. That empty part must be dropped — Anthropic rejects an
    /// empty text content block with a 400, aborting the turn. Only the
    /// image part (here a placeholder, no asset dir) should survive.
    #[test]
    fn converter_drops_empty_text_part() {
        let v = serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": ""},
                {"type": "image", "assetId": "x", "mediaType": "image/png"},
            ],
        });
        let msg = value_to_rig_message_for_provider(&v, None, None).expect("must convert");
        match msg {
            Message::User { content } => {
                let parts: Vec<_> = content.into_iter().collect();
                assert_eq!(parts.len(), 1, "empty text part must be dropped");
                match &parts[0] {
                    UserContent::Text(t) => assert!(
                        t.text.contains("[image unavailable"),
                        "sole part should be the image placeholder, got: {}",
                        t.text
                    ),
                    _ => panic!("expected the image part to survive"),
                }
            }
            _ => panic!("expected User"),
        }
    }

    /// A tampered asset id carrying path-traversal characters must never
    /// reach `Path::join` — it degrades to a placeholder instead of a
    /// file read. Guards against a hand-edited session JSON exfiltrating
    /// an arbitrary `.png` file to the provider.
    #[test]
    fn converter_path_traversal_asset_id_rejected() {
        let root = std::env::temp_dir().join(format!(
            "dirge-conv-trav-{}",
            crate::agent::runner::uuid_v4_simple()
        ));
        let dir = root.join("inner");
        std::fs::create_dir_all(&dir).unwrap();
        // A naive `dir.join("../secret.png")` would escape `inner/` and
        // read this sibling file. Validation must reject the id first.
        std::fs::write(root.join("secret.png"), b"exfiltrated").unwrap();
        for id in ["../secret", "..\\secret", "/etc/passwd", "", ".", "a/b"] {
            let v = serde_json::json!({
                "role": "user",
                "content": [
                    {"type": "image", "assetId": id, "mediaType": "image/png"},
                ],
            });
            let msg =
                value_to_rig_message_for_provider(&v, None, Some(&dir)).expect("must convert");
            match msg {
                Message::User { content } => match content.first() {
                    UserContent::Text(t) => assert!(
                        t.text.contains("[image unavailable"),
                        "traversal id {id:?} must yield a placeholder, got: {}",
                        t.text
                    ),
                    other => panic!("expected placeholder for {id:?}, got {other:?}"),
                },
                _ => panic!("expected User"),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Assistant with a single text block converts cleanly.
    #[test]
    fn assistant_text_block_converts() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "hi there"}],
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::Assistant { id, content } => {
                assert!(id.is_none());
                match content.first() {
                    AssistantContent::Text(t) => assert_eq!(t.text, "hi there"),
                    _ => panic!("expected text"),
                }
            }
            _ => panic!("expected Assistant"),
        }
    }

    /// Assistant with a toolCall block produces a rig `ToolCall`
    /// content.
    #[test]
    fn assistant_tool_call_block_converts() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": "call_1",
                "name": "echo",
                "arguments": {"value": "x"},
            }],
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::Assistant { content, .. } => match content.first() {
                AssistantContent::ToolCall(tc) => {
                    assert_eq!(tc.id, "call_1");
                    assert_eq!(tc.function.name, "echo");
                    assert_eq!(tc.function.arguments["value"], "x");
                }
                _ => panic!("expected ToolCall"),
            },
            _ => panic!("expected Assistant"),
        }
    }

    /// Assistant with a thinking block produces `Reasoning`.
    #[test]
    fn assistant_thinking_block_converts_to_reasoning() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "thinking", "text": "let me think"}],
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::Assistant { content, .. } => match content.first() {
                AssistantContent::Reasoning(_) => {}
                _ => panic!("expected Reasoning"),
            },
            _ => panic!("expected Assistant"),
        }
    }

    /// GH #745 fallout. Cerebras streams reasoning as `delta.reasoning` — the
    /// same non-standard field LocalAI uses. rig 0.39 discarded it, so dirge
    /// never had a thinking block to replay; rig 0.40+ keeps it, and replaying
    /// it as `reasoning_content` makes Cerebras 400 with
    /// `property 'messages.N.assistant.reasoning_content' is unsupported`.
    ///
    /// The block must still be BUILT here — dropping it would lose the model's
    /// reasoning from the next turn's context. The field is renamed at the wire
    /// boundary instead (`CompressingHttpClient::rewrite_provider_quirks`).
    /// `h7_cerebras_tool_dispatch_completes_round_trip` covers the live round
    /// trip; this pins the half that needs no API key.
    #[test]
    fn reasoning_echo_is_preserved_for_every_backend_but_openai() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "text": "let me think"},
                {"type": "text", "text": "the answer"},
            ],
        });
        for provider in ["cerebras", "deepseek", "custom", "ollama", "glm"] {
            let msg = value_to_rig_message_for_provider(&v, Some(provider), None)
                .unwrap_or_else(|| panic!("{provider} must keep the reasoning block"));
            match msg {
                Message::Assistant { content, .. } => {
                    assert!(
                        content
                            .iter()
                            .any(|c| matches!(c, AssistantContent::Reasoning(_))),
                        "{provider} must retain reasoning",
                    );
                    assert!(
                        content
                            .iter()
                            .any(|c| matches!(c, AssistantContent::Text(_))),
                        "{provider} must retain the assistant's text",
                    );
                }
                _ => panic!("expected Assistant"),
            }
        }
    }

    /// GH #821 helper: one assistant message with a signed thinking block
    /// plus a text block, as `serialize_assistant` + the factory's stamp
    /// write it.
    fn signed_thinking_assistant() -> Value {
        serde_json::json!({
            "role": "assistant",
            "content": [
                {
                    "type": "thinking",
                    "text": "let me think",
                    "signature": "sig-821",
                    "signatureModel": "claude-opus-4-6",
                },
                {"type": "text", "text": "the answer"},
            ],
        })
    }

    fn reasoning_signature(msg: &Message) -> Option<Option<String>> {
        match msg {
            Message::Assistant { content, .. } => content.iter().find_map(|c| match c {
                AssistantContent::Reasoning(r) => Some(r.content.iter().find_map(|rc| match rc {
                    rig::completion::message::ReasoningContent::Text { signature, .. } => {
                        signature.clone()
                    }
                    _ => None,
                })),
                _ => None,
            }),
            _ => None,
        }
    }

    /// GH #833: an assistant turn carrying one tool call, and the result it
    /// is paired with. `signature_model` is what the factory stamps at
    /// capture; `None` reproduces a block that was never stamped.
    fn tool_call_history(signature: Option<&str>, signature_model: Option<&str>) -> Vec<Value> {
        let mut call = serde_json::json!({
            "type": "toolCall",
            "id": "call-1",
            "name": "grep",
            "arguments": {"pattern": "x"},
        });
        if let Some(signature) = signature {
            call["signature"] = serde_json::json!(signature);
        }
        if let Some(model) = signature_model {
            call["signatureModel"] = serde_json::json!(model);
        }
        vec![
            serde_json::json!({"role": "user", "content": "find x"}),
            serde_json::json!({"role": "assistant", "content": [call]}),
            serde_json::json!({"role": "toolResult", "toolCallId": "call-1", "content": "found"}),
        ]
    }

    fn tool_call_signature(msg: &Message) -> Option<Option<String>> {
        match msg {
            Message::Assistant { content, .. } => content.iter().find_map(|c| match c {
                AssistantContent::ToolCall(call) => Some(call.signature.clone()),
                _ => None,
            }),
            _ => None,
        }
    }

    fn convert_all(
        history: &[Value],
        provider: Option<&str>,
        replay: ToolCallReplay<'_>,
        dropped: &std::collections::HashSet<String>,
    ) -> Vec<Message> {
        history
            .iter()
            .filter_map(|m| {
                value_to_rig_message_with_replay(
                    m,
                    provider,
                    ThinkingReplay::Unsigned,
                    replay,
                    dropped,
                    None,
                )
            })
            .collect()
    }

    /// GH #833: Gemini requires a `thought_signature` on every `functionCall`
    /// part and rejects a replayed call without one, so the model that minted
    /// it must get it back verbatim. rig sends it straight out again as the
    /// part's `thought_signature`.
    #[test]
    fn gemini_same_model_replay_attaches_the_tool_call_signature() {
        let history = tool_call_history(Some("thought-sig-833"), Some("gemini-3.6-flash"));
        let replay = tool_call_replay_for(Some("gemini"), Some("gemini-3.6-flash"));
        let dropped = unreplayable_tool_calls(&history, replay);
        assert!(dropped.is_empty(), "a same-model signature is replayable");
        let converted = convert_all(&history, Some("gemini"), replay, &dropped);
        assert_eq!(converted.len(), 3, "nothing is dropped");
        assert_eq!(
            tool_call_signature(&converted[1]),
            Some(Some("thought-sig-833".to_string())),
        );
    }

    /// GH #833: with no usable signature the call cannot go out — and it
    /// cannot go alone either, because its paired result would be orphaned,
    /// which every backend rejects. The pair is dropped together: one lost
    /// step of history against a turn that could not be sent at all.
    #[test]
    fn an_unreplayable_gemini_tool_call_takes_its_result_with_it() {
        let cases = [
            (
                tool_call_history(Some("thought-sig-833"), Some("gemini-2.5-flash")),
                "a signature minted by a different model",
            ),
            (
                tool_call_history(Some("thought-sig-833"), None),
                "a signature that was never stamped",
            ),
            (
                tool_call_history(None, None),
                "no signature at all — a session saved before #833",
            ),
        ];
        for (history, why) in cases {
            let replay = tool_call_replay_for(Some("gemini"), Some("gemini-3.6-flash"));
            let dropped = unreplayable_tool_calls(&history, replay);
            assert_eq!(dropped.len(), 1, "{why}");
            let converted = convert_all(&history, Some("gemini"), replay, &dropped);
            assert_eq!(
                converted.len(),
                1,
                "only the user turn survives ({why}); an orphaned result is not an option",
            );
        }
    }

    /// GH #833: every other provider keeps today's unsigned replay,
    /// byte-identical on the wire — and never drops anything.
    #[test]
    fn non_gemini_tool_calls_replay_unsigned_and_are_never_dropped() {
        let history = tool_call_history(None, None);
        for provider in [Some("anthropic"), Some("openai"), Some("glm"), None] {
            let replay = tool_call_replay_for(provider, Some("some-model"));
            let dropped = unreplayable_tool_calls(&history, replay);
            assert!(dropped.is_empty(), "{provider:?}");
            let converted = convert_all(&history, provider, replay, &dropped);
            assert_eq!(converted.len(), 3, "{provider:?}");
            assert_eq!(
                tool_call_signature(&converted[1]),
                Some(None),
                "{provider:?}"
            );
        }
    }

    /// GH #821: replaying a signed thinking block to Anthropic with the
    /// SAME model that minted the signature (reasoning on) must attach
    /// the signature verbatim — Anthropic schema-rejects the block
    /// without it (`thinking.signature: Field required`).
    #[test]
    fn anthropic_same_model_replay_attaches_the_signature() {
        let v = signed_thinking_assistant();
        let replay = thinking_replay_for(Some("anthropic"), Some("claude-opus-4-6"), true);
        let msg = value_to_rig_message_with_replay(
            &v,
            Some("anthropic"),
            replay,
            ToolCallReplay::Unsigned,
            &Default::default(),
            None,
        )
        .expect("must convert");
        assert_eq!(
            reasoning_signature(&msg),
            Some(Some("sig-821".to_string())),
            "the stored signature must ride the replayed Reasoning block"
        );
    }

    /// GH #821: a signature is only valid for the model that minted it —
    /// replaying it to a DIFFERENT model is rejected (`Invalid signature
    /// in thinking block`) — and an unsigned block is schema-rejected, so
    /// on a model mismatch (or a missing/unstamped signature, or reasoning
    /// off) the whole thinking block must be dropped from the Anthropic
    /// request. The rest of the message survives.
    #[test]
    fn anthropic_replay_drops_unattachable_thinking_blocks() {
        let signed = signed_thinking_assistant();
        let unsigned = serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "text": "let me think"},
                {"type": "text", "text": "the answer"},
            ],
        });
        let cases: Vec<(&Value, ThinkingReplay<'_>, &str)> = vec![
            (
                &signed,
                thinking_replay_for(Some("anthropic"), Some("claude-fable-5"), true),
                "foreign-model signature",
            ),
            (
                &signed,
                thinking_replay_for(Some("anthropic"), Some("claude-opus-4-6"), false),
                "reasoning off this turn",
            ),
            (
                &signed,
                thinking_replay_for(Some("anthropic"), None, true),
                "request model unknown",
            ),
            (
                &unsigned,
                thinking_replay_for(Some("anthropic"), Some("claude-opus-4-6"), true),
                "legacy/flattened block with no signature",
            ),
        ];
        for (v, replay, case) in cases {
            let msg = value_to_rig_message_with_replay(
                v,
                Some("anthropic"),
                replay,
                ToolCallReplay::Unsigned,
                &Default::default(),
                None,
            )
            .unwrap_or_else(|| panic!("{case}: message with text must survive"));
            match msg {
                Message::Assistant { content, .. } => {
                    assert!(
                        !content
                            .iter()
                            .any(|c| matches!(c, AssistantContent::Reasoning(_))),
                        "{case}: the thinking block must be dropped",
                    );
                    assert!(
                        content
                            .iter()
                            .any(|c| matches!(c, AssistantContent::Text(_))),
                        "{case}: the text block must survive",
                    );
                }
                _ => panic!("expected Assistant"),
            }
        }
    }

    /// GH #821 non-regression: every non-Anthropic backend keeps the
    /// pre-#821 unsigned echo, byte-identical — a stored signature must
    /// neither attach nor drop the block there.
    #[test]
    fn non_anthropic_replay_of_a_signed_block_is_unchanged() {
        let v = signed_thinking_assistant();
        for provider in [Some("cerebras"), Some("deepseek"), Some("custom"), None] {
            let replay = thinking_replay_for(provider, Some("claude-opus-4-6"), true);
            assert!(matches!(replay, ThinkingReplay::Unsigned));
            let msg = value_to_rig_message_with_replay(
                &v,
                provider,
                replay,
                ToolCallReplay::Unsigned,
                &Default::default(),
                None,
            )
            .unwrap_or_else(|| panic!("{provider:?} must keep the message"));
            assert_eq!(
                reasoning_signature(&msg),
                Some(None),
                "{provider:?}: reasoning must replay unsigned, exactly as before",
            );
        }
    }

    /// GH #821: the factory stamps the model that minted each captured
    /// signature onto the block (the capture layer doesn't know the
    /// model). Only signed, not-yet-stamped blocks are touched.
    #[test]
    fn stamp_records_the_minting_model_on_signed_blocks_only() {
        use super::super::message::{AssistantMessage, StopReason};
        let message = AssistantMessage::new(
            vec![
                ContentBlock::Thinking {
                    text: "signed".to_string(),
                    signature: Some("sig-a".to_string()),
                    signature_model: None,
                },
                ContentBlock::Thinking {
                    text: "unsigned".to_string(),
                    signature: None,
                    signature_model: None,
                },
                ContentBlock::Thinking {
                    text: "already stamped".to_string(),
                    signature: Some("sig-b".to_string()),
                    signature_model: Some("other-model".to_string()),
                },
                ContentBlock::Text {
                    text: "hi".to_string(),
                },
            ],
            StopReason::Stop,
        );
        let evt = StreamEvent::Done {
            reason: StopReason::Stop,
            message,
            usage: None,
        };
        let stamped = stamp_signature_models(evt, Some("claude-opus-4-6"));
        let StreamEvent::Done { message, .. } = stamped else {
            panic!("expected Done back");
        };
        let models: Vec<Option<&str>> = message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Thinking {
                    signature_model, ..
                } => Some(signature_model.as_deref()),
                _ => None,
            })
            .collect();
        assert_eq!(
            models,
            vec![Some("claude-opus-4-6"), None, Some("other-model")],
            "stamp signed+unstamped; leave unsigned and already-stamped alone"
        );
    }

    /// dirge-byun. A turn where the model emitted only reasoning (or nothing
    /// at all) replays as an assistant message whose sole text block is `""`.
    /// Every OpenAI-compatible backend serializes that as
    /// `content: [{"type": "text", "text": ""}]` and rejects it — Moonshot/Kimi
    /// and GLM with `400 invalid_request_error: text content is empty`,
    /// Anthropic with its own 400. Since the offending message is replayed from
    /// session history on EVERY subsequent prompt, the session is wedged: no
    /// further turn can start. Drop the empty block here (mirroring the
    /// user-side guard in `build_user_content`); a message left with nothing
    /// usable drops out entirely.
    #[test]
    fn assistant_empty_text_block_is_dropped() {
        let only_empty = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": ""}],
        });
        for provider in [None, Some("openai"), Some("moonshot"), Some("glm")] {
            assert!(
                value_to_rig_message_for_provider(&only_empty, provider, None).is_none(),
                "{provider:?}: an assistant message with only an empty text block \
                 must not reach the provider",
            );
        }

        // The empty block must not survive alongside real content either — the
        // provider rejects the whole request over one empty part.
        let mixed = serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "text": "let me think"},
                {"type": "text", "text": ""},
            ],
        });
        let msg = value_to_rig_message_for_provider(&mixed, Some("moonshot"), None)
            .expect("the reasoning block keeps the message alive");
        match msg {
            Message::Assistant { content, .. } => {
                assert!(
                    !content
                        .iter()
                        .any(|c| matches!(c, AssistantContent::Text(t) if t.text.is_empty())),
                    "empty text block must be dropped",
                );
                assert!(
                    content
                        .iter()
                        .any(|c| matches!(c, AssistantContent::Reasoning(_))),
                    "reasoning must survive",
                );
            }
            _ => panic!("expected Assistant"),
        }

        // A tool-call turn whose text was empty keeps the call — dropping the
        // message would orphan the tool result that follows it.
        let with_call = serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": ""},
                {"type": "toolCall", "id": "call_1", "name": "echo", "arguments": {}},
            ],
        });
        let msg = value_to_rig_message_for_provider(&with_call, Some("moonshot"), None)
            .expect("the tool call keeps the message alive");
        match msg {
            Message::Assistant { content, .. } => {
                let parts: Vec<_> = content.into_iter().collect();
                assert_eq!(parts.len(), 1, "only the tool call survives");
                assert!(matches!(parts[0], AssistantContent::ToolCall(_)));
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn openai_assistant_thinking_only_is_skipped() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "thinking", "text": "let me think"}],
        });

        assert!(
            value_to_rig_message_for_provider(&v, Some("openai"), None).is_none(),
            "OpenAI Responses rejects historical reasoning without provider-generated IDs"
        );
    }

    #[test]
    fn openai_assistant_history_drops_thinking_but_keeps_tool_calls() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "text": "private reasoning"},
                {"type": "text", "text": "visible answer"},
                {
                    "type": "toolCall",
                    "id": "call_1",
                    "name": "echo",
                    "arguments": {"value": "x"}
                }
            ],
        });

        let msg =
            value_to_rig_message_for_provider(&v, Some("openai"), None).expect("must convert");
        match msg {
            Message::Assistant { content, .. } => {
                let parts: Vec<_> = content.into_iter().collect();
                assert_eq!(parts.len(), 2);
                match &parts[0] {
                    AssistantContent::Text(t) => assert_eq!(t.text, "visible answer"),
                    other => panic!("expected text, got {other:?}"),
                }
                match &parts[1] {
                    AssistantContent::ToolCall(tc) => {
                        assert_eq!(tc.id, "call_1");
                        assert_eq!(tc.call_id.as_deref(), Some("call_1"));
                        assert_eq!(tc.function.name, "echo");
                        assert_eq!(tc.function.arguments["value"], "x");
                    }
                    other => panic!("expected tool call, got {other:?}"),
                }
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn openai_tool_result_history_uses_tool_call_id_as_responses_call_id() {
        let v = serde_json::json!({
            "role": "toolResult",
            "toolCallId": "call_1",
            "toolName": "echo",
            "content": [{"type": "text", "text": "line 1"}],
            "details": {},
            "isError": true,
        });

        let msg =
            value_to_rig_message_for_provider(&v, Some("openai"), None).expect("must convert");
        match msg {
            Message::User { content } => match content.first() {
                UserContent::ToolResult(tr) => {
                    assert_eq!(tr.id, "call_1");
                    assert_eq!(tr.call_id.as_deref(), Some("call_1"));
                }
                other => panic!("expected ToolResult, got {other:?}"),
            },
            other => panic!("expected User, got {other:?}"),
        }
    }

    /// dirge-vcu1: a `system`-role message (compaction fold summary,
    /// mid-session memory reinject) must survive into the outgoing
    /// request, not be dropped. It maps to a `user` message so it stays
    /// in the message history at its position (keeping the cache-warm
    /// system prefix stable) and reaches every provider uniformly —
    /// rig's Anthropic provider would otherwise hoist a `Message::System`
    /// into the top-level system field, busting the prompt cache.
    #[test]
    fn system_role_summary_reaches_the_request() {
        // Mirrors the compaction fold's `role: "system"` summary message.
        let summary = "[CONTEXT COMPACTION — REFERENCE ONLY] …\n## Active Task\nfinish the port";
        let v = serde_json::json!({
            "role": "system",
            "content": summary,
        });
        for provider in [None, Some("anthropic"), Some("openai")] {
            let msg = value_to_rig_message_for_provider(&v, provider, None)
                .unwrap_or_else(|| panic!("system message dropped for provider {provider:?}"));
            match msg {
                Message::User { content } => match content.first() {
                    UserContent::Text(t) => assert!(
                        t.text.contains("Active Task"),
                        "summary body must be preserved"
                    ),
                    other => panic!("expected text user content, got {other:?}"),
                },
                other => panic!("expected User message, got {other:?}"),
            }
        }
    }

    /// ToolResult value → rig's tool_result user-content message.
    /// Content blocks are flattened to a single text body.
    #[test]
    fn tool_result_value_converts() {
        let v = serde_json::json!({
            "role": "toolResult",
            "toolCallId": "call_1",
            "toolName": "echo",
            "content": [
                {"type": "text", "text": "line 1"},
                {"type": "text", "text": "line 2"},
            ],
            "details": {},
            "isError": false,
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::User { content } => match content.first() {
                UserContent::ToolResult(tr) => {
                    assert_eq!(tr.id, "call_1");
                }
                _ => panic!("expected ToolResult"),
            },
            _ => panic!("expected User"),
        }
    }

    /// dirge-byun, second instance of the same family. A silent successful
    /// command (`mkdir -p x`, `touch y`) makes bash return `""`, so the tool
    /// result carries an empty text block — 26 of them across the author's
    /// recent sessions. That serializes as an empty text content block, which
    /// Moonshot/Kimi and GLM reject with `400 invalid_request_error: text
    /// content is empty`, and the result is replayed from history forever
    /// after. The result cannot simply be dropped — Anthropic and OpenAI both
    /// reject an orphan tool call — so it gets a placeholder body instead.
    /// opencode does the same thing (`tool/shell.ts:576`).
    #[test]
    fn empty_tool_result_gets_a_placeholder_body() {
        let shapes = [
            serde_json::json!({
                "role": "toolResult",
                "toolCallId": "call_1",
                "toolName": "bash",
                "content": [{"type": "text", "text": ""}],
                "details": {},
                "isError": false,
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "call_1",
                "content": "",
            }),
            // No content key at all — `unwrap_or_default` used to make this ""
            // too.
            serde_json::json!({
                "role": "toolResult",
                "toolCallId": "call_1",
            }),
        ];
        for v in &shapes {
            let msg = value_to_rig_message_for_provider(v, Some("moonshot"), None)
                .unwrap_or_else(|| panic!("tool result must not be dropped: {v}"));
            match msg {
                Message::User { content } => match content.first() {
                    UserContent::ToolResult(tr) => {
                        assert_eq!(tr.id, "call_1");
                        let body = match tr.content.first() {
                            rig::completion::message::ToolResultContent::Text(t) => t.text.clone(),
                            other => panic!("expected text tool-result content, got {other:?}"),
                        };
                        assert!(
                            !body.is_empty(),
                            "empty tool-result body must be replaced: {v}",
                        );
                    }
                    other => panic!("expected ToolResult, got {other:?}"),
                },
                other => panic!("expected User, got {other:?}"),
            }
        }
    }

    /// A `user` / `system` message whose content is an empty string must not
    /// reach the provider either — same empty text content block, same 400.
    /// The array-of-parts shape is already covered by
    /// `converter_drops_empty_text_part`; this pins the plain-string arms.
    #[test]
    fn empty_string_user_and_system_messages_are_dropped() {
        for role in ["user", "system"] {
            let v = serde_json::json!({"role": role, "content": ""});
            assert!(
                value_to_rig_message_for_provider(&v, Some("moonshot"), None).is_none(),
                "an empty {role} message must not reach the provider",
            );
        }
    }

    /// dirge-byun end to end, over the chain that actually broke: a wedged
    /// session replays through `convert_history` →
    /// `rig_history_to_loop_messages` → `loop_message_to_value` → here on
    /// every single prompt (`ui/run_handlers/submit.rs` rebuilds it each
    /// time). Nothing that reaches the provider may carry an empty text
    /// content block: Moonshot/Kimi and GLM answer
    /// `400 invalid_request_error: text content is empty` and the session can
    /// never take another turn.
    ///
    /// The session below is the reported shape — a turn where the model
    /// emitted only reasoning, plus a silent `mkdir` whose tool result came
    /// back empty.
    #[test]
    fn wedged_session_replays_without_empty_text_blocks() {
        use crate::session::{MessageRole, Session, ToolCallEntry, ToolCallState};

        let mut s = Session::new("moonshot", "kimi", 0);
        s.add_message(MessageRole::User, "proceed with implementation");
        s.add_message_with_tool_calls(
            MessageRole::Assistant,
            "",
            vec![ToolCallEntry {
                id: "tc_1".to_string(),
                name: "bash".to_string(),
                args: serde_json::json!({"command": "mkdir -p src/gen"}),
                state: ToolCallState::Completed {
                    result: String::new(),
                },
            }],
        );
        // The reasoning-only turn: no prose, no tool calls.
        s.add_message(MessageRole::Assistant, "");
        s.add_message(MessageRole::User, "huh?");

        let history = crate::agent::runner::convert_history(&s);
        let loop_msgs =
            crate::agent::agent_loop::integration::rig_history_to_loop_messages(history);
        let outgoing: Vec<Message> = loop_msgs
            .iter()
            .map(crate::agent::agent_loop::message::loop_message_to_value)
            .filter_map(|v| value_to_rig_message_for_provider(&v, Some("moonshot"), None))
            .collect();

        for m in &outgoing {
            match m {
                Message::User { content } => {
                    for part in content.iter() {
                        match part {
                            UserContent::Text(t) => {
                                assert!(!t.text.is_empty(), "empty user text block: {m:?}")
                            }
                            UserContent::ToolResult(tr) => {
                                for c in tr.content.iter() {
                                    if let rig::completion::message::ToolResultContent::Text(t) = c
                                    {
                                        assert!(
                                            !t.text.is_empty(),
                                            "empty tool-result text block: {m:?}",
                                        );
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Message::Assistant { content, .. } => {
                    for part in content.iter() {
                        if let AssistantContent::Text(t) = part {
                            assert!(!t.text.is_empty(), "empty assistant text block: {m:?}");
                        }
                    }
                }
                Message::System { .. } => {}
            }
        }

        // The tool call must still be paired with its result — dropping the
        // empty body instead of replacing it would orphan the call.
        let calls = outgoing
            .iter()
            .filter(|m| match m {
                Message::Assistant { content, .. } => content
                    .iter()
                    .any(|c| matches!(c, AssistantContent::ToolCall(_))),
                _ => false,
            })
            .count();
        let results = outgoing
            .iter()
            .filter(|m| match m {
                Message::User { content } => content
                    .iter()
                    .any(|c| matches!(c, UserContent::ToolResult(_))),
                _ => false,
            })
            .count();
        assert_eq!(calls, 1, "the tool call survives: {outgoing:#?}");
        assert_eq!(results, 1, "so does its result: {outgoing:#?}");
    }

    /// Tool role (snake_case) with tool_call_id → rig ToolResult.
    /// Dual convention: loop uses `toolResult`/`toolCallId`; legacy
    /// session data uses `tool`/`tool_call_id`. Both must convert.
    #[test]
    fn tool_role_snake_case_converts() {
        let v = serde_json::json!({
            "role": "tool",
            "tool_call_id": "call_abc",
            "content": "tool output text",
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::User { content } => match content.first() {
                UserContent::ToolResult(tr) => {
                    assert_eq!(tr.id, "call_abc");
                }
                other => panic!("expected ToolResult, got {other:?}"),
            },
            other => panic!("expected User, got {other:?}"),
        }
    }

    /// Custom / unknown role → skipped (None).
    #[test]
    fn custom_role_returns_none() {
        let v = serde_json::json!({"role": "custom", "content": "x"});
        assert!(value_to_rig_message(&v).is_none());
    }

    /// Missing role field → None.
    #[test]
    fn missing_role_returns_none() {
        let v = serde_json::json!({"content": "x"});
        assert!(value_to_rig_message(&v).is_none());
    }

    /// `loop_tool_to_rig_definition` copies name + description +
    /// parameters; label is intentionally dropped (rig has no
    /// slot).
    #[test]
    fn loop_tool_definition_strips_label() {
        // A minimal LoopTool stub for the conversion test.
        #[derive(Debug)]
        struct Stub;
        impl LoopTool for Stub {
            fn name(&self) -> &str {
                "stub"
            }
            fn description(&self) -> &str {
                "stub description"
            }
            fn label(&self) -> &str {
                "Stub Label"
            }
            fn parameters(&self) -> &Value {
                static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
                P.get_or_init(|| serde_json::json!({"type": "object"}))
            }
            fn execute<'a>(
                &'a self,
                _id: &'a str,
                _args: Value,
                _signal: AbortSignal,
                _on_update: super::super::tool::LoopToolUpdate,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<super::super::result::LoopToolResult, String>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async { unreachable!("not called in conversion test") })
            }
        }

        let def = loop_tool_to_rig_definition(&Stub);
        assert_eq!(def.name, "stub");
        assert_eq!(def.description, "stub description");
        assert_eq!(def.parameters["type"], "object");
    }

    /// Compile-time: `rig_stream_fn_from_model` produces a
    /// `Send + Sync + 'static` StreamFn. This is the bound the
    /// loop demands; if it doesn't compile, no use of the
    /// factory is going to work.
    #[test]
    fn stream_fn_is_send_sync_static() {
        // Use rig's built-in test model (mock_provider) if
        // available; otherwise this test just verifies the type
        // constraints at compile time via assertion shape.
        // We can't easily build a real model in a unit test
        // because every rig provider needs an API key. Instead
        // we assert the trait bound via a turbofish on a generic
        // function — succeeds compile-time if the signature is
        // correct.

        fn assert_constraints<M>(_model: M)
        where
            M: CompletionModel + Clone + Send + Sync + 'static,
            M::StreamingResponse: Clone + Unpin + Send + Sync + GetTokenUsage + 'static,
        {
            // No-op; existence of the function is the proof.
        }

        // We can't instantiate M without a real provider; the
        // compile-time check on the function signature is what
        // matters. This test "passes" by virtue of compiling.
        let _: fn(_) = assert_constraints::<NopModel>;
    }

    /// Minimal stub CompletionModel so we can verify the
    /// factory produces a working `StreamFn` end-to-end. The
    /// stub returns a canned `done` event with empty text via
    /// `model.stream(request)`.
    #[derive(Clone)]
    struct NopModel;

    impl GetTokenUsage for NopStreamResponse {
        // rig 0.39 changed the trait return type from Option<Usage> to
        // Usage. All-zeros is the "provider didn't report" sentinel per
        // rig's own docs — functionally unchanged from the pre-0.39 None.
        fn token_usage(&self) -> rig::completion::Usage {
            rig::completion::Usage::default()
        }
    }

    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
    struct NopStreamResponse;

    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
    struct NopResponse;

    impl CompletionModel for NopModel {
        type Response = NopResponse;
        type StreamingResponse = NopStreamResponse;
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            NopModel
        }

        async fn completion(
            &self,
            _request: rig::completion::CompletionRequest,
        ) -> Result<rig::completion::CompletionResponse<Self::Response>, CompletionError> {
            // Not used by the streaming factory.
            unreachable!("completion() not used in stream factory tests")
        }

        async fn stream(
            &self,
            _request: rig::completion::CompletionRequest,
        ) -> Result<
            rig::streaming::StreamingCompletionResponse<Self::StreamingResponse>,
            CompletionError,
        > {
            // Empty inner stream — the wrap_rig_stream layer
            // synthesizes a `Done { reason: Stop, message: empty }`
            // for an empty stream, which is what we want for
            // the smoke test.
            let inner: rig::streaming::StreamingResult<Self::StreamingResponse> =
                Box::pin(futures::stream::empty());
            Ok(rig::streaming::StreamingCompletionResponse::stream(inner))
        }
    }

    /// End-to-end smoke test: build the factory from `NopModel`,
    /// invoke once, drain the resulting stream. Expect Start +
    /// Done (no Error). Proves the conversion + builder + wrap
    /// chain composes correctly.
    #[tokio::test]
    async fn factory_invocation_produces_start_and_done() {
        use futures::stream::StreamExt;
        let factory = rig_stream_fn_from_model::<NopModel>(NopModel, vec![], None);
        let ctx = LlmContext {
            system_prompt: "test preamble".to_string(),
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
            asset_dir: None,
        };
        let mut stream = factory(
            ctx,
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        );
        let mut kinds = Vec::new();
        while let Some(evt) = stream.next().await {
            kinds.push(match &evt {
                StreamEvent::Start { .. } => "start",
                StreamEvent::Delta { .. } => "delta",
                StreamEvent::Done { .. } => "done",
                StreamEvent::Error { error } => {
                    panic!("unexpected error: {error}");
                }
                StreamEvent::Retry { .. } => {
                    panic!("unexpected retry event in non-retried stream");
                }
            });
        }
        // Expect at minimum Start + Done. No Error.
        assert!(kinds.contains(&"start"));
        assert!(kinds.contains(&"done"));
    }

    /// Empty message list → factory emits an Error event (not a
    /// panic). Defensive — caller misconfiguration is loud.
    #[tokio::test]
    async fn factory_empty_messages_emits_error() {
        use futures::stream::StreamExt;
        let factory = rig_stream_fn_from_model::<NopModel>(NopModel, vec![], None);
        let ctx = LlmContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            asset_dir: None,
        };
        let mut stream = factory(
            ctx,
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        );
        let mut found_error = false;
        while let Some(evt) = stream.next().await {
            if matches!(evt, StreamEvent::Error { .. }) {
                found_error = true;
            }
        }
        assert!(found_error, "empty messages must produce an Error event");
    }

    // ============================================================
    // Per-provider reasoning mapper tests
    // ============================================================

    use crate::agent::agent_loop::stream::StreamOptions;
    use crate::agent::agent_loop::tool::AbortSignal;
    use crate::agent::agent_loop::types::{ThinkingBudgets, ThinkingLevel};

    fn opts_with_reasoning(level: ThinkingLevel) -> StreamOptions {
        let mut o = StreamOptions::from_signal(AbortSignal::new());
        o.reasoning = Some(level);
        o
    }

    // ============================================================
    // request_max_tokens (GH #816)
    // ============================================================

    /// GH #816: with reasoning off, an Anthropic request must still carry
    /// the resolved `max_tokens` — rig 0.41 hard-errors ("`max_tokens` must
    /// be set for Anthropic") before the HTTP call when the field is unset
    /// and the model id is one it has no default for (every Claude 5 id).
    #[test]
    fn non_reasoning_anthropic_request_carries_configured_max_tokens() {
        let mut o = StreamOptions::from_signal(AbortSignal::new());
        o.max_tokens = Some(8192);
        assert_eq!(request_max_tokens(Some("anthropic"), None, &o), Some(8192));
    }

    /// `Some(Off)` puts no thinking params on the wire, so it is a
    /// non-reasoning turn and gets the configured value too.
    #[test]
    fn reasoning_off_level_anthropic_request_carries_configured_max_tokens() {
        let mut o = opts_with_reasoning(ThinkingLevel::Off);
        o.max_tokens = Some(8192);
        assert_eq!(request_max_tokens(Some("anthropic"), None, &o), Some(8192));
    }

    /// A reasoning turn keeps the budget ceiling, NOT the configured value:
    /// Anthropic requires `budget_tokens` strictly below `max_tokens`, so a
    /// configured 8192 must never undercut a level's budget + headroom
    /// (the invariant `anthropic_ceiling_clears_every_budget` pins).
    #[test]
    fn reasoning_anthropic_request_keeps_ceiling_over_configured_value() {
        let mut o = opts_with_reasoning(ThinkingLevel::High);
        o.max_tokens = Some(8192);
        let expected = crate::provider::adapter::max_tokens_for_reasoning(
            Some("anthropic"),
            None,
            ThinkingLevel::High,
            None,
        )
        .expect("high puts a budget on the wire");
        assert_eq!(
            request_max_tokens(Some("anthropic"), None, &o),
            Some(expected)
        );
        assert!(
            expected > 8192,
            "the ceiling must clear the configured value for this test to bite",
        );
    }

    /// No resolved config (tests, paths built without one) leaves the
    /// request unset — byte-identical to the pre-fix behaviour.
    #[test]
    fn non_reasoning_anthropic_request_without_config_stays_unset() {
        let o = StreamOptions::from_signal(AbortSignal::new());
        assert_eq!(request_max_tokens(Some("anthropic"), None, &o), None);
    }

    /// Effort-string providers never had a cap forced on them and still
    /// don't — their backends accept an absent `max_tokens`, and capping an
    /// OpenAI reasoning turn could strangle output (reasoning tokens count
    /// against the output limit there).
    #[test]
    fn other_providers_stay_uncapped_with_and_without_reasoning() {
        for provider in [
            "openai",
            "deepseek",
            "glm",
            "cerebras",
            "custom",
            "openrouter",
        ] {
            let mut off = StreamOptions::from_signal(AbortSignal::new());
            off.max_tokens = Some(8192);
            assert_eq!(
                request_max_tokens(Some(provider), None, &off),
                None,
                "{provider}: non-reasoning turn must stay uncapped",
            );
            let mut on = opts_with_reasoning(ThinkingLevel::High);
            on.max_tokens = Some(8192);
            assert_eq!(
                request_max_tokens(Some(provider), None, &on),
                None,
                "{provider}: reasoning turn must stay uncapped",
            );
        }
    }

    /// Anthropic gets `thinking: { type: "enabled", budget_tokens
    /// }`. Verifies the budget defaults are sane for each level.
    #[test]
    fn anthropic_reasoning_maps_to_thinking_budget() {
        let opts = opts_with_reasoning(ThinkingLevel::Medium);
        let v = build_provider_additional_params(Some("anthropic"), None, &opts).unwrap();
        assert_eq!(v["thinking"]["type"], "enabled");
        assert_eq!(v["thinking"]["budget_tokens"], 4096);
    }

    /// Off level → no thinking key at all (Anthropic).
    #[test]
    fn anthropic_off_omits_thinking_key() {
        let opts = opts_with_reasoning(ThinkingLevel::Off);
        let v = build_provider_additional_params(Some("anthropic"), None, &opts);
        assert!(v.is_none(), "Off should produce empty additional_params");
    }

    /// Caller-supplied budgets override the defaults.
    #[test]
    fn anthropic_respects_caller_budget_override() {
        let mut opts = opts_with_reasoning(ThinkingLevel::High);
        opts.thinking_budgets = Some(ThinkingBudgets {
            high: Some(32_000),
            ..Default::default()
        });
        let v = build_provider_additional_params(Some("anthropic"), None, &opts).unwrap();
        assert_eq!(v["thinking"]["budget_tokens"], 32_000);
    }

    /// OpenAI Responses (and openai-compat: deepseek/glm/custom)
    /// get `reasoning: { effort: low|medium|high }`.
    #[test]
    fn openai_reasoning_maps_to_effort() {
        for (level, expected) in [
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
            (ThinkingLevel::Xhigh, "xhigh"),
            (ThinkingLevel::Max, "max"),
        ] {
            let opts = opts_with_reasoning(level);
            let v = build_provider_additional_params(Some("openai"), None, &opts).unwrap();
            assert_eq!(
                v["reasoning"]["effort"], expected,
                "level {level:?} should map to {expected}"
            );
        }
    }

    /// DeepSeek gets top-level `reasoning_effort` string (not
    /// nested inside `reasoning`). DeepSeek has no "xhigh" tier, so
    /// Xhigh and Max both fold up to "max".
    #[test]
    fn deepseek_reasoning_maps_to_top_level_effort() {
        for (level, expected) in [
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
            (ThinkingLevel::Xhigh, "max"),
            (ThinkingLevel::Max, "max"),
        ] {
            let opts = opts_with_reasoning(level);
            let v = build_provider_additional_params(Some("deepseek"), None, &opts).unwrap();
            assert_eq!(
                v["reasoning_effort"], expected,
                "deepseek level {level:?} should map to top-level reasoning_effort={expected}"
            );
            assert!(
                v.get("reasoning").is_none(),
                "deepseek must not have nested reasoning key for level {level:?}"
            );
        }
    }

    #[test]
    fn cerebras_reasoning_maps_to_supported_top_level_effort() {
        for (level, expected) in [
            (ThinkingLevel::Minimal, "low"),
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
            // No xhigh/max tier on Cerebras — both clamp to "high".
            (ThinkingLevel::Xhigh, "high"),
            (ThinkingLevel::Max, "high"),
        ] {
            let opts = opts_with_reasoning(level);
            let params = build_provider_additional_params(Some("cerebras"), None, &opts)
                .expect("Cerebras reasoning should produce request params");

            assert_eq!(
                params,
                serde_json::json!({ "reasoning_effort": expected }),
                "unexpected Cerebras request params for {level:?}",
            );
            assert_ne!(params["reasoning_effort"], "max");
            assert!(params.get("reasoning_level").is_none());
            assert!(params.get("reasoning").is_none());
        }
    }

    #[test]
    fn cerebras_off_omits_all_reasoning_fields() {
        let opts = opts_with_reasoning(ThinkingLevel::Off);
        assert_eq!(
            build_provider_additional_params(Some("cerebras"), None, &opts),
            None,
        );
    }

    /// Custom, OpenRouter share OpenAI's nested effort-based reasoning
    /// shape (deepseek is separate; GLM uses top-level — see below).
    #[test]
    fn openai_compat_providers_share_effort_shape() {
        let opts = opts_with_reasoning(ThinkingLevel::Medium);
        for provider in ["custom", "openrouter"] {
            let v = build_provider_additional_params(Some(provider), None, &opts).unwrap();
            assert_eq!(
                v["reasoning"]["effort"], "medium",
                "provider {provider} should use effort=medium"
            );
        }
    }

    /// GLM (z.ai) uses top-level `reasoning_effort` (not OpenAI's nested
    /// form) and, on GLM-5.3, accepts only `low`/`high`/`max` — so
    /// `Medium` collapses to `"high"` (mirroring z.ai's own GLM-5.2
    /// folding). There is no `xhigh` tier, so `Xhigh` and `Max` both
    /// round up to `"max"`. See `glm_effort_top_level_with_max_and_collapse`
    /// for the full mapping.
    #[test]
    fn glm_uses_top_level_reasoning_effort_not_nested() {
        let opts = opts_with_reasoning(ThinkingLevel::Medium);
        let v = build_provider_additional_params(Some("glm"), None, &opts).unwrap();
        assert_eq!(
            v["reasoning_effort"], "high",
            "glm Medium should collapse to high"
        );
        assert!(
            v.get("reasoning").is_none(),
            "glm must not use the nested form"
        );

        // Xhigh and Max both fold up to "max" on the 3-tier GLM wire.
        for level in [ThinkingLevel::Xhigh, ThinkingLevel::Max] {
            let opts = opts_with_reasoning(level);
            let v = build_provider_additional_params(Some("glm"), None, &opts).unwrap();
            assert_eq!(
                v["reasoning_effort"], "max",
                "glm {level:?} should map to max"
            );
        }
    }

    /// Minimal clamps to "low" (dirge's Minimal and Low share the `low`
    /// wire value). Xhigh and Max are NOT clamped on OpenAI — OpenAI's
    /// `ReasoningEffort` Literal exposes both as distinct tiers, the
    /// whole point of the Xhigh/Max split. The 3-tier providers that lack
    /// `xhigh` (DeepSeek, GLM) fold Xhigh up to "max" in their own
    /// mapping functions, not here.
    #[test]
    fn openai_clamps_unsupported_levels() {
        let opts_min = opts_with_reasoning(ThinkingLevel::Minimal);
        let v = build_provider_additional_params(Some("openai"), None, &opts_min).unwrap();
        assert_eq!(v["reasoning"]["effort"], "low");

        let opts_x = opts_with_reasoning(ThinkingLevel::Xhigh);
        let v = build_provider_additional_params(Some("openai"), None, &opts_x).unwrap();
        assert_eq!(v["reasoning"]["effort"], "xhigh");

        let opts_max = opts_with_reasoning(ThinkingLevel::Max);
        let v = build_provider_additional_params(Some("openai"), None, &opts_max).unwrap();
        assert_eq!(v["reasoning"]["effort"], "max");
    }

    /// OpenAI Off → omits the reasoning key entirely.
    #[test]
    fn openai_off_omits_reasoning_key() {
        let opts = opts_with_reasoning(ThinkingLevel::Off);
        let v = build_provider_additional_params(Some("openai"), None, &opts);
        assert!(v.is_none());
    }

    /// DeepSeek Off → no `reasoning_effort` key. GH #827: it now sends the
    /// disable toggle instead of nothing at all — DeepSeek has carried that
    /// shape all along for tool-less one-shots, and omitting the effort string
    /// only means "provider default", which is not off.
    #[test]
    fn deepseek_off_disables_rather_than_omitting_everything() {
        let opts = opts_with_reasoning(ThinkingLevel::Off);
        let v = build_provider_additional_params(Some("deepseek"), None, &opts).unwrap();
        assert!(v.get("reasoning_effort").is_none());
        assert_eq!(v["thinking"]["type"], "disabled");
    }

    /// OpenAI has no safe disable knob, so `off` still sends nothing — the
    /// routing added in #827 only reaches providers that have one.
    #[test]
    fn openai_off_still_sends_nothing() {
        let opts = opts_with_reasoning(ThinkingLevel::Off);
        assert!(build_provider_additional_params(Some("openai"), None, &opts).is_none());
    }

    /// OpenAI with High reasoning still returns the nested
    /// `{"reasoning":{"effort":"high"}}` shape (unchanged).
    #[test]
    fn openai_high_still_uses_nested_reasoning_effort() {
        let opts = opts_with_reasoning(ThinkingLevel::High);
        let v = build_provider_additional_params(Some("openai"), None, &opts).unwrap();
        assert_eq!(v["reasoning"]["effort"], "high");
        assert!(v.get("reasoning_effort").is_none());
    }

    /// GH #832: Gemini's thinking config must sit inside `generationConfig`,
    /// the only key rig's Gemini provider claims. A top-level `thinking_config`
    /// was flattened into the request body and rejected with `Unknown name
    /// "thinking_config"`. An unclassifiable id keeps the 2.5 budget wire.
    #[test]
    fn gemini_reasoning_nests_under_generation_config() {
        let opts = opts_with_reasoning(ThinkingLevel::High);
        let v = build_provider_additional_params(Some("gemini"), None, &opts).unwrap();
        assert_eq!(
            v["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            16384
        );
        assert!(
            v.get("thinking_config").is_none(),
            "the rejected shape: {v}"
        );
    }

    /// GH #832: and a Gemini 3 id takes the depth knob instead — the two are
    /// mutually exclusive on the wire.
    #[test]
    fn gemini_3_reasoning_sends_a_thinking_level() {
        let opts = opts_with_reasoning(ThinkingLevel::High);
        let v = build_provider_additional_params(Some("gemini"), Some("gemini-3.6-flash"), &opts)
            .unwrap();
        assert_eq!(
            v["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
        assert!(
            v["generationConfig"]["thinkingConfig"]
                .get("thinkingBudget")
                .is_none(),
            "budget and level must never ship together: {v}",
        );
    }

    /// GH #827: `off` is a level, not an absence. It used to send nothing, and
    /// on a model that thinks adaptively when `thinking` is missing (Sonnet 5,
    /// Opus 5) that left reasoning fully on — silently, on the one setting
    /// whose whole purpose is opting out.
    #[test]
    fn effort_off_sends_the_providers_disable_wire() {
        let opts = opts_with_reasoning(ThinkingLevel::Off);
        let v = build_provider_additional_params(Some("anthropic"), Some("claude-sonnet-5"), &opts)
            .expect("off must put the disable shape on the wire");
        assert_eq!(v["thinking"]["type"], "disabled");
        // Claude 4.6 keeps the omit — there, absence still means off, and the
        // toggle would be a 400.
        assert!(
            build_provider_additional_params(Some("anthropic"), Some("claude-opus-4-6"), &opts)
                .is_none(),
        );
        // GLM has carried this exact shape all along for tool-less one-shots;
        // `off` simply reaches it now.
        let glm = build_provider_additional_params(Some("glm"), None, &opts).unwrap();
        assert_eq!(glm["thinking"]["type"], "disabled");
    }

    /// ...and `off` is still NOT a reasoning turn. #816's `max_tokens`
    /// fallback on Anthropic reads `turn_reasoning_enabled`, so folding the
    /// disable shape into `reasoning_params` would have flipped it true and
    /// dropped the ceiling rig hard-errors without.
    #[test]
    fn effort_off_is_still_not_a_reasoning_turn() {
        let mut opts = opts_with_reasoning(ThinkingLevel::Off);
        opts.max_tokens = Some(8192);
        assert!(!turn_reasoning_enabled(
            Some("anthropic"),
            Some("claude-sonnet-5"),
            &opts
        ));
        assert_eq!(
            request_max_tokens(Some("anthropic"), Some("claude-sonnet-5"), &opts),
            Some(8192),
        );
    }

    /// Metadata passes through under its conventional key regardless of
    /// provider. Headers do NOT — see below.
    #[test]
    fn metadata_passes_through_for_all_providers() {
        let mut opts = StreamOptions::from_signal(AbortSignal::new());
        opts.metadata
            .insert("user_id".to_string(), serde_json::json!("u-42"));
        for provider in ["anthropic", "openai", "gemini", "ollama", "unknown"] {
            let v = build_provider_additional_params(Some(provider), None, &opts).unwrap();
            assert_eq!(v["metadata"]["user_id"], "u-42", "provider {provider}");
        }
    }

    /// dirge-vpma.25. These three assertions replace tests that asserted the
    /// OPPOSITE — that `api_key` produced `headers.Authorization = "Bearer …"`
    /// in the params, that an explicit header won over it, and that headers
    /// passed through for every provider.
    ///
    /// Those tests were written down from the implementation's output, so the
    /// bug became the contract. What they were pinning is a credential being
    /// serialized into the JSON request BODY (`additional_params` is
    /// serde-flattened into it), where it authenticates nothing — no provider
    /// promotes a body field to an HTTP header — and can be logged by the
    /// endpoint.
    #[test]
    fn request_header_knobs_contribute_nothing_to_the_body() {
        let mut opts = StreamOptions::from_signal(AbortSignal::new());
        opts.api_key = Some("dynamic-token".to_string());
        opts.headers
            .insert("X-Tenant".to_string(), "acme".to_string());
        opts.headers.insert(
            "Authorization".to_string(),
            "Bearer explicit-token".to_string(),
        );

        // Nothing else is set, so the whole params object must be absent.
        assert!(
            build_provider_additional_params(Some("openai"), None, &opts).is_none(),
            "api_key/headers must contribute nothing to the request body"
        );

        // And they must not ride along beside something that IS legitimate.
        opts.metadata
            .insert("user_id".to_string(), serde_json::json!("u-42"));
        let v = build_provider_additional_params(Some("openai"), None, &opts)
            .expect("metadata present");
        let body = serde_json::to_string(&v).expect("serializable");
        assert!(body.contains("u-42"), "metadata should still pass: {body}");
        for leaked in [
            "dynamic-token",
            "explicit-token",
            "Authorization",
            "X-Tenant",
        ] {
            assert!(
                !body.contains(leaked),
                "{leaked} reached the request body: {body}"
            );
        }
    }

    #[test]
    fn chatgpt_provider_auth_does_not_add_account_header_to_body() {
        let opts = StreamOptions::from_signal(AbortSignal::new());

        assert!(
            build_provider_additional_params(Some("openai-chatgpt-test"), None, &opts).is_none()
        );
    }

    /// No reasoning, no headers, no metadata → None (caller
    /// skips additional_params entirely).
    #[test]
    fn empty_options_produces_none() {
        let opts = StreamOptions::from_signal(AbortSignal::new());
        assert!(build_provider_additional_params(Some("anthropic"), None, &opts).is_none());
        assert!(build_provider_additional_params(None, None, &opts).is_none());
    }

    /// Unknown provider falls back to the generic
    /// `reasoning_level` key (debugging aid; rig provider impl
    /// may or may not honor).
    #[test]
    fn unknown_provider_uses_generic_key() {
        let opts = opts_with_reasoning(ThinkingLevel::High);
        let v = build_provider_additional_params(Some("future-provider"), None, &opts).unwrap();
        assert!(v.get("reasoning_level").is_some());
        assert!(v.get("reasoning").is_none());
        assert!(v.get("thinking").is_none());
    }

    // ============================================================
    // Phase-3 tool_search filter tests
    // ============================================================

    fn mk_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("desc for {name}"),
            parameters: serde_json::json!({}),
        }
    }

    /// Default-off: filter is None → every tool ships, byte-for-
    /// byte identical input/output. The behavior-preservation
    /// guarantee from the spec.
    #[test]
    fn tool_search_filter_none_passes_all_tools() {
        let defs = vec![mk_def("read"), mk_def("write"), mk_def("custom_mcp")];
        let out = filter_tool_defs(&defs, None);
        assert_eq!(out.len(), 3);
        let names: Vec<&str> = out.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["read", "write", "custom_mcp"]);
    }

    /// Empty loaded set + a filter → only always-on names ship
    /// (which includes `tool_search` itself by construction).
    #[test]
    fn tool_search_filter_empty_set_keeps_only_always_on() {
        let defs = vec![
            mk_def("read"),
            mk_def("write"),
            mk_def("tool_search"),
            mk_def("write_todo_list"),
            mk_def("task_status"),
            mk_def("session_search"),
            mk_def("custom_mcp"),
        ];
        let filter = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashSet::<String>::new(),
        ));
        let out = filter_tool_defs(&defs, Some(&filter));
        let names: std::collections::HashSet<String> = out.iter().map(|d| d.name.clone()).collect();
        // Always-on tools survive; the long tail doesn't.
        assert!(names.contains("tool_search"));
        assert!(names.contains("write_todo_list"));
        assert!(names.contains("task_status"));
        // dirge-w72q: the core loop tools are always-on too — the model can
        // read and write without a discovery round-trip.
        assert!(names.contains("read"), "read must ship unfiltered");
        assert!(names.contains("write"), "write must ship unfiltered");
        assert!(!names.contains("session_search"));
        assert!(!names.contains("custom_mcp"));
    }

    /// Filter containing only `tool_search` (already always-on)
    /// — other tools still suppressed. Mirrors the "filter is
    /// `{tool_search}` only, all other tools are absent" check
    /// from the spec.
    #[test]
    fn tool_search_filter_only_tool_search_suppresses_others() {
        let defs = vec![
            mk_def("session_search"),
            mk_def("spec"),
            mk_def("tool_search"),
            mk_def("custom_mcp"),
        ];
        let mut set = std::collections::HashSet::new();
        set.insert("tool_search".to_string());
        let filter = std::sync::Arc::new(std::sync::Mutex::new(set));
        let out = filter_tool_defs(&defs, Some(&filter));
        let names: Vec<&str> = out.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["tool_search"]);
    }

    /// After `tool_search` returns "custom_mcp", the shared set
    /// contains "custom_mcp"; the NEXT filter call surfaces it.
    /// Mirrors the spec's "model calls tool_search, NEXT turn's
    /// defs include the discovered tool" check.
    #[test]
    fn tool_search_filter_loaded_tool_surfaces_on_next_turn() {
        let defs = vec![
            mk_def("session_search"),
            mk_def("tool_search"),
            mk_def("custom_mcp"),
        ];
        let filter = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashSet::<String>::new(),
        ));
        // Turn 1: no "custom_mcp" in set.
        let out1 = filter_tool_defs(&defs, Some(&filter));
        assert!(!out1.iter().any(|d| d.name == "custom_mcp"));
        // Tool execution inserts "custom_mcp" into the shared set.
        filter.lock().unwrap().insert("custom_mcp".to_string());
        // Turn 2: "custom_mcp" must now ship.
        let out2 = filter_tool_defs(&defs, Some(&filter));
        assert!(out2.iter().any(|d| d.name == "custom_mcp"));
    }

    // ============================================================
    // dirge-41al: prompt deny_tools withhold the definition
    // ============================================================

    /// A prompt's `deny_tools` refuses the call before it leaves dirge, so
    /// shipping the schema only buys a rejected tool call. `--prompt ask`
    /// used to send all 34 defs including `write`/`edit`/`bash`.
    #[test]
    fn denied_tools_are_withheld_from_the_request() {
        let defs = vec![
            mk_def("read"),
            mk_def("write"),
            mk_def("edit"),
            mk_def("bash"),
            mk_def("grep"),
        ];
        let denied = vec!["write".to_string(), "edit".to_string(), "bash".to_string()];
        let out = retain_tool_defs(&defs, None, &denied);
        let names: Vec<&str> = out.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["read", "grep"]);
    }

    /// Empty deny list is the common case and must be byte-for-byte the old
    /// behavior.
    #[test]
    fn empty_deny_list_passes_every_tool() {
        let defs = vec![mk_def("read"), mk_def("write")];
        let out = retain_tool_defs(&defs, None, &[]);
        assert_eq!(out.len(), 2);
    }

    /// A qualified `mcp_tool:<server>:<name>` entry withholds that server's
    /// tool by its registered bare name. The `mcp_tool` umbrella does not
    /// expand here — a ToolDefinition can't be identified as MCP-exported,
    /// and hiding built-ins on a guess would be worse than shipping them.
    #[test]
    fn qualified_mcp_deny_matches_bare_name_umbrella_does_not() {
        let defs = vec![mk_def("read"), mk_def("search_docs")];
        let out = retain_tool_defs(
            &defs,
            None,
            &["mcp_tool:docs-server:search_docs".to_string()],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "read");

        let out = retain_tool_defs(&defs, None, &["mcp_tool".to_string()]);
        assert_eq!(out.len(), 2, "umbrella must not hide built-ins by guess");
    }

    /// Deny wins over always-on: a denied tool stays out even when
    /// dynamic_tool_search would otherwise force it into every request.
    #[test]
    fn deny_beats_the_always_on_set() {
        let defs = vec![mk_def("write"), mk_def("read"), mk_def("tool_search")];
        let filter = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashSet::<String>::new(),
        ));
        let out = retain_tool_defs(&defs, Some(&filter), &["write".to_string()]);
        let names: Vec<&str> = out.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["read", "tool_search"]);
    }

    /// `filter_tool_defs` reads the live deny list that `/prompt` writes, so
    /// a mid-session prompt switch changes the next request's tool list.
    #[test]
    fn filter_tool_defs_reads_the_live_prompt_deny_list() {
        // The deny list is process-global; use a name no sibling test ships
        // so a concurrent `filter_tool_defs` call can't observe this.
        let defs = vec![mk_def("read"), mk_def("deny_probe_tool")];
        crate::permission::apply_prompt_deny(&None, &["deny_probe_tool".to_string()]);
        let out = filter_tool_defs(&defs, None);
        crate::permission::apply_prompt_deny(&None, &[]);

        let names: Vec<&str> = out.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["read"]);
    }

    /// Names in the loaded set that aren't in the registry are
    /// silently ignored — matches the "user removed a tool"
    /// degraded path.
    #[test]
    fn tool_search_filter_ignores_unknown_names_in_set() {
        let defs = vec![mk_def("read"), mk_def("tool_search")];
        let mut set = std::collections::HashSet::new();
        set.insert("read".to_string());
        set.insert("phantom_tool".to_string()); // doesn't exist
        let filter = std::sync::Arc::new(std::sync::Mutex::new(set));
        let out = filter_tool_defs(&defs, Some(&filter));
        let names: std::collections::HashSet<String> = out.iter().map(|d| d.name.clone()).collect();
        assert!(names.contains("read"));
        assert!(names.contains("tool_search"));
        assert!(!names.contains("phantom_tool"));
        assert_eq!(out.len(), 2);
    }

    /// The per-request tool filter must preserve the registry's order.
    ///
    /// This is the sharp end of the fingerprint work. `filter_tool_defs`
    /// consults a `HashSet` of loaded names and the `PROMPT_DENIED_TOOLS`
    /// global; if it ever iterated either of those instead of walking the
    /// `tools` slice, the outgoing order would vary run to run — and since
    /// the provider caches over the order actually sent, every session would
    /// silently pay a full prefix rewrite. Nothing else in the suite pins
    /// this, and the cost of getting it wrong is invisible.
    #[test]
    fn filtering_preserves_the_registry_order() {
        let defs = vec![
            mk_def("read"),
            mk_def("write"),
            mk_def("grep"),
            mk_def("tool_search"),
        ];
        // A filter admitting several, inserted in an order deliberately
        // unlike the registry's, so an implementation iterating the SET
        // rather than the slice would produce the set's order instead.
        let mut set = std::collections::HashSet::new();
        set.insert("grep".to_string());
        set.insert("read".to_string());
        set.insert("write".to_string());
        let filter = std::sync::Arc::new(std::sync::Mutex::new(set));

        let first: Vec<String> = filter_tool_defs(&defs, Some(&filter))
            .iter()
            .map(|d| d.name.clone())
            .collect();

        // Registry order, not set order, not alphabetical.
        assert_eq!(
            first,
            vec!["read", "write", "grep", "tool_search"],
            "the outgoing list must follow the registry slice"
        );

        // And it must be stable across calls — a HashSet's iteration order
        // is randomised per process, but also varies as it is re-hashed, so
        // a single call could agree by luck.
        for _ in 0..16 {
            let again: Vec<String> = filter_tool_defs(&defs, Some(&filter))
                .iter()
                .map(|d| d.name.clone())
                .collect();
            assert_eq!(again, first, "tool order must not vary between requests");
        }
    }

    /// The emitter is deterministic and carries its comparison state forward.
    ///
    /// Note the corrected contract. This test used to say "permuting tool
    /// order must NOT change the digest (the helper sorts before hashing)" —
    /// and never asserted it. That intent was backwards: the provider caches
    /// over the tool list in the order it is SENT, so a permutation is a full
    /// prefix invalidation, and sorting before hashing made the one change
    /// that costs the most the one change the telemetry could not see.
    #[test]
    fn emit_cache_prefix_event_tracks_the_previous_prefix() {
        let defs = vec![mk_def("write"), mk_def("read")];
        let last = std::sync::Mutex::new(None);

        // First call establishes the baseline.
        emit_cache_prefix_event(Some("anthropic"), "preamble-x", &defs, 3, &last);
        let first = last.lock().unwrap().expect("baseline recorded");

        // An identical request must fingerprint identically, or every turn
        // would report drift and the signal would be worthless.
        emit_cache_prefix_event(Some("anthropic"), "preamble-x", &defs, 3, &last);
        let second = last.lock().unwrap().expect("still recorded");
        assert_eq!(first, second, "an unchanged prefix must not read as drift");
        assert!(!second.changes_from(&first).any());

        // Permuting the tool order DOES change it.
        let permuted = vec![mk_def("read"), mk_def("write")];
        emit_cache_prefix_event(Some("anthropic"), "preamble-x", &permuted, 3, &last);
        let third = last.lock().unwrap().expect("still recorded");
        assert_ne!(
            second, third,
            "the wire carries the tools in this order; a permutation invalidates the cache"
        );
        let change = third.changes_from(&second);
        assert!(change.tools, "attributed to the tools");
        assert!(!change.system, "the preamble did not move");
    }
}

#[cfg(test)]
mod tool_choice_tests {
    use super::*;
    use crate::agent::agent_loop::stream::StreamOptions;
    use crate::agent::agent_loop::tool::AbortSignal;
    use crate::agent::agent_loop::types::ToolChoice;

    fn opts(choice: Option<ToolChoice>) -> StreamOptions {
        let mut o = StreamOptions::from_signal(AbortSignal::new());
        o.tool_choice = choice;
        o
    }

    /// `None` must reach the request body, or the whole feature is inert.
    #[test]
    fn forbidding_tools_reaches_the_request_body() {
        let params =
            build_provider_additional_params(Some("openai"), None, &opts(Some(ToolChoice::None)))
                .expect("params");
        assert_eq!(
            params.get("tool_choice").and_then(|v| v.as_str()),
            Some("none")
        );
    }

    /// dirge-vpma.25: a credential must never reach the request body.
    ///
    /// `additional_params` is serde-flattened into the JSON body, so anything
    /// put there is sent as body content. The old `headers` key meant that
    /// setting `api_key` shipped `Authorization: Bearer <key>` to the endpoint
    /// as data — where it can be logged — while still failing to authenticate,
    /// because no provider promotes a body field to an HTTP header.
    #[test]
    fn a_credential_never_reaches_the_request_body() {
        let mut o = opts(None);
        o.api_key = Some("sk-super-secret".to_string());
        o.headers
            .insert("X-Custom".to_string(), "value".to_string());

        let params = build_provider_additional_params(Some("openai"), None, &o);

        // Nothing at all is the expected shape here: `headers` was the only
        // thing these two knobs contributed.
        let body = params
            .map(|p| serde_json::to_string(&p).expect("serializable"))
            .unwrap_or_default();
        assert!(
            !body.contains("sk-super-secret"),
            "the api key reached the request body: {body}"
        );
        assert!(
            !body.contains("Authorization"),
            "an Authorization header was serialized into the request body: {body}"
        );
        assert!(
            !body.contains("X-Custom"),
            "request headers were serialized into the request body: {body}"
        );
    }

    /// The discrimination half: the builder still emits what it is supposed
    /// to. Without this, the assertions above would pass just as well against
    /// a builder that had been broken into returning nothing at all.
    #[test]
    fn the_builder_still_emits_real_params() {
        let params =
            build_provider_additional_params(Some("openai"), None, &opts(Some(ToolChoice::None)))
                .expect("tool_choice must still be emitted");
        assert!(params.get("tool_choice").is_some());
    }

    /// dirge-vpma.26: the wire dump's `reasoning` flag must track whether
    /// reasoning params were actually emitted.
    ///
    /// It used to be `additional.is_some()`, which is true for any occupant of
    /// `additional_params` — a `tool_choice` gate or a metadata map with
    /// thinking fully off. The dump exists to be read during a debugging
    /// session, so a turn labelled reasoning-enabled that sent no reasoning
    /// params sends the reader after the wrong thing.
    #[test]
    fn a_turn_carrying_only_a_tool_gate_is_not_a_reasoning_turn() {
        let opts = opts(Some(ToolChoice::None));
        assert!(
            build_provider_additional_params(Some("openai"), None, &opts).is_some(),
            "precondition: the gate alone fills additional_params"
        );
        assert!(
            !turn_reasoning_enabled(Some("openai"), None, &opts),
            "a tool gate was reported as reasoning"
        );
    }

    /// The other half: a turn that really does ask for reasoning reports true,
    /// so the flag is not simply hardwired off.
    #[test]
    fn a_thinking_turn_is_a_reasoning_turn() {
        let mut o = opts(None);
        o.reasoning = Some(crate::agent::agent_loop::types::ThinkingLevel::High);
        assert!(
            turn_reasoning_enabled(Some("deepseek"), None, &o),
            "a thinking turn was not reported as reasoning"
        );
        let params = build_provider_additional_params(Some("deepseek"), None, &o)
            .expect("reasoning params must reach the body");
        assert!(
            params.get("reasoning_effort").is_some(),
            "precondition: deepseek carries a top-level effort: {params:?}"
        );
    }

    /// An unconstrained turn must send NOTHING — not `"auto"`. Some
    /// OpenAI-compatible backends reject `tool_choice` on a request carrying no
    /// `tools` array, and dirge makes plenty of those (summarizer, critic, goal
    /// judge), so saying what the provider already assumes would break them.
    /// This is also why there is no `ToolChoice::Auto` to send.
    #[test]
    fn an_unconstrained_turn_sends_nothing() {
        let params = build_provider_additional_params(Some("openai"), None, &opts(None));
        let has_key = params.as_ref().and_then(|p| p.get("tool_choice")).is_some();
        assert!(
            !has_key,
            "an unconstrained turn put tool_choice on the wire"
        );
    }

    /// The key is provider-independent — every backend dirge targets spells it
    /// `tool_choice` and reads `none` the same way.
    #[test]
    fn every_provider_gets_the_same_key() {
        for provider in [
            "openai",
            "anthropic",
            "deepseek",
            "glm",
            "openrouter",
            "custom",
        ] {
            let params = build_provider_additional_params(
                Some(provider),
                None,
                &opts(Some(ToolChoice::None)),
            )
            .unwrap_or_else(|| panic!("{provider} produced no params"));
            assert_eq!(
                params.get("tool_choice").and_then(|v| v.as_str()),
                Some("none"),
                "{provider} did not carry the tool gate"
            );
        }
    }
}
