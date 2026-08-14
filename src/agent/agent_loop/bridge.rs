//! Phase 4.5c — translate `LoopEvent` → `AgentEvent`.
//!
//! Dirge's UI and ACP consume `AgentEvent` (the legacy event
//! vocabulary). The new loop emits `LoopEvent` (the pi-style
//! vocabulary). This module bridges the two so existing
//! consumers can drink from the new loop without rewrites.
//!
//! Translation table:
//!
//! | `LoopEvent`                               | `AgentEvent`(s) emitted          |
//! |-------------------------------------------|----------------------------------|
//! | `AgentStart`                              | (none — dirge has no start event)|
//! | `AgentEnd { messages }`                   | `Done { response, tokens, cost }`|
//! | `TurnStart`                               | `TurnStart { index: counter }`   |
//! | `TurnEnd { message, tool_results }`       | `TurnEnd { index: counter }`     |
//! | `MessageStart { User }`                   | `UserMessage { content }`        |
//! | `MessageStart { Assistant }`              | (none — tokens flow from Update) |
//! | `MessageStart { ToolResult }`             | (none — already via ToolExecutionEnd) |
//! | `MessageStart { Custom }`                 | (none)                           |
//! | `MessageUpdate { TextStart/TextDelta }`   | `Token(delta_chunk)`             |
//! | `MessageUpdate { ThinkingStart/Delta }`   | `Reasoning(delta_chunk)`         |
//! | `MessageUpdate { ToolCall* / *End }`      | (none — covered elsewhere)       |
//! | `MessageEnd`                              | (none — Done finalizes)          |
//! | `ToolExecutionStart { id, name, args }`   | `ToolCall { id, name, args }`    |
//! |                                           | + `ToolStarted { id }`           |
//! | `ToolExecutionUpdate`                     | (none — no AgentEvent equivalent)|
//! | `ToolExecutionEnd { id, name, result }`   | `ToolResult { id, output, kind }`|
//! | `SystemNotice { content }`                | `SystemNotice { content }`       |
//!
//! **State maintained**:
//!   - `turn_index`: increments on each `TurnStart`; used to label
//!     `TurnStart` / `TurnEnd` events.
//!   - `last_text_emitted` / `last_reasoning_emitted`: concatenated
//!     text seen so far per kind. Each `MessageUpdate` carries the
//!     FULL `partial` message; we extract the concatenated text /
//!     reasoning, compute the delta vs last-seen, and emit only the
//!     new chunk. Mirrors how dirge's existing runner emits Token /
//!     Reasoning incrementally.
//!   - `tool_name_by_id`: records tool names at
//!     `ToolExecutionStart` so the matching `ToolResult` can pick
//!     the right `ToolContent` classification (`Text` vs `File`).
//!
//! The bridge is **stateful per-run** — instantiate one per
//! `run_agent_loop` invocation. Feeding events from multiple runs
//! through the same bridge would scramble turn indices and delta
//! tracking.

use std::collections::HashMap;

use compact_str::CompactString;

use crate::event::{AgentEvent, ToolContent};

use super::message::{ContentBlock, DeltaPhase, LoopEvent, LoopMessage, StopReason, TokenUsage};

/// One `Token` event, or none at all when the filter withheld everything the
/// chunk contained. An empty `Token` is not the same as no token: consumers
/// treat it as "the model said something" and light up their spinners.
fn token_or_nothing(text: String) -> Vec<AgentEvent> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![AgentEvent::Token(CompactString::from(text))]
    }
}

