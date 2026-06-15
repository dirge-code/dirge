//! `write_todo_list` — the model's in-session checklist of discrete work
//! items, kept in a process-global `TODO_LIST`; the loop nudges the model to
//! finish or clear pending items before stopping.
//!
//! One of four similarly-named work-tracking surfaces — NOT the phased
//! `/plan` workflow, plan-**mode**, or background `task`s. See the canonical
//! map in [`crate::agent::plan`].

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
    pub priority: String,
}

#[derive(Deserialize)]
pub struct TodoWriteArgs {
    pub todos: Vec<TodoItem>,
}

pub static TODO_LIST: std::sync::Mutex<Vec<TodoItem>> = std::sync::Mutex::new(Vec::new());

/// Empty the todo list. Hooked into /clear so the TODOS panel resets along
/// with the conversation, mirroring `modified::clear_modified`.
pub fn clear() {
    TODO_LIST.lock_ignore_poison().clear();
}

/// Snapshot the current todo list. `save_session` persists this with the
/// session so a resumed session restores the TODOS panel even when a
/// compaction has dropped the originating `write_todo_list` call from the
/// message history.
pub fn snapshot() -> Vec<TodoItem> {
    TODO_LIST.lock_ignore_poison().clone()
}

/// Number of todos still `pending` or `in_progress`. Used by the agent loop
/// to nudge the model not to stop with unfinished planned work (ported from
/// vix's end-of-turn TODO-completion nudge, session.go:1551).
pub fn unfinished_count() -> usize {
    TODO_LIST
        .lock()
        .map(|list| {
            list.iter()
                .filter(|t| t.status == "pending" || t.status == "in_progress")
                .count()
        })
        .unwrap_or(0)
}

pub struct WriteTodoList {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
}

impl WriteTodoList {
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        WriteTodoList { permission, ask_tx }
    }
}

impl Tool for WriteTodoList {
    const NAME: &'static str = "write_todo_list";

    type Error = ToolError;
    type Args = TodoWriteArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "write_todo_list".to_string(),
            description: "Create or update a structured todo list to track progress on a COMPLEX, MULTI-STEP task in the current session. Each call REPLACES the whole list. Keep statuses current (pending / in_progress / completed / cancelled) — the loop nudges you to finish or clear pending items before ending a turn. Skip this for trivial single-step work. To delegate independent work to a background subagent instead, use `task`.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "Task description" },
                                "status": { "type": "string", "description": "pending, in_progress, completed, or cancelled" },
                                "priority": { "type": "string", "description": "high, medium, or low" }
                            },
                            "required": ["content", "status", "priority"]
                        },
                        "description": "Full list of tasks to track"
                    }
                },
                "required": ["todos"]
            }),
        }
    }

    async fn call(&self, args: TodoWriteArgs) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "write_todo_list", "").await?;

        // Cap the todo list so a pathological agent can't bloat
        // memory + every subsequent prompt by spamming hundreds of
        // todos. 50 is generous for any reasonable plan; lists
        // longer than that are usually a sign the agent should
        // break the task into a separate plan/loop pass.
        const MAX_TODOS: usize = 50;
        if args.todos.len() > MAX_TODOS {
            return Err(ToolError::Msg(format!(
                "todo list too long ({} items); cap is {}. Trim the list or split the work across multiple turns.",
                args.todos.len(),
                MAX_TODOS,
            )));
        }

        let mut list = TODO_LIST.lock_ignore_poison();
        *list = args.todos;

        if list.is_empty() {
            return Ok("Todo list cleared.".to_string());
        }

        let total = list.len();
        let completed = list.iter().filter(|t| t.status == "completed").count();
        let in_progress = list.iter().filter(|t| t.status == "in_progress").count();
        let pending = list.iter().filter(|t| t.status == "pending").count();

        let mut result = format!("Todo list ({} items, {} done):\n", total, completed);
        for item in list.iter() {
            let icon = match item.status.as_str() {
                "completed" => "[x]",
                "in_progress" => "[>]",
                "cancelled" => "[-]",
                _ => "[ ]",
            };
            result.push_str(&format!(
                "  {} [{}] {}\n",
                icon, item.priority, item.content
            ));
        }
        result.push_str(&format!(
            "\nSummary: {} pending, {} in progress, {} completed, {} cancelled",
            pending,
            in_progress,
            completed,
            list.iter().filter(|t| t.status == "cancelled").count()
        ));
        Ok(result)
    }
}

#[cfg(test)]
mod nudge_tests {
    use super::*;

    /// `unfinished_count` counts pending + in_progress, ignores
    /// completed/cancelled. (Sole mutator of the global TODO_LIST in tests.)
    #[test]
    fn unfinished_count_counts_pending_and_in_progress() {
        let item = |status: &str| TodoItem {
            content: "x".into(),
            status: status.into(),
            priority: "medium".into(),
        };
        {
            let mut list = TODO_LIST.lock_ignore_poison();
            *list = vec![
                item("completed"),
                item("pending"),
                item("in_progress"),
                item("cancelled"),
            ];
        }
        assert_eq!(unfinished_count(), 2);
        {
            let mut list = TODO_LIST.lock_ignore_poison();
            *list = vec![item("completed"), item("cancelled")];
        }
        assert_eq!(unfinished_count(), 0);
        // Leave the global clean for any other consumer.
        TODO_LIST.lock_ignore_poison().clear();
    }
}
