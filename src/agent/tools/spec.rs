//! The `spec` tool: a spec-driven workflow tracker (OpenSpec-inspired)
//! backed by the per-project SQLite store ([`crate::extras::spec_db`]).
//!
//! One action-dispatched tool, mirroring the `memory` tool's shape. The
//! agent proposes a change, records requirement deltas + a task checklist,
//! works the tasks (real status, not regex over checkboxes), then archives
//! — folding the deltas into the living specs in one transaction.

use std::sync::Arc;

use rig::tool::PortableTool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::extras::memory_provider::MemoryProvider;
use crate::extras::spec_db::{Scenario, SpecStore};

pub struct SpecTool {
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    store: Arc<SpecStore>,
    /// Optional project memory store. On `archive`, the change's intent and
    /// design decisions are folded into a durable memory so the rationale
    /// outlives the change record. `None` disables that (no-op).
    memory: Option<Arc<dyn MemoryProvider>>,
}

impl SpecTool {
    pub fn new(
        store: Arc<SpecStore>,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            store,
            memory: None,
        }
    }

    /// Attach the project memory store so `archive` forms a memory of the
    /// change's why/decisions. `None` is a no-op.
    pub fn with_memory(mut self, memory: Option<Arc<dyn MemoryProvider>>) -> Self {
        self.memory = memory;
        self
    }
}

#[derive(Deserialize)]
pub struct Args {
    action: String,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    why: Option<String>,
    #[serde(default)]
    what: Option<String>,
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    value: Option<String>,
    // tasks
    #[serde(default)]
    group_no: Option<i64>,
    #[serde(default)]
    seq: Option<i64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    task_id: Option<i64>,
    #[serde(default)]
    status: Option<String>,
    // deltas
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    capability: Option<String>,
    #[serde(default)]
    requirement: Option<String>,
    #[serde(default)]
    scenarios: Option<Vec<Scenario>>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    migration: Option<String>,
    #[serde(default)]
    rename_to: Option<String>,
}

fn req<'a>(v: &'a Option<String>, name: &str) -> Result<&'a str, ToolError> {
    match v.as_deref() {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(ToolError::Msg(format!("'{name}' is required"))),
    }
}

fn json(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.into())
}