/// Bridges `LoopEvent` stream to `AgentEvent` stream. Stateful
/// per-run.
pub struct EventBridge {
    /// Turn counter. Incremented on each `LoopEvent::TurnStart`.
    /// `AgentEvent::TurnStart`/`TurnEnd` label themselves with
    /// this value.
    turn_index: u32,
    /// Concatenated text content emitted so far across all text
    /// blocks in the current run. Used to compute delta chunks
    /// for `Token` events from `MessageUpdate { TextDelta }`.
    last_text_emitted: String,
    /// Same as `last_text_emitted` but for reasoning / thinking
    /// content. Used for `Reasoning` events.
    last_reasoning_emitted: String,
    /// Tool name lookup at `ToolExecutionStart` time so the
    /// matching `ToolExecutionEnd` can classify the output's
    /// `ToolContent` (Text vs File). Matches the per-name
    /// classification dirge's existing runner uses (read /
    /// find_files / list_dir → File).
    tool_name_by_id: HashMap<String, String>,
    /// Per-run provider usage accumulation, so `AgentEnd` can report real
    /// token totals in `Done { tokens }` (input + output).
    run_usage: TokenUsage,
    /// Withholds tool calls the model wrote into its ANSWER instead of the
    /// tool channel (dirge-n00z). Every consumer of `AgentEvent` — the TUI,
    /// `-p`, ACP, subagents — reads the text through this bridge, so this is
    /// the one place that has to know, and the loop's transcript keeps the
    /// model's words verbatim either way.
    text_filter: super::call_syntax::DisplayFilter,
}

impl EventBridge {
    /// `allowed_tools` is the run's tool set. It gates the filter above:
    /// syntax naming a tool that will actually run is withheld, and syntax
    /// naming nothing is left alone. An empty set therefore withholds
    /// nothing, which is what the bridge's own tests want — but it means
    /// production must pass the real set, so the parameter is required
    /// rather than defaulted.
    pub fn new(allowed_tools: std::collections::HashSet<String>) -> Self {
        Self {
            turn_index: 0,
            last_text_emitted: String::new(),
            last_reasoning_emitted: String::new(),
            tool_name_by_id: HashMap::new(),
            run_usage: TokenUsage::default(),
            text_filter: super::call_syntax::DisplayFilter::new(allowed_tools),
        }
    }

