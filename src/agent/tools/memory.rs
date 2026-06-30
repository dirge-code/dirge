use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::extras::memory_provider::MemoryProvider;

pub struct MemoryTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    // dirge-bov5: dyn-dispatched provider so alternative backends
    // (vector store, MCP, remote sync) can plug in without churning
    // the call sites. `Arc<MemoryToolStore>` is the default and
    // coerces to this trait object via unsizing.
    store: Arc<dyn MemoryProvider>,
    /// Optional cross-project (global) memory tier. When present, an action
    /// with `scope: "global"` routes here instead of the per-project
    /// `store`; absent, a global request falls back to the project store.
    global_store: Option<Arc<dyn MemoryProvider>>,
    /// dirge-ygm3: gate the background-review actions (`mark`, `supersede`).
    /// These record procedural outcomes and contradiction supersessions that
    /// the background review pass infers from the transcript — the interactive
    /// agent must not self-grade or self-supersede. When `false` (the default,
    /// the main agent's instance) those actions are absent from the tool
    /// schema AND rejected at the call layer; the review runner gets a separate
    /// instance built with `true`.
    review_actions: bool,
}

impl MemoryTool {
    pub fn new(
        store: Arc<dyn MemoryProvider>,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            store,
            global_store: None,
            review_actions: false,
        }
    }

    /// Attach the global (cross-project) memory tier. `None` is a no-op.
    pub fn with_global(mut self, global_store: Option<Arc<dyn MemoryProvider>>) -> Self {
        self.global_store = global_store;
        self
    }

    /// Enable the background-review-only actions (`mark`/`supersede`). Set on
    /// the instance handed to the review runner, never the main agent's.
    pub fn with_review_actions(mut self, enabled: bool) -> Self {
        self.review_actions = enabled;
        self
    }

    /// Pick the store an action targets. `scope == "global"` selects the
    /// global tier when configured; everything else (including a global
    /// request with no global tier) uses the per-project store.
    fn scoped_store(&self, scope: Option<&str>) -> &Arc<dyn MemoryProvider> {
        match scope {
            Some("global") => self.global_store.as_ref().unwrap_or(&self.store),
            _ => &self.store,
        }
    }
}

#[derive(Deserialize)]
pub struct Args {
    action: String,
    #[serde(default = "default_target")]
    target: String,
    content: Option<String>,
    old_text: Option<String>,
    /// UMP memory kind (types.ts:8-13). One of: semantic, episodic,
    /// procedural, working, identity, overview. Defaults to "procedural".
    #[serde(default = "default_kind")]
    kind: Option<String>,
    /// Full-text query for the `search` action (dirge-q8wt).
    #[serde(default)]
    query: Option<String>,
    /// Outcome for the `mark` action (dirge-zygq): "success" or
    /// "failure". Records a procedural playbook's real-world result.
    #[serde(default)]
    outcome: Option<String>,
    /// Contradiction type for the `supersede` action (dirge-fa10):
    /// `true` when the user flatly denied the old fact (discounts the
    /// successor's confidence), `false`/absent for a natural update.
    #[serde(default)]
    harsh: Option<bool>,
    /// Memory scope: "project" (default) for facts about THIS repo, or
    /// "global" for durable cross-project user preferences that should
    /// follow the user everywhere.
    #[serde(default)]
    scope: Option<String>,
}

fn default_target() -> String {
    "memory".to_string()
}

fn default_kind() -> Option<String> {
    None
}

