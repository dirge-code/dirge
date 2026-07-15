//! `issue` — the model's persistent issue/kanban board, stored in the
//! per-project session DB via [`crate::extras::issue_db::IssueStore`].
//!
//! Two buckets: ACTIVE (session-scoped, in the panel + nudged to finish)
//! and BACKLOG (unassigned, filed for later). `create` files to the passive
//! backlog (optionally under an epic via `epic=<id>`); `start` claims a
//! backlog issue onto the active queue. `show` on an epic also lists its
//! live children. The incremental, single-item surface over the board;
//! `write_todo_list` writes to the SAME store in bulk for laying out a plan.
//! Issues persist across sessions and are surfaced by the harness at turn
//! start, so the model doesn't have to remember to list them. This tool is
//! the WRITE + on-demand-read surface; routine reads are injected automatically.

use std::path::PathBuf;

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::extras::issue_db::{IssueStore, parse_issue_id};

#[derive(Deserialize)]
pub struct IssueArgs {
    /// create | list | show | start | block | close | update | search
    pub action: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    /// Issue id for show/update/start/block/close. Accepts 7 or "#7".
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    /// Parent epic id (create only). Accepts a number or a string like "#drg-a1b2".
    #[serde(default)]
    pub epic: Option<serde_json::Value>,
    /// Filter for `list` (status) or term for `search`.
    #[serde(default)]
    pub query: Option<String>,
}

/// Coerce the `id` field, which the model may send as a number or a string
/// like "drg-a1b2", "#7", or bare "a1b2".
fn coerce_id(v: &Option<serde_json::Value>) -> Option<String> {
    match v.as_ref()? {
        serde_json::Value::Number(n) => n.as_i64().map(|i| i.to_string()),
        serde_json::Value::String(s) => parse_issue_id(s),
        _ => None,
    }
}

pub struct IssueTool {
    db_path: PathBuf,
    session_id: Option<String>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
}

impl IssueTool {
    pub fn new(
        db_path: PathBuf,
        session_id: Option<String>,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        IssueTool {
            db_path,
            session_id,
            permission,
            ask_tx,
        }
    }

    fn store(&self) -> Result<IssueStore, ToolError> {
        IssueStore::open_at(&self.db_path).map_err(ToolError::Msg)
    }
}

fn render_issue(i: &crate::extras::issue_db::Issue) -> String {
    let mut out = format!("{} [{}] ({})\n  {}", i.id, i.status, i.priority, i.title);
    if !i.body.trim().is_empty() {
        out.push_str(&format!("\n  {}", i.body.replace('\n', "\n  ")));
    }
    if let Some(ref epic) = i.epic_id {
        out.push_str(&format!("\n  epic: {epic}"));
    }
    out.push_str(&format!(
        "\n  created {} · updated {}",
        i.created_at, i.updated_at
    ));
    out
}

impl Tool for IssueTool {
    const NAME: &'static str = "issue";