    /// Translate one `LoopEvent` to zero-or-more `AgentEvent`s.
    /// Returns a `Vec` because some loop events expand to multiple
    /// dirge events (e.g. `ToolExecutionStart` → `ToolCall` +
    /// `ToolStarted`) and some expand to none (e.g. `AgentStart`).
    pub fn translate(&mut self, event: LoopEvent) -> Vec<AgentEvent> {
        match event {
            LoopEvent::AgentStart => Vec::new(),

            // Context compaction: log the event but no AgentEvent
            // conversion needed — the UI shows a status line when it
            // sees the event. In the future this could emit a
            // dedicated AgentEvent variant for richer UI feedback.
            LoopEvent::CompactionStarted { tokens_before } => {
                vec![AgentEvent::CompactionStarted { tokens_before }]
            }
            LoopEvent::ContextCompacted {
                ref new_session_id,
                tokens_before,
                tokens_after,
                ref summary,
                first_kept_index,
                compaction_kind,
                ref summary_model,
            } => {
                tracing::info!(
                    target: "dirge::agent_loop",
                    session_id = %new_session_id,
                    tokens_before,
                    tokens_after,
                    has_summary = !summary.is_empty(),
                    first_kept_index,
                    kind = ?compaction_kind,
                    "context compacted — session rotated"
                );
                vec![AgentEvent::ContextCompacted {
                    new_session_id: CompactString::new(new_session_id),
                    tokens_before,
                    tokens_after,
                    summary: CompactString::new(summary),
                    first_kept_index,
                    compaction_kind,
                    summary_model: summary_model.as_deref().map(CompactString::new),
                }]
            }

            LoopEvent::CheckpointRefresh { summary } => {
                vec![AgentEvent::CheckpointRefresh {
                    summary: CompactString::new(summary),
                }]
            }

            LoopEvent::RetryNotice {
                attempt,
                delay_ms,
                error,
            } => {
                vec![AgentEvent::RetryNotice {
                    attempt,
                    delay_ms,
                    error: CompactString::from(error),
                }]
            }

            LoopEvent::RepairStats { snapshot } => {
                vec![AgentEvent::RepairStats { snapshot }]
            }

            LoopEvent::SystemNotice { content } => {
                vec![AgentEvent::SystemNotice {
                    content: CompactString::from(content),
                }]
            }

            LoopEvent::EscalationActivated { provider, reason } => {
                tracing::info!(
                    target: "dirge::agent_loop",
                    provider = %provider,
                    reason = ?reason,
                    "dual-client escalation activated for next LLM call",
                );
                vec![AgentEvent::EscalationActivated {
                    provider: CompactString::from(provider),
                    reason,
                }]
            }

            LoopEvent::AgentEnd { messages } => {
                // Phase 4.5h-1: classify the run's terminal state
                // by inspecting the LAST assistant message:
                //   - stop_reason=Error + context-length signal
                //     → AgentEvent::ContextOverflow (UI auto-
                //       compacts and respawns)
                //   - stop_reason=Error otherwise → AgentEvent::Error
                //   - stop_reason=Aborted → AgentEvent::Done (the
                //     UI's existing Interjected event covers
                //     graceful aborts elsewhere; emit Done with
                //     empty response so consumers see uniform
                //     terminal events)
                //   - any other stop_reason (Stop, ToolUse, Length)
                //     → AgentEvent::Done with the assembled final
                //     text
                //
                // Pi's agent_end carries newMessages; dirge's Done
                // carries the final response string for the UI's
                // terminal render. `tokens` is the run's real provider
                // usage (input + output, accumulated from
                // `LoopEvent::Usage`); `cost` stays 0.0 — cost display
                // was deliberately dropped (see session/mod.rs).
                let last_assistant = messages.iter().rev().find_map(|m| match m {
                    LoopMessage::Assistant(a) => Some(a),
                    _ => None,
                });
                if let Some(a) = last_assistant
                    && matches!(a.stop_reason, StopReason::Error)
                {
                    let error_text = a
                        .error_message
                        .as_deref()
                        .unwrap_or("agent loop produced an error with no message");
                    // Cancellation via the interject channel
                    // (`AbortSignal::cancel()` from
                    // `LoopRunner::into_agent_runner`'s bridge task)
                    // surfaces as an error with this exact message
                    // from `rig_stream.rs:124`. It's NOT a real
                    // failure — it's the user asking to stop. Emit
                    // `Interjected` instead so the UI drains its
                    // queued messages and respawns, rather than
                    // `Error` which would drop them.
                    if error_text.contains("stream aborted by cancellation signal") {
                        let partial_response = a
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        return vec![AgentEvent::Interjected {
                            partial_response: CompactString::from(partial_response),
                            tokens: 0,
                        }];
                    }
                    let kind = crate::agent::recovery::classify_error(error_text);
                    return if matches!(kind, crate::agent::recovery::ErrorKind::ContextLength) {
                        // Extract the user prompt that triggered
                        // this run. Pi's runAgentLoop puts prompts
                        // FIRST in newMessages; new_messages[0]
                        // is the user prompt (or the start of one)
                        // for typical runs. agentLoopContinue
                        // starts new_messages empty — fall back to
                        // empty prompt in that case.
                        let prompt_text = messages
                            .iter()
                            .find_map(|m| match m {
                                LoopMessage::User(u) => Some(u.text_joined()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        vec![AgentEvent::ContextOverflow {
                            prompt: CompactString::from(prompt_text),
                            error: CompactString::from(error_text),
                        }]
                    } else {
                        vec![AgentEvent::Error(CompactString::from(error_text))]
                    };
                }
                let response = messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        LoopMessage::Assistant(a) => Some(
                            a.content
                                .iter()
                                .filter_map(|b| match b {
                                    ContentBlock::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join(""),
                        ),
                        _ => None,
                    })
                    .unwrap_or_default();
                // `-p` treats this as the authoritative full text and
                // overwrites whatever it echoed while streaming, so filtering
                // the Token stream alone would leave the syntax in the final
                // answer and in `--output-format json` (dirge-n00z).
                let response = self.text_filter.strip(&response);
                vec![AgentEvent::Done {
                    response: CompactString::from(response),
                    // Real provider totals for the run; `cost` stays 0.0
                    // by decision — cost display was dropped (rates can't
                    // be sourced or kept fresh offline).
                    tokens: self.run_usage.input_tokens + self.run_usage.output_tokens,
                    cost: 0.0,
                }]
            }

            LoopEvent::TurnStart => {
                let evt = AgentEvent::TurnStart {
                    index: self.turn_index,
                };
                self.turn_index += 1;
                // LOOP-9: reset the text and reasoning emission
                // trackers at each turn boundary. Without this,
                // `last_text_emitted` from turn 1 is still set
                // when turn 2 starts — if the model says the same
                // words again (e.g. "Sure, let me..."), those
                // bytes are already in the tracker and the second
                // turn's streaming text is silently dropped.
                self.last_text_emitted.clear();
                self.last_reasoning_emitted.clear();
                self.text_filter.reset();
                vec![evt]
            }

            LoopEvent::TurnEnd { .. } => {
                // `turn_index` was bumped at TurnStart; current
                // value is the NEXT turn's index. Use
                // `turn_index - 1` for the closing TurnEnd.
                let idx = self.turn_index.saturating_sub(1);
                // LOOP-10: drop tool-id → name entries whose
                // `ToolExecutionEnd` never landed (cancellation,
                // panic in dispatch, abnormal termination). Without
                // this the map grows for the bridge lifetime; on
                // long sessions with cancellations it becomes a
                // small leak.
                self.tool_name_by_id.clear();
                vec![AgentEvent::TurnEnd { index: idx }]
            }

            LoopEvent::Usage { usage } => {
                self.run_usage.input_tokens = self
                    .run_usage
                    .input_tokens
                    .saturating_add(usage.input_tokens);
                self.run_usage.cached_input_tokens = self
                    .run_usage
                    .cached_input_tokens
                    .saturating_add(usage.cached_input_tokens);
                self.run_usage.cache_creation_input_tokens = self
                    .run_usage
                    .cache_creation_input_tokens
                    .saturating_add(usage.cache_creation_input_tokens);
                self.run_usage.output_tokens = self
                    .run_usage
                    .output_tokens
                    .saturating_add(usage.output_tokens);
                vec![AgentEvent::Usage {
                    input_tokens: usage.input_tokens,
                    cached_input_tokens: usage.cached_input_tokens,
                    cache_creation_input_tokens: usage.cache_creation_input_tokens,
                    output_tokens: usage.output_tokens,
                }]
            }

            LoopEvent::MessageStart { message } => {
                match message {
                    LoopMessage::User(u) => {
                        vec![AgentEvent::UserMessage {
                            content: CompactString::from(u.text_joined()),
                        }]
                    }
                    LoopMessage::Custom(payload) => {
                        vec![AgentEvent::CustomMessage {
                            payload: payload.clone(),
                        }]
                    }
                    // Assistant / ToolResult starts don't map to
                    // AgentEvents — token streaming flows from
                    // MessageUpdate, tool results flow from
                    // ToolExecutionEnd.
                    _ => Vec::new(),
                }
            }

            LoopEvent::MessageEnd { message } => {
                // The message is complete, so anything the filter is still
                // holding is decided now: a region with no closing tag is a
                // truncated call, not one still arriving. Flushing HERE and
                // not at the turn boundary matters — tool results are
                // emitted between the two, and text released after them
                // would read as a comment on output it was written before.
                if matches!(message, LoopMessage::Assistant(_)) {
                    return token_or_nothing(self.text_filter.flush());
                }
                Vec::new()
            }

            LoopEvent::MessageUpdate { message, phase } => {
                match phase {
                    DeltaPhase::TextStart | DeltaPhase::TextDelta => {
                        // Concatenate all text content across the
                        // partial; compute delta vs last-emitted.
                        let concat: String = message
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        if concat.len() > self.last_text_emitted.len()
                            && concat.starts_with(&self.last_text_emitted)
                        {
                            let new_chunk = &concat[self.last_text_emitted.len()..];
                            let chunk = self.text_filter.push(new_chunk);
                            self.last_text_emitted = concat;
                            token_or_nothing(chunk)
                        } else if concat != self.last_text_emitted {
                            // Defensive: provider re-emitted text
                            // out of order. Emit the FULL concat
                            // as a single Token and resync — the
                            // filter resyncs with it, since what it
                            // was holding belonged to text that is
                            // being replaced wholesale.
                            self.text_filter.reset();
                            let chunk = self.text_filter.push(&concat);
                            self.last_text_emitted = concat;
                            token_or_nothing(chunk)
                        } else {
                            // No new text in this update.
                            Vec::new()
                        }
                    }
                    DeltaPhase::ThinkingStart | DeltaPhase::ThinkingDelta => {
                        let concat: String = message
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Thinking { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        if concat.len() > self.last_reasoning_emitted.len()
                            && concat.starts_with(&self.last_reasoning_emitted)
                        {
                            let new_chunk = &concat[self.last_reasoning_emitted.len()..];
                            let chunk = CompactString::from(new_chunk);
                            self.last_reasoning_emitted = concat;
                            vec![AgentEvent::Reasoning(chunk)]
                        } else if concat != self.last_reasoning_emitted {
                            let chunk = CompactString::from(concat.as_str());
                            self.last_reasoning_emitted = concat;
                            vec![AgentEvent::Reasoning(chunk)]
                        } else {
                            Vec::new()
                        }
                    }
                    // *End markers: content already emitted via
                    // the corresponding Delta events. No-op.
                    DeltaPhase::TextEnd
                    | DeltaPhase::ThinkingEnd
                    | DeltaPhase::ToolCallStart
                    | DeltaPhase::ToolCallDelta
                    | DeltaPhase::ToolCallEnd => Vec::new(),
                }
            }

            LoopEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                // Remember the name so the matching
                // ToolExecutionEnd can classify the output.
                self.tool_name_by_id
                    .insert(tool_call_id.clone(), tool_name.clone());
                // Pi/dirge fires both `ToolCall` AND `ToolStarted`
                // consecutively. ToolCall = "LLM emitted the
                // call", ToolStarted = "dispatch is imminent".
                // For the new loop these collapse to one event
                // since `tool_execution_start` IS dispatch-imminent.
                // We still emit both for back-compat with existing
                // UI / ACP consumers that distinguish them.
                vec![
                    AgentEvent::ToolCall {
                        id: CompactString::from(tool_call_id.clone()),
                        name: CompactString::from(tool_name),
                        args,
                    },
                    AgentEvent::ToolStarted {
                        id: CompactString::from(tool_call_id),
                    },
                ]
            }

            LoopEvent::ToolExecutionUpdate { .. } => {
                // Dirge has no `ToolUpdate` AgentEvent. Phase 6
                // hardening could add one if a real UI use case
                // emerges. For now: silent.
                Vec::new()
            }

            LoopEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name: _,
                result,
                is_error: _,
            } => {
                // Convert the LoopToolResult's content to a
                // single string (the LLM-facing payload). Pi's
                // content is `Vec<Value>` with text/image blocks;
                // dirge's AgentEvent::ToolResult is a flat
                // CompactString.
                let output = flatten_content(&result.content);
                let name = self.tool_name_by_id.remove(&tool_call_id);
                let kind = classify_tool(name.as_deref());
                vec![AgentEvent::ToolResult {
                    id: CompactString::from(tool_call_id),
                    output: CompactString::from(output),
                    kind,
                }]
            }
        }
    }
}

/// Flatten the `Vec<Value>` content blocks of a `LoopToolResult`
/// into a single string. Matches dirge's existing runner shape
/// (`AgentEvent::ToolResult.output: CompactString`).
///
/// Recognises `{type: "text", text: "..."}` blocks. Anything else
/// is JSON-stringified — better than dropping the data.
fn flatten_content(content: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for block in content {
        if let Some(obj) = block.as_object()
            && obj.get("type").and_then(|t| t.as_str()) == Some("text")
            && let Some(text) = obj.get("text").and_then(|t| t.as_str())
        {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
            continue;
        }
        // Fallback: stringify. Image / other types end up as
        // JSON for now — opaque but not lost.
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&block.to_string());
    }
    out
}

/// Classify a tool name into `ToolContent::Text` vs
/// `ToolContent::File`. Matches dirge's existing runner.rs
/// classification (read / find_files / list_dir → File; everything
/// else → Text).
fn classify_tool(name: Option<&str>) -> ToolContent {
    match name {
        Some("read") | Some("find_files") | Some("list_dir") => ToolContent::File,
        _ => ToolContent::Text,
    }
}

// Tests live in the sibling `bridge_tests.rs` file. `#[path = "..."]`
// pulls it in as the `tests` child module so the `use super::*`
// references inside continue to resolve against this module.
#[cfg(test)]
#[path = "bridge_tests.rs"]
mod tests;