impl Tool for MemoryTool {
    const NAME: &'static str = "memory";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        // dirge-ygm3: `mark`/`supersede` are background-review-only. They live
        // in the action enum (parameters), not the prose description, so gating
        // them changes only the schema's allowed values — never the
        // length-checked description. The review runner's instance sets
        // `review_actions = true`; the main agent's leaves them out entirely.
        let mut actions = vec![
            "view", "add", "replace", "remove", "restore", "expand", "search",
        ];
        let mut action_desc = "The action to perform.".to_string();
        if self.review_actions {
            actions.push("mark");
            actions.push("supersede");
            action_desc.push_str(
                " 'mark' records a procedural playbook's outcome (old_text + \
                 outcome=success|failure). 'supersede' retires a contradicted fact \
                 (old_text) and writes a corrected one (content), keeping the old as \
                 an audit record — use it instead of 'replace' when a fact CHANGED \
                 rather than was reworded.",
            );
        }
        ToolDefinition {
            name: "memory".to_string(),
            description: r#"Persistent long-term memory for project facts and pitfalls.

SAVE WHEN: the user corrects you or says "remember this"; you discover build/test commands, conventions, architecture patterns, or library quirks; something was tried and failed (pitfall).

TARGETS: "memory" (facts, conventions, build, architecture), "pitfalls" (anti-patterns, things tried and failed).

KINDS (optional, default "procedural"): semantic (fact), episodic (event), procedural (rule), working (short-lived), identity (user/agent), overview (singular project orientation; adding one replaces it).

ACTIONS:
- view: inline entries + breadcrumb index for a target
- add: new entry (content)
- replace: update matched entry (old_text + content)
- remove: archive matched entry (old_text); restorable
- restore: un-archive a removed entry (old_text)
- expand: full text of one entry by id/substring (old_text)
- search: full-text search across all memory (query)

old_text matches a unique substring or the exact "urn:ump:…" id from view/index."#
                .to_string(),
            parameters: {
                let mut params = serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": actions,
                            "description": action_desc
                        },
                        "target": {
                            "type": "string",
                            "enum": ["memory", "pitfalls"],
                            "description": "Which memory store: 'memory' for project facts, 'pitfalls' for anti-patterns."
                        },
                        "content": {
                            "type": "string",
                            "description": "The entry content. Required for 'add' and 'replace'."
                        },
                        "old_text": {
                            "type": "string",
                            "description": "Short unique substring identifying the entry to replace, remove, restore, or expand — or the entry's exact 'urn:ump:…' id from view's meta / the breadcrumb index."
                        },
                        "query": {
                            "type": "string",
                            "description": "Full-text query for the 'search' action."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["semantic", "episodic", "procedural", "working", "identity", "overview"],
                            "description": "The UMP memory kind. Defaults to 'procedural'. See KINDS above."
                        },
                        "scope": {
                            "type": "string",
                            "enum": ["project", "global"],
                            "description": "Where the entry lives: 'project' (default) for facts about THIS repo; 'global' for durable user preferences that should follow the user across every project."
                        }
                    },
                    "required": ["action"]
                });
                // dirge-ygm3: the mark/supersede-only params travel with their
                // actions — present only on the review instance.
                if self.review_actions
                    && let Some(props) = params["properties"].as_object_mut()
                {
                    props.insert(
                        "outcome".to_string(),
                        serde_json::json!({
                            "type": "string",
                            "enum": ["success", "failure"],
                            "description": "For the 'mark' action: whether a procedural playbook worked ('success') or failed ('failure') in practice."
                        }),
                    );
                    props.insert(
                        "harsh".to_string(),
                        serde_json::json!({
                            "type": "boolean",
                            "description": "For the 'supersede' action: true when the user flatly DENIED the old fact (discounts the new fact's confidence); false/omitted for a natural update like a changed preference."
                        }),
                    );
                }
                params
            },
        }
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "memory", &args.action).await?;

        let target = validate_target(&args.target)?;
        // Route to the project or global tier per `scope` (default project).
        let store = self.scoped_store(args.scope.as_deref());

        // dirge-ygm3: defense in depth — `mark`/`supersede` are absent from
        // this instance's schema unless review_actions is on, but reject them
        // at the call layer too so a hand-crafted call can't reach them on the
        // interactive agent's tool.
        if !self.review_actions && matches!(args.action.as_str(), "mark" | "supersede") {
            return Err(ToolError::Msg(format!(
                "Action '{}' is only available to the background review pass.",
                args.action
            )));
        }

        match args.action.as_str() {
            "view" => {
                let resp = store.view(target);
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            // dirge-5feg: the tool layer fires `on_memory_write`
            // exactly once after each successful CRUD, regardless of
            // which provider impl handled the call. Providers are
            // forbidden from calling the hook themselves to avoid
            // double-firing through wrappers.
            //
            // dirge-ix7n: the third hook arg carries action-specific
            // semantics — `content` for add/replace, `old_text` for
            // remove. The trait doc on `on_memory_write` calls this
            // out as `payload` to avoid the "always a new value"
            // misreading.
            "add" => {
                let content = crate::agent::tools::required_nonblank(
                    args.content.as_deref(),
                    "content",
                    "add",
                )?;
                let resp = store
                    .add(target, content, args.kind.as_deref())
                    .map_err(ToolError::Msg)?;
                crate::agent::review::fire_memory_write(store.as_ref(), "add", target, content);
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            "replace" => {
                let old_text = crate::agent::tools::required_nonblank(
                    args.old_text.as_deref(),
                    "old_text",
                    "replace",
                )?;
                let content = crate::agent::tools::required_nonblank(
                    args.content.as_deref(),
                    "content",
                    "replace",
                )?;
                let resp = store
                    .replace(target, old_text, content, args.kind.as_deref())
                    .map_err(ToolError::Msg)?;
                crate::agent::review::fire_memory_write(store.as_ref(), "replace", target, content);
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            // supersede writes a NEW corrected entry, so the hook payload
            // is the new content (like add/replace), not the old_text.
            "supersede" => {
                let old_text = crate::agent::tools::required_nonblank(
                    args.old_text.as_deref(),
                    "old_text",
                    "supersede",
                )?;
                let content = crate::agent::tools::required_nonblank(
                    args.content.as_deref(),
                    "content",
                    "supersede",
                )?;
                let resp = store
                    .supersede(
                        target,
                        old_text,
                        content,
                        args.kind.as_deref(),
                        args.harsh.unwrap_or(false),
                    )
                    .map_err(ToolError::Msg)?;
                crate::agent::review::fire_memory_write(
                    store.as_ref(),
                    "supersede",
                    target,
                    content,
                );
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            "remove" => {
                let old_text = crate::agent::tools::required_nonblank(
                    args.old_text.as_deref(),
                    "old_text",
                    "remove",
                )?;
                let resp = store.remove(target, old_text).map_err(ToolError::Msg)?;
                crate::agent::review::fire_memory_write(store.as_ref(), "remove", target, old_text);
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            "restore" => {
                let old_text = crate::agent::tools::required_nonblank(
                    args.old_text.as_deref(),
                    "old_text",
                    "restore",
                )?;
                let resp = store.restore(target, old_text).map_err(ToolError::Msg)?;
                crate::agent::review::fire_memory_write(
                    store.as_ref(),
                    "restore",
                    target,
                    old_text,
                );
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            // expand/search are reads — no on_memory_write fire.
            // Both span targets, so `target` is ignored.
            "expand" => {
                let old_text = crate::agent::tools::required_nonblank(
                    args.old_text.as_deref(),
                    "old_text",
                    "expand",
                )?;
                let resp = store.expand(old_text).map_err(ToolError::Msg)?;
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            "search" => {
                let query = crate::agent::tools::required_nonblank(
                    args.query.as_deref(),
                    "query",
                    "search",
                )?;
                // dirge-4hld: search can do blocking work — SQLite I/O for the
                // builtin store, and a network embedding round-trip for the
                // hybrid provider. Off-load to the blocking pool so it never
                // parks an async runtime worker.
                let store = store.clone();
                let query = query.to_string();
                let resp = tokio::task::spawn_blocking(move || store.search(&query))
                    .await
                    .map_err(|e| ToolError::Msg(format!("memory search task failed: {e}")))?
                    .map_err(ToolError::Msg)?;
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            // mark records an outcome signal (dirge-zygq), not a content
            // CRUD — like expand it mutates a usage counter, so no
            // on_memory_write fire.
            "mark" => {
                let old_text = crate::agent::tools::required_nonblank(
                    args.old_text.as_deref(),
                    "old_text",
                    "mark",
                )?;
                let outcome = crate::agent::tools::required_nonblank(
                    args.outcome.as_deref(),
                    "outcome",
                    "mark",
                )?;
                let success = match outcome {
                    "success" => true,
                    "failure" => false,
                    other => {
                        return Err(ToolError::Msg(format!(
                            "Invalid outcome '{other}'. Use 'success' or 'failure'."
                        )));
                    }
                };
                let resp = store
                    .record_outcome(target, old_text, success)
                    .map_err(ToolError::Msg)?;
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            _ => Err(ToolError::Msg(format!(
                "Unknown action '{}'. Use: view, add, replace, remove, restore, expand, search, mark, supersede.",
                args.action
            ))),
        }
    }
}

fn validate_target(target: &str) -> Result<&str, ToolError> {
    match target {
        "memory" | "pitfalls" => Ok(target),
        _ => Err(ToolError::Msg(format!(
            "Invalid target '{}'. Use 'memory' or 'pitfalls'.",
            target
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::dirge_paths::ProjectPaths;
    use crate::extras::memory_db::SqliteMemoryStore;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_store() -> (Arc<dyn MemoryProvider>, std::path::PathBuf) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("dirge-mem-tool-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let paths = ProjectPaths::new(&dir);
        let store: Arc<dyn MemoryProvider> = Arc::new(SqliteMemoryStore::load(&paths).unwrap());
        (store, dir)
    }

    fn make_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    /// Minimal `Args` with everything but `action` defaulted.
    fn action_args(action: &str) -> Args {
        Args {
            action: action.into(),
            target: "memory".into(),
            content: None,
            old_text: None,
            kind: None,
            query: None,
            outcome: None,
            harsh: None,
            scope: None,
        }
    }

    fn action_enum(tool: &MemoryTool, rt: &tokio::runtime::Runtime) -> Vec<String> {
        let def = rt.block_on(tool.definition(String::new()));
        def.parameters["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    /// dirge-ygm3: the main agent's instance (default) must not advertise the
    /// background-review actions in its schema.
    #[test]
    fn default_tool_hides_review_actions_from_schema() {
        let (store, _d) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();
        let actions = action_enum(&tool, &rt);
        assert!(
            !actions.iter().any(|a| a == "mark" || a == "supersede"),
            "main agent schema must omit mark/supersede: {actions:?}",
        );
        // The base actions are still all present.
        for a in [
            "view", "add", "replace", "remove", "restore", "expand", "search",
        ] {
            assert!(actions.iter().any(|x| x == a), "missing base action {a}");
        }
    }

    /// The review runner's instance exposes mark/supersede.
    #[test]
    fn review_tool_exposes_review_actions() {
        let (store, _d) = temp_store();
        let tool = MemoryTool::new(store, None, None).with_review_actions(true);
        let rt = make_runtime();
        let actions = action_enum(&tool, &rt);
        assert!(
            actions.iter().any(|a| a == "mark"),
            "mark present: {actions:?}"
        );
        assert!(
            actions.iter().any(|a| a == "supersede"),
            "supersede present: {actions:?}",
        );
    }

    /// Defense in depth: even a hand-crafted call is rejected on the main
    /// agent's (non-review) instance.
    #[test]
    fn default_tool_rejects_review_actions_at_call_layer() {
        let (store, _d) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();
        for action in ["mark", "supersede"] {
            let err = rt
                .block_on(tool.call(action_args(action)))
                .expect_err("must be rejected");
            let msg = format!("{err:?}");
            assert!(
                msg.contains("background review"),
                "reject reason names the gate: {msg}",
            );
        }
    }

    /// The review instance can actually record an outcome.
    #[test]
    fn review_tool_accepts_mark() {
        let (store, _d) = temp_store();
        store
            .add("memory", "run cargo fmt before commit", Some("procedural"))
            .unwrap();
        let tool = MemoryTool::new(store, None, None).with_review_actions(true);
        let rt = make_runtime();
        let mut args = action_args("mark");
        args.old_text = Some("cargo fmt".into());
        args.outcome = Some("success".into());
        let out = rt
            .block_on(tool.call(args))
            .expect("mark succeeds on review tool");
        assert!(out.contains("success"), "outcome recorded: {out}");
    }

    /// `scope: "global"` routes the write to the global tier, leaving the
    /// project store untouched. A second independent store stands in for
    /// the global tier.
    #[test]
    fn scope_global_routes_to_the_global_store() {
        let (project, _pd) = temp_store();
        let (global, _gd) = temp_store();
        let tool = MemoryTool::new(project.clone(), None, None).with_global(Some(global.clone()));
        let rt = make_runtime();

        rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("user prefers TDD".into()),
            old_text: None,
            kind: None,
            query: None,
            scope: Some("global".into()),
            outcome: None,
            harsh: None,
        }))
        .expect("global add should succeed");

        assert!(
            global
                .view("memory")
                .to_string()
                .contains("user prefers TDD"),
            "the entry must land in the global store"
        );
        assert!(
            !project
                .view("memory")
                .to_string()
                .contains("user prefers TDD"),
            "the project store must be untouched"
        );
    }

    #[test]
    fn test_add_and_view() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        // Add an entry.
        let result = rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("build command: cargo build --release".into()),
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }));
        assert!(result.is_ok(), "add failed: {:?}", result);
        let resp: serde_json::Value = serde_json::from_str(&result.unwrap()).expect("valid JSON");
        assert_eq!(resp["success"], true);
        assert_eq!(resp["entry_count"], 1);

        // View — should see the entry.
        let result = rt.block_on(tool.call(Args {
            action: "view".into(),
            target: "memory".into(),
            content: None,
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }));
        let resp: serde_json::Value = serde_json::from_str(&result.unwrap()).expect("valid JSON");
        let entries = resp["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].as_str().unwrap().contains("cargo build"));
    }

    #[test]
    fn test_add_to_pitfalls() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "pitfalls".into(),
            content: Some("Don't use async in the render loop".into()),
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }));
        assert!(result.is_ok());
        let resp: serde_json::Value = serde_json::from_str(&result.unwrap()).expect("valid JSON");
        assert_eq!(resp["target"], "pitfalls");
    }

    #[test]
    fn test_duplicate_rejected() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("same entry".into()),
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }))
        .unwrap();

        let result = rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("same entry".into()),
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_replace_by_substring() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("build command: cargo build".into()),
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }))
        .unwrap();

        let result = rt.block_on(tool.call(Args {
            action: "replace".into(),
            target: "memory".into(),
            content: Some("build command: cargo build --release".into()),
            old_text: Some("cargo build".into()),
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }));
        let resp: serde_json::Value = serde_json::from_str(&result.unwrap()).expect("valid JSON");
        assert_eq!(resp["success"], true);

        // Verify the entry was replaced.
        let result = rt.block_on(tool.call(Args {
            action: "view".into(),
            target: "memory".into(),
            content: None,
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }));
        let resp: serde_json::Value = serde_json::from_str(&result.unwrap()).expect("valid JSON");
        let entries = resp["entries"].as_array().unwrap();
        assert!(entries[0].as_str().unwrap().contains("--release"));
    }

    #[test]
    fn test_remove_entry() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("temp entry to remove".into()),
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }))
        .unwrap();

        let result = rt.block_on(tool.call(Args {
            action: "remove".into(),
            target: "memory".into(),
            content: None,
            old_text: Some("temp entry".into()),
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }));
        let resp: serde_json::Value = serde_json::from_str(&result.unwrap()).expect("valid JSON");
        assert_eq!(resp["success"], true);

        // Verify empty.
        let result = rt.block_on(tool.call(Args {
            action: "view".into(),
            target: "memory".into(),
            content: None,
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }));
        let resp: serde_json::Value = serde_json::from_str(&result.unwrap()).expect("valid JSON");
        assert_eq!(resp["entry_count"], 0);
    }

    #[test]
    fn test_invalid_target_rejected() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(Args {
            action: "view".into(),
            target: "user".into(),
            content: None,
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid target"));
    }

    #[test]
    fn test_missing_content_for_add() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: None,
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_definition_includes_both_targets() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();
        let def = rt.block_on(tool.definition(String::new()));
        assert!(def.description.contains("memory"));
        assert!(def.description.contains("pitfalls"));
    }

    /// dirge-bov5 — `MemoryTool` routes through the `MemoryProvider`
    /// trait so an alternative backend (vector store, MCP-backed,
    /// etc.) receives every call. Verifies a custom recording
    /// provider sees both writes and reads.
    #[test]
    fn integration_tool_routes_calls_through_custom_provider() {
        use crate::extras::memory_provider::MemoryProvider;
        use serde_json::json;
        use std::sync::Mutex;

        #[derive(Default)]
        struct RecordingProvider {
            calls: Mutex<Vec<String>>,
        }
        impl MemoryProvider for RecordingProvider {
            fn name(&self) -> &str {
                "recording"
            }
            fn view(&self, target: &str) -> serde_json::Value {
                self.calls.lock().unwrap().push(format!("view:{}", target));
                json!({ "entries": [], "count": 0 })
            }
            fn add(
                &self,
                target: &str,
                content: &str,
                _kind: Option<&str>,
            ) -> Result<serde_json::Value, String> {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("add:{}:{}", target, content));
                Ok(json!({ "success": true, "entry_count": 1 }))
            }
            fn replace(
                &self,
                target: &str,
                old: &str,
                content: &str,
                _kind: Option<&str>,
            ) -> Result<serde_json::Value, String> {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("replace:{}:{}:{}", target, old, content));
                Ok(json!({ "success": true }))
            }
            fn remove(&self, target: &str, old: &str) -> Result<serde_json::Value, String> {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("remove:{}:{}", target, old));
                Ok(json!({ "success": true }))
            }
        }

        let provider = Arc::new(RecordingProvider::default());
        let tool = MemoryTool::new(provider.clone() as Arc<dyn MemoryProvider>, None, None);
        let rt = make_runtime();

        rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("from-tool".into()),
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }))
        .unwrap();
        rt.block_on(tool.call(Args {
            action: "view".into(),
            target: "memory".into(),
            content: None,
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }))
        .unwrap();
        rt.block_on(tool.call(Args {
            action: "replace".into(),
            target: "memory".into(),
            content: Some("new".into()),
            old_text: Some("from-tool".into()),
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }))
        .unwrap();
        rt.block_on(tool.call(Args {
            action: "remove".into(),
            target: "memory".into(),
            content: None,
            old_text: Some("new".into()),
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }))
        .unwrap();

        let calls = provider.calls.lock().unwrap();
        assert_eq!(
            *calls,
            vec![
                "add:memory:from-tool".to_string(),
                "view:memory".to_string(),
                "replace:memory:from-tool:new".to_string(),
                "remove:memory:new".to_string(),
            ],
            "custom provider must receive every tool call verbatim"
        );
    }

    /// dirge-5feg — the tool layer fires `on_memory_write` exactly
    /// once per successful CRUD, regardless of whether the
    /// provider's CRUD impl self-fired. `view` does NOT fire the
    /// hook (it's not a write).
    #[test]
    fn integration_tool_layer_fires_on_memory_write_once_per_crud() {
        use crate::extras::memory_provider::MemoryProvider;
        use serde_json::json;
        use std::sync::Mutex;

        #[derive(Default)]
        struct RecordingHookProvider {
            hooks: Mutex<Vec<(String, String, String)>>,
        }
        impl MemoryProvider for RecordingHookProvider {
            fn name(&self) -> &str {
                "hook-recorder"
            }
            // CRUD impls deliberately do NOT call on_memory_write —
            // the tool layer is supposed to.
            fn view(&self, _: &str) -> serde_json::Value {
                json!({ "entries": [] })
            }
            fn add(
                &self,
                _: &str,
                _: &str,
                _kind: Option<&str>,
            ) -> Result<serde_json::Value, String> {
                Ok(json!({ "success": true }))
            }
            fn replace(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _kind: Option<&str>,
            ) -> Result<serde_json::Value, String> {
                Ok(json!({ "success": true }))
            }
            fn remove(&self, _: &str, _: &str) -> Result<serde_json::Value, String> {
                Ok(json!({ "success": true }))
            }
            fn on_memory_write(&self, action: &str, target: &str, content: &str) {
                self.hooks
                    .lock()
                    .unwrap()
                    .push((action.into(), target.into(), content.into()));
            }
        }

        let provider = Arc::new(RecordingHookProvider::default());
        let tool = MemoryTool::new(provider.clone() as Arc<dyn MemoryProvider>, None, None);
        let rt = make_runtime();

        // view does NOT fire the hook.
        rt.block_on(tool.call(Args {
            action: "view".into(),
            target: "memory".into(),
            content: None,
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }))
        .unwrap();
        assert!(
            provider.hooks.lock().unwrap().is_empty(),
            "view must not fire on_memory_write"
        );

        // add → one fire with the content.
        rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("alpha".into()),
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }))
        .unwrap();
        // replace → one fire with the new content.
        rt.block_on(tool.call(Args {
            action: "replace".into(),
            target: "memory".into(),
            content: Some("beta".into()),
            old_text: Some("alpha".into()),
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }))
        .unwrap();
        // remove → one fire with the old_text (no new content).
        rt.block_on(tool.call(Args {
            action: "remove".into(),
            target: "pitfalls".into(),
            content: None,
            old_text: Some("beta".into()),
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }))
        .unwrap();

        let hooks = provider.hooks.lock().unwrap();
        assert_eq!(
            *hooks,
            vec![
                ("add".into(), "memory".into(), "alpha".into()),
                ("replace".into(), "memory".into(), "beta".into()),
                ("remove".into(), "pitfalls".into(), "beta".into()),
            ],
            "tool layer must fire on_memory_write exactly once per CRUD"
        );
    }

    /// End-to-end: every action the SYSTEM_PROMPT names for the memory
    /// tool must succeed against a real MemoryTool with valid args.
    /// If this fails, the prompt is lying to the model. See dirge-yqmo.
    #[test]
    fn integration_prompt_actions_all_executable() {
        use crate::agent::prompt::SYSTEM_PROMPT;

        let memory_line = SYSTEM_PROMPT
            .lines()
            .find(|l| l.trim_start().starts_with("- memory:"))
            .expect("SYSTEM_PROMPT should describe the memory tool");

        // Extract candidate action words from the prompt.
        let known_actions = [
            "view", "add", "replace", "remove", "restore", "expand", "search",
        ];
        let prompt_actions: Vec<&str> = known_actions
            .iter()
            .copied()
            .filter(|a| {
                memory_line
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|w| w == *a)
            })
            .collect();
        assert_eq!(
            prompt_actions.len(),
            known_actions.len(),
            "prompt should list all real actions; got {:?}",
            prompt_actions
        );

        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        // Seed an entry so replace/remove have something to match.
        rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("seed: build command cargo test".into()),
            old_text: None,
            kind: None,
            query: None,
            scope: None,
            outcome: None,
            harsh: None,
        }))
        .expect("seed add should succeed");

        for action in &prompt_actions {
            let args = match *action {
                "view" => Args {
                    action: "view".into(),
                    target: "memory".into(),
                    content: None,
                    old_text: None,
                    kind: None,
                    query: None,
                    scope: None,
                    outcome: None,
                    harsh: None,
                },
                "add" => Args {
                    action: "add".into(),
                    target: "memory".into(),
                    content: Some(format!("entry-for-{}", action)),
                    old_text: None,
                    kind: None,
                    query: None,
                    scope: None,
                    outcome: None,
                    harsh: None,
                },
                "replace" => Args {
                    action: "replace".into(),
                    target: "memory".into(),
                    content: Some("seed: build command cargo test --release".into()),
                    old_text: Some("seed:".into()),
                    kind: None,
                    query: None,
                    scope: None,
                    outcome: None,
                    harsh: None,
                },
                "remove" => Args {
                    action: "remove".into(),
                    target: "memory".into(),
                    content: None,
                    old_text: Some("entry-for-add".into()),
                    kind: None,
                    query: None,
                    scope: None,
                    outcome: None,
                    harsh: None,
                },
                // Runs after "remove" archived entry-for-add, so the
                // restore has a tombstoned entry to revive.
                "restore" => Args {
                    action: "restore".into(),
                    target: "memory".into(),
                    content: None,
                    old_text: Some("entry-for-add".into()),
                    kind: None,
                    query: None,
                    scope: None,
                    outcome: None,
                    harsh: None,
                },
                // Runs after "restore", so entry-for-add is active.
                "expand" => Args {
                    action: "expand".into(),
                    target: "memory".into(),
                    content: None,
                    old_text: Some("entry-for-add".into()),
                    kind: None,
                    query: None,
                    scope: None,
                    outcome: None,
                    harsh: None,
                },
                "search" => Args {
                    action: "search".into(),
                    target: "memory".into(),
                    content: None,
                    old_text: None,
                    kind: None,
                    query: Some("seed".into()),
                    scope: None,
                    outcome: None,
                    harsh: None,
                },
                _ => unreachable!(),
            };
            let result = rt.block_on(tool.call(args));
            assert!(
                result.is_ok(),
                "prompt-advertised action '{}' failed end-to-end: {:?}",
                action,
                result
            );
        }
    }
}