    type Error = ToolError;
    type Args = IssueArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "issue".to_string(),
            description: "Persistent issue/kanban board for tracking work (stored in the project DB, persists ACROSS sessions). The harness shows your open board at the start of each turn, so you don't need to list it constantly. This is the incremental, single-item surface; `write_todo_list` writes to the SAME board in bulk for laying out a multi-step plan. Actions: \
                create (title, optional body/priority high|normal|low, optional epic=<id>) — files to the BACKLOG for later (NOT on your active work queue; use `start` to pick it up when you actually begin it); \
                start (id → in_progress) — claims the issue onto your active work queue; block (id → blocked); close (id → done); \
                update (id, optional status open|in_progress|blocked|done|cancelled / priority / body); \
                show (id) — shows detail, and for an epic also lists its live children; list (optional status filter); search (query). \
                Ids look like \"drg-a1b2\" (legacy \"7\"/\"#7\" also accepted). Create issues as you discover work; start one when you begin it; close it when done.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "description": "create | list | show | start | block | close | update | search" },
                    "title": { "type": "string", "description": "Title (create)" },
                    "body": { "type": "string", "description": "Optional details (create/update)" },
                    "id": { "type": ["integer", "string"], "description": "Issue id for show/update/start/block/close (e.g. \"drg-a1b2\"; legacy 7/\"#7\" also accepted)" },
                    "status": { "type": "string", "description": "open | in_progress | blocked | done | cancelled (update)" },
                    "priority": { "type": "string", "description": "high | normal | low (create/update)" },
                    "epic": { "type": ["integer", "string"], "description": "Parent epic id (create only; e.g. \"drg-a1b2\" or 7)" },
                    "query": { "type": "string", "description": "Status filter for list, or search term for search" }
                },
                "required": ["action"]
            }),
        }
    }

    async fn call(&self, args: IssueArgs) -> Result<String, ToolError> {
        let action = args.action.trim().to_ascii_lowercase();
        let is_write = matches!(
            action.as_str(),
            "create" | "start" | "block" | "close" | "update"
        );
        if is_write {
            check_perm(&self.permission, &self.ask_tx, "issue", &action).await?;
        }
        let store = self.store()?;

        // After a write, re-sync the right-pane / nudge mirror from the store we
        // already hold open (no second DB open), so the panel reflects
        // issue-tool edits, not just `write_todo_list` ones.
        let refresh = || {
            crate::agent::tools::todo::refresh_board_from(&store, self.session_id.as_deref());
        };

        let need_id = |args: &IssueArgs| -> Result<String, ToolError> {
            coerce_id(&args.id).ok_or_else(|| {
                ToolError::Msg(format!(
                    "action '{action}' needs an `id` (e.g. 7 or \"#7\")"
                ))
            })
        };

        match action.as_str() {
            "create" => {
                let title = args
                    .title
                    .as_deref()
                    .ok_or_else(|| ToolError::Msg("create needs a `title`".to_string()))?;
                let epic_str = coerce_id(&args.epic);
                let id = store
                    .create(
                        title,
                        args.body.as_deref().unwrap_or(""),
                        args.priority.as_deref(),
                        None,
                        epic_str.as_deref(),
                    )
                    .map_err(ToolError::Msg)?;
                refresh();
                let mut msg = format!("Created issue {id}: {}", title.trim());
                if let Some(eid) = &epic_str {
                    msg.push_str(&format!(" (under {eid})"));
                }
                Ok(msg)
            }
            "start" | "block" | "close" => {
                let id = need_id(&args)?;
                let status = match action.as_str() {
                    "start" => "in_progress",
                    "block" => "blocked",
                    _ => "done",
                };
                if store.set_status(&id, status).map_err(ToolError::Msg)? {
                    // Starting an issue is the "I'm working on this now" signal:
                    // claim it for THIS conversation so it lands on the session's
                    // board / TODOS panel even if it was created elsewhere (or in
                    // a pre-fold incarnation of this session). No-op when the
                    // issue is already ours.
                    if status == "in_progress" {
                        store
                            .assign_to_session(&id, self.session_id.as_deref())
                            .map_err(ToolError::Msg)?;
                    }
                    refresh();
                    Ok(format!("Issue {id} → {status}"))
                } else {
                    Err(ToolError::Msg(format!("no issue {id}")))
                }
            }
            "update" => {
                let id = need_id(&args)?;
                if args.status.is_none() && args.priority.is_none() {
                    return Err(ToolError::Msg(
                        "update needs at least one of: status, priority".to_string(),
                    ));
                }
                // Pre-validate BOTH fields before mutating: set_status and
                // set_priority are two separate writes with no surrounding
                // transaction, so validating after the first write would let an
                // invalid second field leave a half-applied update (e.g. status
                // committed, then an invalid priority returns Err).
                use crate::extras::issue_db::{normalize_priority, normalize_status};
                if let Some(status) = args.status.as_deref() {
                    normalize_status(status).ok_or_else(|| {
                        ToolError::Msg(format!(
                            "unknown status '{status}' (use open|in_progress|blocked|done|cancelled)"
                        ))
                    })?;
                }
                if let Some(priority) = args.priority.as_deref() {
                    normalize_priority(priority).ok_or_else(|| {
                        ToolError::Msg(format!(
                            "unknown priority '{priority}' (use high|normal|low)"
                        ))
                    })?;
                }
                // Existence check up front so a missing id fails before any write.
                if store.get(&id).map_err(ToolError::Msg)?.is_none() {
                    return Err(ToolError::Msg(format!("no issue {id}")));
                }
                let mut changed = Vec::new();
                if let Some(status) = args.status.as_deref() {
                    store.set_status(&id, status).map_err(ToolError::Msg)?;
                    changed.push("status");
                    // An update that starts the issue claims it too (same as the
                    // `start` verb), so a picked-up issue joins this board.
                    if normalize_status(status) == Some("in_progress") {
                        store
                            .assign_to_session(&id, self.session_id.as_deref())
                            .map_err(ToolError::Msg)?;
                    }
                }
                if let Some(priority) = args.priority.as_deref() {
                    store.set_priority(&id, priority).map_err(ToolError::Msg)?;
                    changed.push("priority");
                }
                refresh();
                Ok(format!("Updated issue {id} ({})", changed.join(", ")))
            }
            "show" => {
                let id = need_id(&args)?;
                match store.get(&id).map_err(ToolError::Msg)? {
                    Some(issue) => {
                        let mut out = render_issue(&issue);
                        // Show live children of an epic.
                        if let Ok(kids) = store.children_of(&id)
                            && !kids.is_empty()
                        {
                            out.push_str(&format!("\nChildren ({}):", kids.len()));
                            for k in &kids {
                                out.push_str(&format!("\n  {}", k.one_line()));
                            }
                        }
                        Ok(out)
                    }
                    None => Err(ToolError::Msg(format!("no issue {id}"))),
                }
            }
            "list" => {
                let issues = match args
                    .query
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(status) => store.list_by_status(status).map_err(ToolError::Msg)?,
                    None => store.board(None).map_err(ToolError::Msg)?,
                };
                if issues.is_empty() {
                    return Ok("no matching issues".to_string());
                }
                let lines: Vec<String> = issues.iter().map(|i| i.one_line()).collect();
                Ok(format!("{} issue(s):\n{}", lines.len(), lines.join("\n")))
            }
            "search" => {
                let query = args
                    .query
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ToolError::Msg("search needs a `query`".to_string()))?;
                let hits = store.search(query, 20).map_err(ToolError::Msg)?;
                if hits.is_empty() {
                    return Ok(format!("no issues match '{query}'"));
                }
                let lines: Vec<String> = hits.iter().map(|i| i.one_line()).collect();
                Ok(format!("{} match(es):\n{}", lines.len(), lines.join("\n")))
            }
            other => Err(ToolError::Msg(format!(
                "unknown action '{other}' (use create|list|show|start|block|close|update|search)"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process-wide counter guarantees a unique temp dir even when two tests
    /// enter within the same clock nanosecond — `list`/`search` read the
    /// project-wide board, so a shared DB would let parallel tests see each
    /// other's issues (the source of a flaky `create_then_list_then_close_flow`).
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn unique_db(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.db")
    }

    fn tool() -> IssueTool {
        IssueTool::new(
            unique_db("dirge-issuetool"),
            Some("sess-1".into()),
            None,
            None,
        )
    }

    fn args(action: &str) -> IssueArgs {
        IssueArgs {
            action: action.into(),
            title: None,
            body: None,
            id: None,
            status: None,
            priority: None,
            epic: None,
            query: None,
        }
    }

    #[tokio::test]
    async fn create_then_list_then_close_flow() {
        let t = tool();
        let created = t
            .call(IssueArgs {
                title: Some("Build auth".into()),
                priority: Some("high".into()),
                ..args("create")
            })
            .await
            .unwrap();
        assert!(created.starts_with("Created issue drg-"), "{created}");
        // Extract the id from the created message.
        let id = created
            .strip_prefix("Created issue ")
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_string();

        // Create files to the PASSIVE backlog — issue is NOT on the active board.
        let store = t.store().unwrap();
        assert!(
            store
                .board_for_session(Some("sess-1"), None)
                .unwrap()
                .is_empty(),
            "freshly created issue must not be on the active work queue"
        );
        // But it IS findable via get and search.
        assert!(store.get(&id).unwrap().is_some());
        assert_eq!(store.search("Build auth", 10).unwrap().len(), 1);

        let listed = t.call(args("list")).await.unwrap();
        assert!(listed.contains(&id));
        assert!(listed.contains("Build auth"));

        // start → claims the issue onto the active session board.
        let started = t
            .call(IssueArgs {
                id: Some(serde_json::json!(&id)),
                ..args("start")
            })
            .await
            .unwrap();
        assert!(started.contains("in_progress"), "{started}");
        assert!(
            !store
                .board_for_session(Some("sess-1"), None)
                .unwrap()
                .is_empty(),
            "started issue must appear on the active work queue"
        );

        let closed = t
            .call(IssueArgs {
                id: Some(serde_json::json!(&id)),
                ..args("close")
            })
            .await
            .unwrap();
        assert!(closed.contains("done"), "{closed}");

        // closed issue drops off the default board.
        let after = t.call(args("list")).await.unwrap();
        assert!(after.contains("no matching issues"), "{after}");
    }

    #[tokio::test]
    async fn start_claims_a_foreign_issue_for_this_session() {
        // An issue created in another conversation, sitting in the same project
        // DB the tool is bound to.
        let db = unique_db("dirge-issuetool-claim");
        let store = IssueStore::open_at(&db).unwrap();
        let foreign = store
            .create("picked up", "", None, Some("other-sess"), None)
            .unwrap();

        let t = IssueTool::new(db.clone(), Some("sess-1".into()), None, None);
        // Nothing on sess-1's board before pickup.
        assert!(
            store
                .board_for_session(Some("sess-1"), None)
                .unwrap()
                .is_empty()
        );

        t.call(IssueArgs {
            id: Some(serde_json::json!(foreign)),
            ..args("start")
        })
        .await
        .unwrap();

        let board = store.board_for_session(Some("sess-1"), None).unwrap();
        assert_eq!(board.len(), 1, "start must pull the issue onto our board");
        assert_eq!(board[0].title, "picked up");
        assert_eq!(board[0].status, "in_progress");
    }

    #[tokio::test]
    async fn create_requires_title_and_close_requires_id() {
        let t = tool();
        assert!(t.call(args("create")).await.is_err());
        assert!(t.call(args("close")).await.is_err());
    }

    #[tokio::test]
    async fn unknown_action_errors() {
        let t = tool();
        assert!(t.call(args("frobnicate")).await.is_err());
    }

    #[tokio::test]
    async fn update_with_invalid_priority_does_not_half_apply_status() {
        let t = tool();
        let created = t
            .call(IssueArgs {
                title: Some("x".into()),
                ..args("create")
            })
            .await
            .unwrap();
        let id = created
            .strip_prefix("Created issue ")
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_string();
        // status valid, priority invalid → must error WITHOUT committing the
        // status change (no transaction wraps the two writes).
        let res = t
            .call(IssueArgs {
                id: Some(serde_json::json!(&id)),
                status: Some("done".into()),
                priority: Some("bogus".into()),
                ..args("update")
            })
            .await;
        assert!(res.is_err(), "invalid priority should error");
        // The issue must still be open (status not half-applied).
        let shown = t
            .call(IssueArgs {
                id: Some(serde_json::json!(&id)),
                ..args("show")
            })
            .await
            .unwrap();
        assert!(
            shown.contains("[open]"),
            "status must not have been applied: {shown}"
        );
    }

    #[tokio::test]
    async fn create_with_epic_stores_epic_id() {
        let t = tool();
        // Create an epic parent first.
        let parent_created = t
            .call(IssueArgs {
                title: Some("Epic".into()),
                ..args("create")
            })
            .await
            .unwrap();
        let parent_id = parent_created
            .strip_prefix("Created issue ")
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_string();

        // Create a child under the epic.
        let child_created = t
            .call(IssueArgs {
                title: Some("Child task".into()),
                epic: Some(serde_json::json!(&parent_id)),
                ..args("create")
            })
            .await
            .unwrap();
        assert!(
            child_created.starts_with("Created issue drg-"),
            "{child_created}"
        );
        assert!(
            child_created.contains(&format!("(under {parent_id})")),
            "should mention epic parent: {child_created}"
        );

        let store = t.store().unwrap();
        let child_id = child_created
            .strip_prefix("Created issue ")
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_string();
        let child = store.get(&child_id).unwrap().unwrap();
        assert_eq!(child.epic_id.as_deref(), Some(parent_id.as_str()));
    }

    #[tokio::test]
    async fn show_on_epic_lists_children() {
        let t = tool();
        let parent_id = t
            .call(IssueArgs {
                title: Some("Parent epic".into()),
                ..args("create")
            })
            .await
            .unwrap();
        let parent_id = parent_id
            .strip_prefix("Created issue ")
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_string();

        // Create two children.
        let c1 = t
            .call(IssueArgs {
                title: Some("Child A".into()),
                epic: Some(serde_json::json!(&parent_id)),
                ..args("create")
            })
            .await
            .unwrap();
        let c1_id = c1
            .strip_prefix("Created issue ")
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_string();
        t.call(IssueArgs {
            title: Some("Child B".into()),
            epic: Some(serde_json::json!(&parent_id)),
            ..args("create")
        })
        .await
        .unwrap();

        // show on the parent includes children.
        let shown = t
            .call(IssueArgs {
                id: Some(serde_json::json!(&parent_id)),
                ..args("show")
            })
            .await
            .unwrap();
        assert!(shown.contains("Children (2):"), "{shown}");
        assert!(shown.contains("Child A"), "{shown}");
        assert!(shown.contains("Child B"), "{shown}");
        assert!(shown.contains(&c1_id), "{shown}");
    }

    #[tokio::test]
    async fn show_on_child_contains_epic_line() {
        let t = tool();
        let parent_id = t
            .call(IssueArgs {
                title: Some("Epic".into()),
                ..args("create")
            })
            .await
            .unwrap();
        let parent_id = parent_id
            .strip_prefix("Created issue ")
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_string();

        let child = t
            .call(IssueArgs {
                title: Some("Under epic".into()),
                epic: Some(serde_json::json!(&parent_id)),
                ..args("create")
            })
            .await
            .unwrap();
        let child_id = child
            .strip_prefix("Created issue ")
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_string();

        let shown = t
            .call(IssueArgs {
                id: Some(serde_json::json!(&child_id)),
                ..args("show")
            })
            .await
            .unwrap();
        assert!(shown.contains(&format!("epic: {parent_id}")), "{shown}");
    }
}