impl PortableTool for SpecTool {
    const NAME: &'static str = "spec";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    fn description(&self) -> String {
        r#"Spec-driven workflow tracker (SQLite-backed). Align on WHAT before HOW, then track implementation. Living specs (capability → requirement → scenario) are the current truth; a CHANGE carries requirement deltas + a task checklist; ARCHIVE folds the deltas into the living specs.

Actions:
- propose (slug, why, what): new change.
- add_delta (slug, op=added|modified|removed|renamed, capability, requirement; text+scenarios for added/modified; reason+migration for removed; rename_to for renamed): record a requirement delta.
- add_task (slug, text): append a checklist item (auto-sequenced).
- set_task (task_id, status=pending|in_progress|done|blocked): update task status.
- archive (slug): fold deltas into living specs once all tasks are done.
- status (slug, or none to list all): inspect a change.
- specs (capability, or none to list all): read living requirements.
- set_field (slug, field=title|why|what|design, value): edit a change field.

scenarios = array of {name, when_then} (when_then in WHEN/THEN form)."#
                .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["propose", "set_field", "add_delta", "add_task", "set_task", "archive", "status", "specs"],
                    "description": "The action to perform."
                },
                "slug": {"type": "string", "description": "Change identifier, kebab-case (e.g. add-dark-mode)."},
                "title": {"type": "string", "description": "Human title for propose."},
                "why": {"type": "string", "description": "Why this change is needed (propose)."},
                "what": {"type": "string", "description": "What changes (propose)."},
                "field": {"type": "string", "enum": ["title", "why", "what", "design"], "description": "Field to update (set_field)."},
                "value": {"type": "string", "description": "New value (set_field)."},
                "group_no": {"type": "integer", "description": "Task group number (add_task; default 1)."},
                "seq": {"type": "integer", "description": "Task order within its group (add_task; auto if omitted)."},
                "text": {"type": "string", "description": "Task description (add_task)."},
                "task_id": {"type": "integer", "description": "Task id (set_task)."},
                "status": {"type": "string", "enum": ["pending", "in_progress", "done", "blocked"], "description": "Task status (set_task)."},
                "op": {"type": "string", "enum": ["added", "modified", "removed", "renamed"], "description": "Delta operation (add_delta)."},
                "capability": {"type": "string", "description": "Capability (kebab-case) the requirement belongs to."},
                "requirement": {"type": "string", "description": "Requirement name."},
                "scenarios": {
                    "type": "array",
                    "items": {"type": "object", "properties": {"name": {"type": "string"}, "when_then": {"type": "string"}}, "required": ["name", "when_then"]},
                    "description": "Behavior examples for added/modified requirements."
                },
                "reason": {"type": "string", "description": "Why a requirement is removed (removed)."},
                "migration": {"type": "string", "description": "Migration path for a removed requirement (removed)."},
                "rename_to": {"type": "string", "description": "New requirement name (renamed)."}
            },
            "required": ["action"]
        })
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "spec", &args.action).await?;
        let store = &self.store;
        let m = |e: String| ToolError::Msg(e);

        match args.action.as_str() {
            "propose" => {
                let slug = req(&args.slug, "slug")?;
                let why = req(&args.why, "why")?;
                let what = req(&args.what, "what")?;
                let title = args.title.as_deref().unwrap_or("");
                store.create_change(slug, title, why, what).map_err(m)?;
                // A proposed change is immediately the active one.
                store.set_change_status(slug, "active").map_err(m)?;
                Ok(format!(
                    "Created change '{slug}'. Next: add_delta for each requirement, then add_task for the implementation checklist."
                ))
            }
            "set_field" => {
                let slug = req(&args.slug, "slug")?;
                let field = req(&args.field, "field")?;
                let value = args.value.as_deref().unwrap_or("");
                store.set_change_field(slug, field, value).map_err(m)?;
                Ok(format!("Updated {field} of '{slug}'."))
            }
            "add_delta" => {
                let slug = req(&args.slug, "slug")?;
                let op = req(&args.op, "op")?;
                let capability = req(&args.capability, "capability")?;
                let requirement = req(&args.requirement, "requirement")?;
                let scenarios = args.scenarios.clone().unwrap_or_default();
                let id = store
                    .add_delta(
                        slug,
                        op,
                        capability,
                        requirement,
                        args.text.as_deref().unwrap_or(""),
                        &scenarios,
                        args.reason.as_deref().unwrap_or(""),
                        args.migration.as_deref().unwrap_or(""),
                        args.rename_to.as_deref().unwrap_or(""),
                    )
                    .map_err(m)?;
                Ok(format!(
                    "Recorded {op} delta #{id} on '{slug}' ({capability}: {requirement})."
                ))
            }
            "add_task" => {
                let slug = req(&args.slug, "slug")?;
                let text = req(&args.text, "text")?;
                let group_no = args.group_no.unwrap_or(1);
                let seq = match args.seq {
                    Some(s) => s,
                    None => {
                        // Auto-sequence within the group.
                        let existing = store.list_tasks(slug).map_err(m)?;
                        existing.iter().filter(|t| t.group_no == group_no).count() as i64 + 1
                    }
                };
                let id = store.add_task(slug, group_no, seq, text).map_err(m)?;
                Ok(format!("Added task #{id} ({group_no}.{seq}) to '{slug}'."))
            }
            "set_task" => {
                let task_id = args
                    .task_id
                    .ok_or_else(|| ToolError::Msg("'task_id' is required".into()))?;
                let status = req(&args.status, "status")?;
                store.set_task_status(task_id, status).map_err(m)?;
                Ok(format!("Task #{task_id} → {status}."))
            }
            "archive" => {
                let slug = req(&args.slug, "slug")?;
                // Capture the change first: it must exist, must not already be
                // archived (re-archiving would re-fold its deltas), and we use
                // its why/design to form a memory after the fold.
                let change = store
                    .get_change(slug)
                    .map_err(m)?
                    .ok_or_else(|| ToolError::Msg(format!("no change '{slug}'")))?;
                if change.status == "archived" {
                    return Err(ToolError::Msg(format!("'{slug}' is already archived.")));
                }
                let (done, total) = store.task_progress(slug).map_err(m)?;
                if total > 0 && done < total {
                    return Err(ToolError::Msg(format!(
                        "Cannot archive '{slug}': {done}/{total} tasks done. Finish or remove open tasks first."
                    )));
                }
                let report = store.archive_change(slug).map_err(m)?;
                // Fold the change's why/design into durable project memory so
                // the rationale outlives the change record (best-effort).
                if let Some(mem) = &self.memory {
                    let c = &change;
                    let mut content = format!("Shipped change '{}'.", c.slug);
                    if !c.why.trim().is_empty() {
                        content.push_str(&format!(" Why: {}", c.why.trim()));
                    }
                    if !c.design.trim().is_empty() {
                        content.push_str(&format!(" Decisions: {}", c.design.trim()));
                    }
                    let _ = mem.add("memory", &content, Some("episodic"));
                }
                Ok(format!(
                    "Archived '{slug}'. Folded into living specs: {} added, {} modified, {} removed, {} renamed.",
                    report.added, report.modified, report.removed, report.renamed
                ))
            }
            "status" => match args.slug.as_deref() {
                Some(slug) if !slug.trim().is_empty() => {
                    let change = store
                        .get_change(slug)
                        .map_err(m)?
                        .ok_or_else(|| ToolError::Msg(format!("no change '{slug}'")))?;
                    let tasks = store.list_tasks(slug).map_err(m)?;
                    let deltas = store.list_deltas(slug).map_err(m)?;
                    let (done, total) = store.task_progress(slug).map_err(m)?;
                    Ok(json(&serde_json::json!({
                        "change": change,
                        "progress": {"done": done, "total": total},
                        "tasks": tasks,
                        "deltas": deltas,
                    })))
                }
                _ => {
                    let changes = store.list_changes(None).map_err(m)?;
                    Ok(json(&serde_json::json!({ "changes": changes })))
                }
            },
            "specs" => match args.capability.as_deref() {
                Some(cap) if !cap.trim().is_empty() => {
                    let reqs = store.capability_requirements(cap).map_err(m)?;
                    Ok(json(
                        &serde_json::json!({ "capability": cap, "requirements": reqs }),
                    ))
                }
                _ => {
                    let caps = store.list_capabilities().map_err(m)?;
                    Ok(json(&serde_json::json!({ "capabilities": caps })))
                }
            },
            other => Err(ToolError::Msg(format!("unknown spec action '{other}'"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tool() -> (SpecTool, std::path::PathBuf) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dirge-spectool-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let store = SpecStore::open_at(&dir.join("state.db")).unwrap();
        (SpecTool::new(Arc::new(store), None, None), dir)
    }

    async fn call(t: &SpecTool, v: serde_json::Value) -> Result<String, ToolError> {
        t.call(serde_json::from_value::<Args>(v).unwrap()).await
    }

    #[tokio::test]
    async fn full_flow_propose_delta_task_archive() {
        let (t, _d) = tool();
        call(
            &t,
            serde_json::json!({"action": "propose", "slug": "add-x", "why": "need x", "what": "add x"}),
        )
        .await
        .unwrap();

        call(
            &t,
            serde_json::json!({
                "action": "add_delta", "slug": "add-x", "op": "added",
                "capability": "xcap", "requirement": "Do X",
                "text": "The system SHALL do X.",
                "scenarios": [{"name": "s1", "when_then": "WHEN a THEN b"}]
            }),
        )
        .await
        .unwrap();

        let added = call(
            &t,
            serde_json::json!({"action": "add_task", "slug": "add-x", "text": "build it"}),
        )
        .await
        .unwrap();
        assert!(added.contains("(1.1)"), "auto-sequenced: {added}");

        // Archive refused while the task is open.
        let blocked = call(
            &t,
            serde_json::json!({"action": "archive", "slug": "add-x"}),
        )
        .await;
        assert!(blocked.is_err(), "archive must refuse open tasks");

        // Find the task id from status, mark it done.
        let status = call(&t, serde_json::json!({"action": "status", "slug": "add-x"}))
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&status).unwrap();
        let task_id = parsed["tasks"][0]["id"].as_i64().unwrap();
        call(
            &t,
            serde_json::json!({"action": "set_task", "task_id": task_id, "status": "done"}),
        )
        .await
        .unwrap();

        let archived = call(
            &t,
            serde_json::json!({"action": "archive", "slug": "add-x"}),
        )
        .await
        .unwrap();
        assert!(archived.contains("1 added"), "archive report: {archived}");

        // Living specs now contain the capability + requirement.
        let specs = call(
            &t,
            serde_json::json!({"action": "specs", "capability": "xcap"}),
        )
        .await
        .unwrap();
        assert!(specs.contains("Do X"));
        assert!(specs.contains("WHEN a THEN b"));
    }

    #[tokio::test]
    async fn missing_required_field_errors() {
        let (t, _d) = tool();
        assert!(
            call(&t, serde_json::json!({"action": "propose", "slug": "x"}))
                .await
                .is_err()
        );
        assert!(
            call(&t, serde_json::json!({"action": "bogus"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn archive_forms_a_memory_of_the_change() {
        use std::sync::Mutex;

        // Records add() calls so we can assert archive folds the change into
        // memory.
        struct RecordingMem(Mutex<Vec<String>>);
        impl MemoryProvider for RecordingMem {
            fn name(&self) -> &str {
                "rec"
            }
            fn view(&self, _t: &str) -> serde_json::Value {
                serde_json::json!({})
            }
            fn add(
                &self,
                _target: &str,
                content: &str,
                _kind: Option<&str>,
            ) -> Result<serde_json::Value, String> {
                self.0.lock().unwrap().push(content.to_string());
                Ok(serde_json::json!({}))
            }
            fn replace(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: Option<&str>,
            ) -> Result<serde_json::Value, String> {
                Ok(serde_json::json!({}))
            }
            fn remove(&self, _: &str, _: &str) -> Result<serde_json::Value, String> {
                Ok(serde_json::json!({}))
            }
        }

        let dir = std::env::temp_dir().join(format!("dirge-specmem-{}", std::process::id()));
        // dirge-m1ni: clear first — the name is keyed on a recyclable pid.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = SpecStore::open_at(&dir.join("state.db")).unwrap();
        let mem = Arc::new(RecordingMem(Mutex::new(Vec::new())));
        let t = SpecTool::new(Arc::new(store), None, None)
            .with_memory(Some(mem.clone() as Arc<dyn MemoryProvider>));

        call(
            &t,
            serde_json::json!({"action": "propose", "slug": "ship-it", "why": "users need it", "what": "the thing"}),
        )
        .await
        .unwrap();
        call(
            &t,
            serde_json::json!({"action": "set_field", "slug": "ship-it", "field": "design", "value": "use a queue"}),
        )
        .await
        .unwrap();
        // No tasks → archive allowed immediately.
        call(
            &t,
            serde_json::json!({"action": "archive", "slug": "ship-it"}),
        )
        .await
        .unwrap();

        let recorded = mem.0.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1, "exactly one memory formed on archive");
        assert!(recorded[0].contains("ship-it"));
        assert!(recorded[0].contains("users need it"));
        assert!(recorded[0].contains("use a queue"));

        // Re-archiving is refused (would otherwise re-fold the deltas).
        let again = call(
            &t,
            serde_json::json!({"action": "archive", "slug": "ship-it"}),
        )
        .await;
        assert!(again.is_err(), "second archive must be refused");
        assert_eq!(
            mem.0.lock().unwrap().len(),
            1,
            "refused re-archive forms no new memory"
        );
    }
}
