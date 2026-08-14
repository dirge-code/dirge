//! The material a compaction summarizes, independent of where it came from.
//!
//! # Why this exists (dirge-dlpl)
//!
//! dirge had two compaction implementations because it has two message types.
//! `/compact` works on `&[SessionMessage]`; the automatic in-loop fold works on
//! the loop's `&[Value]`. Each grew its own serializer, and from there its own
//! everything: the prompt-injection fencing landed on one and not the other
//! (dirge-tgb9, P1), tool calls reached one summarizer and not the other
//! (dirge-czg9), the source-coverage section shipped to one and had to be added
//! to the second separately, and `/compact` never validated its summary at all
//! before installing it over real messages.
//!
//! Four defects, one cause: **the two paths shared a purpose and no code.**
//!
//! So the message type stops being the thing everything else is built around.
//! Both callers convert to [`Turn`] — a role, its text, and the calls it made —
//! and every step after that is shared: one serializer, one set of caps, one
//! prompt builder, one section template, one validation. Adding a fifth
//! divergence now means deliberately writing a second implementation, rather
//! than forgetting to update one of two.
//!
//! [`Turn`] deliberately carries only what a SUMMARY needs. It is not a general
//! message type and should not grow into one — ids, timestamps, images and tree
//! nodes all stay in the types that own them.

use serde_json::Value;

use crate::session::{MessageRole, SessionMessage, ToolCallState};

/// Who produced a turn. Coarser than either source type, because a summary
/// does not distinguish an aborted tool result from a successful one — the
/// text says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    User,
    Assistant,
    System,
    ToolResult,
}

impl TurnRole {
    /// The label the summarizer sees. Kept identical across both callers so a
    /// prompt built from a session and one built from loop messages differ only
    /// where the conversations differ.
    pub fn label(self) -> &'static str {
        match self {
            TurnRole::User => "user",
            TurnRole::Assistant => "assistant",
            TurnRole::System => "system",
            TurnRole::ToolResult => "toolResult",
        }
    }
}

/// One tool invocation, as a summary needs it: what was called and against
/// what. The result is a separate [`Turn`] — it always was, on both paths,
/// which is why dirge-czg9 could lose the call while keeping the outcome.
#[derive(Debug, Clone)]
pub struct ToolCallRef {
    pub name: String,
    /// Arguments as JSON. Uncut here; the cut belongs to the serializer, which
    /// is the layer that knows the prompt budget.
    pub args: String,
}

/// A single turn of the material being summarized.
#[derive(Debug, Clone)]
pub struct Turn {
    pub role: TurnRole,
    pub text: String,
    pub calls: Vec<ToolCallRef>,
}

/// Convert the loop's `Vec<Value>` messages.
///
/// Handles both content shapes — a scalar string and the block array — because
/// both reach the loop: heal-on-load produces the first, live turns the second.
pub fn from_loop_messages(messages: &[Value]) -> Vec<Turn> {
    messages
        .iter()
        .map(|m| {
            let role = match m.get("role").and_then(|r| r.as_str()) {
                Some("user") => TurnRole::User,
                Some("system") => TurnRole::System,
                Some("tool") | Some("toolResult") => TurnRole::ToolResult,
                _ => TurnRole::Assistant,
            };
            Turn {
                role,
                text: crate::agent::compression::content_text(m.get("content")),
                calls: tool_calls_of(m.get("content")),
            }
        })
        .collect()
}

/// Convert a session's `SessionMessage` list.
///
/// A `SessionMessage` carries its tool calls in a side list with their results
/// attached, rather than as content blocks. Both become the same [`Turn`]
/// shape: the call on the assistant turn, the result as its own turn — which
/// is how the loop already represents it, so the serializer sees one layout.
pub fn from_session_messages(messages: &[SessionMessage]) -> Vec<Turn> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        let role = match m.role {
            MessageRole::User => TurnRole::User,
            MessageRole::Assistant => TurnRole::Assistant,
            MessageRole::System => TurnRole::System,
        };
        out.push(Turn {
            role,
            text: m.content.to_string(),
            calls: m
                .tool_calls
                .iter()
                .map(|tc| ToolCallRef {
                    name: tc.name.clone(),
                    args: tc.args.to_string(),
                })
                .collect(),
        });
        for tc in &m.tool_calls {
            let text = match &tc.state {
                ToolCallState::Completed { result } => result.clone(),
                ToolCallState::Interrupted => "<interrupted>".to_string(),
                ToolCallState::Failed { error } => format!("<failed: {error}>"),
            };
            out.push(Turn {
                role: TurnRole::ToolResult,
                text,
                calls: Vec::new(),
            });
        }
    }
    out
}

