//! `write_todo_list` — the model's bulk planning surface over the persistent
//! issue board.
//!
//! Historically this was a throwaway in-memory checklist separate from the
//! `issue` tool. The two have been consolidated: a todo IS an issue. Each
//! `write_todo_list` call upserts its items into the project's `issues` table
//! (see [`crate::extras::issue_db`]), scoped to the current session, so bulk
//! planning gains the full issue lifecycle (open / in_progress / blocked /
//! done / cancelled) and the model can rely on which items are still open.
//!
//! [`TODO_LIST`] is no longer the source of truth — it's a fast in-memory
//! mirror of this session's live board that the right-pane panel and the
//! end-of-turn nudge read without touching SQLite on every frame. Both tools
//! ([`WriteTodoList`] and [`super::issue::IssueTool`]) refresh it after a write,
//! and `session::rehydrate` refreshes it on resume.
//!
//! One of four similarly-named work-tracking surfaces — NOT the phased `/plan`
//! workflow, plan-**mode**, or background `task`s. See the canonical map in
//! [`crate::agent::plan`].

use std::path::{Path, PathBuf};

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::extras::issue_db::{
    IssueStore, is_terminal_status, normalize_priority, normalize_status,
};

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

/// In-memory mirror of the current session's live board (open / in_progress /
/// blocked), refreshed from the `issues` table after every write. The
/// right-pane TODOS panel and the end-of-turn nudge read this so neither has to
/// hit SQLite on every redraw. The DB is authoritative; this is a cache.
pub static TODO_LIST: std::sync::Mutex<Vec<TodoItem>> = std::sync::Mutex::new(Vec::new());

/// Re-query the session's live board from the issue DB and replace the
/// [`TODO_LIST`] mirror. Best-effort: a DB open/read failure leaves the mirror
/// as-is (transient lock) rather than blanking the panel. `session_id = None`
/// matches issues with a NULL session.
pub fn refresh_board(db_path: &Path, session_id: Option<&str>) {
    if let Ok(store) = IssueStore::open_at(db_path) {
        refresh_board_from(&store, session_id);
    }
}

/// Refresh the mirror from an already-open store. Callers that just wrote to
/// the board (the `write_todo_list` and `issue` tools) use this to avoid
/// re-opening the DB — a second `open_at` would re-run `CREATE TABLE IF NOT
/// EXISTS` and the WAL pragma and add lock contention on the shared file.
pub fn refresh_board_from(store: &IssueStore, session_id: Option<&str>) {
    if let Ok(board) = store.board_for_session(session_id, None) {
        let items: Vec<TodoItem> = board
            .into_iter()
            .map(|i| TodoItem {
                content: i.title,
                status: i.status,
                priority: i.priority,
            })
            .collect();
        *TODO_LIST.lock_ignore_poison() = items;
    }
}

/// Snapshot the current mirror. `save_session` persists this with the session
/// so the TODOS panel shows immediately on resume, before the first tool call
/// re-runs `refresh_board`.
pub fn snapshot() -> Vec<TodoItem> {
    TODO_LIST.lock_ignore_poison().clone()
}

/// Number of unfinished items on the mirrored board — anything still open or
/// in progress (blocked items are deliberately parked, terminal items are
/// gone). Used by the agent loop to nudge the model not to stop with unfinished
/// planned work.
pub fn unfinished_count() -> usize {
    // The mirror only ever holds normalized DB statuses (open / in_progress /
    // blocked), so this need only match the two that count as unfinished.
    // Blocked is deliberately parked and doesn't nudge.
    TODO_LIST
        .lock()
        .map(|list| {
            list.iter()
                .filter(|t| matches!(t.status.as_str(), "open" | "in_progress"))
                .count()
        })
        .unwrap_or(0)
}

pub struct WriteTodoList {
    db_path: PathBuf,
    session_id: Option<String>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
}

