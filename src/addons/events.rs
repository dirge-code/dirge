//! The run's events as the `:dirge/event` hook reads them.
//!
//! One hook key hears every event the front end gets, so an addon can watch
//! a new part of the run without dirge growing a hook point for it. Each
//! event reaches the hook as a flat map whose `:event` names it
//! (`:turn-start`, `:tool-call`, `:tool-result`, `:done`, …). Answers are
//! ignored and nothing waits for them.

use serde_json::{Value, json};

use crate::event::{AgentEvent, CompactionKind};

/// The hook key events are posted to.
pub const EVENT_KEY: &str = "dirge/event";

/// Longest text an event carries, in bytes; longer text is cut at a char
/// boundary and marked.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;

/// `event` as the `:dirge/event` hook's ctx, or `None` for the events it
/// does not hear: streamed token and reasoning deltas (the whole response
/// arrives with `:done`), and the tool-started tick that always follows
/// `:tool-call`.
pub fn project(event: &AgentEvent) -> Option<Value> {
    let ctx = match event {
        AgentEvent::Token(_) | AgentEvent::Reasoning(_) | AgentEvent::ToolStarted { .. } => {
            return None;
        }
        AgentEvent::ToolCall { id, name, args } => {
            json!({ "event": "tool-call", "id": id.as_str(), "tool": name.as_str(), "args": args })
        }
        AgentEvent::ToolResult { id, output, .. } => {
            json!({ "event": "tool-result", "id": id.as_str(), "output": clip(output) })
        }
        AgentEvent::Error(message) => json!({ "event": "error", "message": clip(message) }),
        AgentEvent::ContextOverflow { error, .. } => {
            json!({ "event": "context-overflow", "message": clip(error) })
        }
        AgentEvent::CompactionStarted { tokens_before } => {
            json!({ "event": "compaction-started", "tokens-before": tokens_before })
        }
        AgentEvent::ContextCompacted {
            new_session_id,
            tokens_before,
            tokens_after,
            summary,
            compaction_kind,
            ..
        } => json!({
            "event": "context-compacted",
            "session-id": new_session_id.as_str(),
            "tokens-before": tokens_before,
            "tokens-after": tokens_after,
            "summary": clip(summary),
            "kind": compaction_kind_name(*compaction_kind),
        }),
        AgentEvent::CheckpointRefresh { summary } => {
            json!({ "event": "checkpoint", "summary": clip(summary) })
        }
        AgentEvent::Done {
            response,
            tokens,
            cost,
        } => json!({
            "event": "done",
            "response": clip(response),
            "tokens": tokens,
            "cost": cost,
        }),
        AgentEvent::Usage {
            input_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            output_tokens,
        } => json!({
            "event": "usage",
            "input-tokens": input_tokens,
            "cached-input-tokens": cached_input_tokens,
            "cache-creation-input-tokens": cache_creation_input_tokens,
            "output-tokens": output_tokens,
        }),
        AgentEvent::TurnStart { index } => json!({ "event": "turn-start", "index": index }),
        AgentEvent::TurnEnd { index } => json!({ "event": "turn-end", "index": index }),
        AgentEvent::CustomMessage { payload } => {
            json!({ "event": "custom-message", "payload": payload })
        }
        AgentEvent::Interjected {
            partial_response,
            tokens,
        } => json!({
            "event": "interjected",
            "response": clip(partial_response),
            "tokens": tokens,
        }),
        AgentEvent::UserMessage { content } => {
            json!({ "event": "user-message", "content": clip(content) })
        }
        AgentEvent::RetryNotice {
            attempt,
            delay_ms,
            error,
        } => json!({
            "event": "retry",
            "attempt": attempt,
            "delay-ms": delay_ms,
            "message": clip(error),
        }),
        AgentEvent::SystemNotice { content } => {
            json!({ "event": "notice", "content": clip(content) })
        }
        AgentEvent::RepairStats { .. } => json!({ "event": "repair-stats" }),
        AgentEvent::EscalationActivated { provider, reason } => json!({
            "event": "escalation",
            "provider": provider.as_str(),
            "reason": format!("{reason:?}"),
        }),
    };
    Some(ctx)
}

fn compaction_kind_name(kind: CompactionKind) -> &'static str {
    match kind {
        CompactionKind::PruneOnly => "prune-only",
        CompactionKind::PruneAndSummary => "prune-and-summary",
        CompactionKind::PruneAndFailedSummary => "prune-and-failed-summary",
        CompactionKind::PruneSummarizerDisabled => "prune-summarizer-disabled",
    }
}

/// `text` cut to [`MAX_TEXT_BYTES`] at a char boundary, marked when cut.
fn clip(text: &str) -> String {
    if text.len() <= MAX_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[{} more bytes]", &text[..end], text.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_and_the_started_tick_are_not_heard() {
        assert!(project(&AgentEvent::Token("a".into())).is_none());
        assert!(project(&AgentEvent::Reasoning("a".into())).is_none());
        assert!(project(&AgentEvent::ToolStarted { id: "1".into() }).is_none());
    }

    #[test]
    fn a_tool_call_names_its_tool_and_args() {
        let ctx = project(&AgentEvent::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args: json!({"path": "a.rs"}),
        })
        .unwrap();
        assert_eq!(
            ctx,
            json!({"event": "tool-call", "id": "c1", "tool": "read", "args": {"path": "a.rs"}})
        );
    }

    #[test]
    fn turn_bounds_and_the_end_of_a_run_are_heard() {
        assert_eq!(
            project(&AgentEvent::TurnEnd { index: 2 }).unwrap(),
            json!({"event": "turn-end", "index": 2})
        );
        let done = project(&AgentEvent::Done {
            response: "ok".into(),
            tokens: 10,
            cost: 0.5,
        })
        .unwrap();
        assert_eq!(done["event"], "done");
        assert_eq!(done["response"], "ok");
    }

    #[test]
    fn long_text_is_cut_at_a_char_boundary_and_marked() {
        let long = "é".repeat(MAX_TEXT_BYTES);
        let ctx = project(&AgentEvent::ToolResult {
            id: "c1".into(),
            output: long.as_str().into(),
            kind: Default::default(),
        })
        .unwrap();
        let output = ctx["output"].as_str().unwrap();
        assert!(output.len() < long.len());
        assert!(output.contains("more bytes]"));
    }
}