/// `toolCall` blocks of a loop message's content.
fn tool_calls_of(content: Option<&Value>) -> Vec<ToolCallRef> {
    let Some(Value::Array(blocks)) = content else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|b| {
            let obj = b.as_object()?;
            if obj.get("type").and_then(|t| t.as_str())? != "toolCall" {
                return None;
            }
            Some(ToolCallRef {
                name: obj
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("?")
                    .to_string(),
                args: obj
                    .get("arguments")
                    .map(|a| a.to_string())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ToolCallEntry;
    use compact_str::CompactString;
    use serde_json::json;

    fn sm(role: MessageRole, content: &str, tool_calls: Vec<ToolCallEntry>) -> SessionMessage {
        SessionMessage {
            role,
            content: CompactString::from(content),
            estimated_tokens: 0,
            id: CompactString::from("id"),
            timestamp: 0,
            tool_calls,
            images: Vec::new(),
        }
    }

    /// The point of the whole module: the same conversation, expressed in
    /// either message type, converts to the same material. If this drifts,
    /// `/compact` and the automatic fold are summarizing different things
    /// again — which is the bug this was built to end.
    #[test]
    fn both_sources_produce_the_same_material() {
        let from_loop = from_loop_messages(&[
            json!({"role": "user", "content": "fix the backfill"}),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "writing it"},
                    {"type": "toolCall", "id": "c1", "name": "write",
                     "arguments": {"path": "resume.rs"}},
                ],
            }),
            json!({"role": "toolResult", "content": "wrote 12 lines"}),
        ]);

        let from_session = from_session_messages(&[
            sm(MessageRole::User, "fix the backfill", vec![]),
            sm(
                MessageRole::Assistant,
                "writing it",
                vec![ToolCallEntry {
                    id: "c1".into(),
                    name: "write".into(),
                    args: json!({"path": "resume.rs"}),
                    state: ToolCallState::Completed {
                        result: "wrote 12 lines".into(),
                    },
                }],
            ),
        ]);

        assert_eq!(from_loop.len(), from_session.len(), "turn count differs");
        for (a, b) in from_loop.iter().zip(&from_session) {
            assert_eq!(a.role, b.role);
            assert_eq!(a.text, b.text);
            assert_eq!(a.calls.len(), b.calls.len());
            for (x, y) in a.calls.iter().zip(&b.calls) {
                assert_eq!(x.name, y.name);
                assert_eq!(x.args, y.args);
            }
        }
    }

    /// A tool call's arguments must survive the conversion from BOTH sources —
    /// losing them here would reintroduce dirge-czg9 one layer lower, where the
    /// serializer tests would not see it.
    #[test]
    fn tool_call_arguments_survive_both_conversions() {
        let loop_turns = from_loop_messages(&[json!({
            "role": "assistant",
            "content": [{"type": "toolCall", "id": "c", "name": "bash",
                         "arguments": {"command": "psql -f 0043.sql"}}],
        })]);
        assert!(loop_turns[0].calls[0].args.contains("psql -f 0043.sql"));

        let session_turns = from_session_messages(&[sm(
            MessageRole::Assistant,
            "",
            vec![ToolCallEntry {
                id: "c".into(),
                name: "bash".into(),
                args: json!({"command": "psql -f 0043.sql"}),
                state: ToolCallState::Completed {
                    result: "ok".into(),
                },
            }],
        )]);
        assert!(session_turns[0].calls[0].args.contains("psql -f 0043.sql"));
    }

    /// dirge-dlpl, the guard the whole refactor exists to make possible: the
    /// same conversation compacted either way produces the same PROMPT.
    ///
    /// Four defects came from the two paths sharing a purpose and no code, and
    /// each was invisible because both implementations looked correct on their
    /// own. This compares them end to end — material, serializer, fencing,
    /// template, caps — so a fifth divergence has to be written deliberately
    /// rather than forgotten into existence.
    #[test]
    fn both_paths_build_the_same_prompt() {
        use crate::agent::compression::{
            build_summary_prompt, estimate_turn_tokens, summary_budget,
        };

        let loop_msgs = vec![
            json!({"role": "user", "content": "get the backfill unstuck"}),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "running the migration"},
                    {"type": "toolCall", "id": "c1", "name": "bash",
                     "arguments": {"command": "psql -f 0043.sql"}},
                ],
            }),
            json!({"role": "toolResult", "content": "CREATE INDEX"}),
            json!({"role": "user", "content": "good, now the replay"}),
        ];
        let session_msgs = vec![
            sm(MessageRole::User, "get the backfill unstuck", vec![]),
            sm(
                MessageRole::Assistant,
                "running the migration",
                vec![ToolCallEntry {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: json!({"command": "psql -f 0043.sql"}),
                    state: ToolCallState::Completed {
                        result: "CREATE INDEX".into(),
                    },
                }],
            ),
            sm(MessageRole::User, "good, now the replay", vec![]),
        ];

        let from_loop = from_loop_messages(&loop_msgs);
        let from_session = from_session_messages(&session_msgs);
        let budget = summary_budget(estimate_turn_tokens(&from_loop));

        let a = build_summary_prompt(&from_loop, budget, None, None).expect("clean");
        let b = build_summary_prompt(&from_session, budget, None, None).expect("clean");
        assert_eq!(
            a, b,
            "the two compaction paths built different prompts from the same \
             conversation — they have diverged again"
        );
    }

    /// An interrupted or failed call still produces a result turn, so the
    /// summarizer is told the call did not return rather than seeing nothing.
    #[test]
    fn an_unfinished_call_still_yields_a_result_turn() {
        for (state, want) in [
            (ToolCallState::Interrupted, "<interrupted>"),
            (
                ToolCallState::Failed {
                    error: "boom".into(),
                },
                "<failed: boom>",
            ),
        ] {
            let turns = from_session_messages(&[sm(
                MessageRole::Assistant,
                "trying",
                vec![ToolCallEntry {
                    id: "c".into(),
                    name: "bash".into(),
                    args: json!({}),
                    state,
                }],
            )]);
            assert_eq!(turns.len(), 2);
            assert_eq!(turns[1].role, TurnRole::ToolResult);
            assert_eq!(turns[1].text, want);
        }
    }
}