impl WriteTodoList {
    pub fn new(
        db_path: PathBuf,
        session_id: Option<String>,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        WriteTodoList {
            db_path,
            session_id,
            permission,
            ask_tx,
        }
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
            description: "Lay out or update a structured plan for a COMPLEX, MULTI-STEP task. Each item is a tracked issue on your persistent board (the same board the `issue` tool and /issues use), scoped to this session. Listing an item creates it or updates a matching one by title; statuses follow the issue lifecycle (pending|in_progress|completed|cancelled, plus blocked). Items you omit are NOT auto-closed — to finish one, restate it with status completed (or cancelled); the loop nudges you to finish or close open items before ending a turn. Use the `issue` tool for incremental single-item edits or cross-session work. Skip this for trivial single-step work; use `task` to delegate independent work to a background subagent.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "Task description (matched by title on later calls)" },
                                "status": { "type": "string", "description": "pending, in_progress, blocked, completed, or cancelled" },
                                "priority": { "type": "string", "description": "high, normal, or low" }
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

        // Cap the plan so a pathological agent can't bloat the board (and every
        // subsequent prompt's reminder) by spamming hundreds of items. 50 is
        // generous for any reasonable plan; longer lists usually mean the work
        // should be split across turns.
        const MAX_TODOS: usize = 50;
        if args.todos.len() > MAX_TODOS {
            return Err(ToolError::Msg(format!(
                "todo list too long ({} items); cap is {}. Trim the list or split the work across multiple turns.",
                args.todos.len(),
                MAX_TODOS,
            )));
        }

        // No active session (e.g. `--no-session`): the user opted out of
        // persistence, so keep the plan ephemeral in the mirror only rather
        // than writing durable rows that would leak into the project-wide
        // turn-start board reminder of every future session. Mirrors the old
        // in-memory `write_todo_list` for this mode: replace the whole list,
        // normalized and minus terminal items (which leave the board).
        let Some(session_id) = self.session_id.as_deref() else {
            let items: Vec<TodoItem> = args
                .todos
                .iter()
                .filter_map(|t| {
                    let content = t.content.trim();
                    let status = normalize_status(&t.status).unwrap_or("open");
                    if content.is_empty() || is_terminal_status(status) {
                        return None;
                    }
                    Some(TodoItem {
                        content: content.to_string(),
                        status: status.to_string(),
                        priority: normalize_priority(&t.priority)
                            .unwrap_or("normal")
                            .to_string(),
                    })
                })
                .collect();
            let live = items.len();
            *TODO_LIST.lock_ignore_poison() = items;
            return Ok(format!(
                "Tracked {live} live item(s) (not persisted — no active session)."
            ));
        };

        let triples: Vec<(&str, &str, &str)> = args
            .todos
            .iter()
            .map(|t| (t.content.as_str(), t.status.as_str(), t.priority.as_str()))
            .collect();

        let store = IssueStore::open_at(&self.db_path).map_err(ToolError::Msg)?;
        let applied = store
            .sync_todos(Some(session_id), &triples)
            .map_err(ToolError::Msg)?;

        // Refresh the panel/nudge mirror from the store we already hold open.
        refresh_board_from(&store, Some(session_id));
        let live = TODO_LIST.lock_ignore_poison().len();
        Ok(format!(
            "Synced {applied} item(s) to the board; {live} live item(s) now on this session's board."
        ))
    }
}

#[cfg(test)]
mod tool_tests {
    use super::*;

    fn tmp_db() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dirge-todotool-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.db")
    }

    /// `write_todo_list` writes its plan through to the session's issue board,
    /// upserting by title across calls (no duplicate rows, omitted items kept).
    #[tokio::test]
    async fn write_todo_list_persists_to_the_issue_board() {
        let db = tmp_db();
        let tool = WriteTodoList::new(db.clone(), Some("sess-1".into()), None, None);

        tool.call(TodoWriteArgs {
            todos: vec![
                TodoItem {
                    content: "Design API".into(),
                    status: "in_progress".into(),
                    priority: "high".into(),
                },
                TodoItem {
                    content: "Write tests".into(),
                    status: "pending".into(),
                    priority: "low".into(),
                },
            ],
        })
        .await
        .unwrap();

        let store = IssueStore::open_at(&db).unwrap();
        let board = store.board_for_session(Some("sess-1"), None).unwrap();
        assert_eq!(board.len(), 2);
        // in_progress sorts ahead of open.
        assert_eq!(board[0].title, "Design API");
        assert_eq!(board[0].status, "in_progress");

        // Second call completes one item; the other is omitted but must stay.
        tool.call(TodoWriteArgs {
            todos: vec![TodoItem {
                content: "Design API".into(),
                status: "completed".into(),
                priority: "high".into(),
            }],
        })
        .await
        .unwrap();

        let board = store.board_for_session(Some("sess-1"), None).unwrap();
        let titles: Vec<&str> = board.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, vec!["Write tests"], "omitted item stays live");
        // No duplicate "Design API" — it was upserted, then closed.
        assert_eq!(store.search("Design API", 10).unwrap().len(), 1);
    }
}

#[cfg(test)]
mod nudge_tests {
    use super::*;

    /// `unfinished_count` counts open/pending + in_progress, ignoring
    /// blocked/done/cancelled. (Mutates the global TODO_LIST mirror directly.)
    #[test]
    fn unfinished_count_counts_open_and_in_progress() {
        let item = |status: &str| TodoItem {
            content: "x".into(),
            status: status.into(),
            priority: "normal".into(),
        };
        {
            let mut list = TODO_LIST.lock_ignore_poison();
            *list = vec![
                item("done"),
                item("open"),
                item("in_progress"),
                item("blocked"),
            ];
        }
        assert_eq!(unfinished_count(), 2);
        {
            let mut list = TODO_LIST.lock_ignore_poison();
            *list = vec![item("done"), item("blocked")];
        }
        assert_eq!(unfinished_count(), 0);
        // Leave the global clean for any other consumer.
        TODO_LIST.lock_ignore_poison().clear();
    }
}
