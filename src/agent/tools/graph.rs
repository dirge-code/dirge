//! Agent-callable graph tool (#393, Task 18).
//!
//! Exposes the entity/relation graph for programmatic model access.
//! Two actions: `search_graph` (FTS5) and `traverse_graph` (recursive CTE).
//! Feature-gated behind `experimental-graph-search`.
//!
//! The tool description tells the model when to use it:
//! "Search the entity/relation graph for files, errors, commits,
//!  and their relationships. Use when reasoning about code structure
//!  across turns."

use std::path::PathBuf;

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};

pub struct GraphTool {
    db_path: PathBuf,
    current_session_id: Option<String>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
}

impl GraphTool {
    pub fn new(
        db_path: PathBuf,
        current_session_id: Option<String>,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        Self {
            db_path,
            current_session_id,
            permission,
            ask_tx,
        }
    }
}

#[derive(Deserialize)]
pub struct GraphArgs {
    action: Option<String>,
    query: Option<String>,
    kind: Option<String>,
    limit: Option<usize>,
    entity_id: Option<i64>,
    depth: Option<u32>,
    compress: Option<bool>,
}

impl Tool for GraphTool {
    const NAME: &'static str = "search_graph";

    type Error = ToolError;
    type Args = GraphArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "search_graph".to_string(),
            description: concat!(
                "Search the entity/relation graph for files, errors, commits, ",
                "and their relationships. Use when reasoning about code structure ",
                "across turns.\n\n",
                "Two actions:\n",
                "- `search_graph`: FTS5 search over entities by name and kind\n",
                "- `traverse_graph`: recursive CTE traversal from an entity ID, ",
                "returning typed relation paths\n\n",
                "Entities are facts extracted from tool output: files, errors, ",
                "commits, and other structured items with typed relationships ",
                "between them.",
            )
            .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "search_graph (FTS5 search) or traverse_graph (CTE traversal from entity)"
                    },
                    "query": {
                        "type": "string",
                        "description": "FTS5 search query for search_graph (e.g. 'E0308', 'src/main.rs')"
                    },
                    "kind": {
                        "type": "string",
                        "description": "Optional entity kind filter for search_graph (e.g. 'file', 'error', 'commit')"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max results for search_graph (default 10)"
                    },
                    "entity_id": {
                        "type": "integer",
                        "description": "Entity ID seed for traverse_graph"
                    },
                    "depth": {
                        "type": "integer",
                        "description": "Max traversal depth for traverse_graph (default 2)"
                    },
                    "compress": {
                        "type": "boolean",
                        "description": "Compress traversal output into kind-grouped summary (default false)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn call(&self, args: GraphArgs) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "search_graph", "read").await?;

        let action = args.action.as_deref().unwrap_or("search_graph");

        let db = crate::extras::session_db::SessionDb::open(&self.db_path)
            .map_err(|e| ToolError::Msg(format!("graph: failed to open DB: {e}")))?;

        match action {
            "search_graph" => {
                let query = args.query.as_deref().unwrap_or("");
                if query.trim().is_empty() {
                    return Err(ToolError::Msg(
                        "`query` is required for action 'search_graph'".into(),
                    ));
                }
                let kind_filter = args.kind.as_deref();
                let limit = args.limit.unwrap_or(10).min(50);
                let results = crate::extras::entity_db::search_entities(
                    &db.conn,
                    query,
                    kind_filter,
                    self.current_session_id.as_deref(),
                    limit,
                )
                .map_err(ToolError::Msg)?;

                let json_rows: Vec<serde_json::Value> = results
                    .iter()
                    .map(|row| {
                        let m = crate::extras::entity_db::EntityMatch::from_row(row.clone(), None);
                        serde_json::json!({
                            "id": m.id,
                            "session_id": m.session_id,
                            "kind": m.kind,
                            "name": m.name,
                            "extra": m.extra,
                            "created_at": m.created_at,
                        })
                    })
                    .collect();

                Ok(
                    serde_json::to_string_pretty(&json_rows)
                        .unwrap_or_else(|_| r#"[]"#.to_string()),
                )
            }

            "traverse_graph" => {
                let seed_id = args.entity_id.ok_or_else(|| {
                    ToolError::Msg("`entity_id` is required for action 'traverse_graph'".into())
                })?;
                let depth = args.depth.unwrap_or(2).min(5);
                let do_compress = args.compress.unwrap_or(false);

                let trace =
                    crate::extras::entity_search::traverse_from(&db.conn, &[seed_id], depth, None)
                        .map_err(ToolError::Msg)?;

                if do_compress {
                    let compressed =
                        crate::extras::entity_compress::compress_bundle(&db.conn, &trace, "")
                            .map_err(ToolError::Msg)?;
                    Ok(compressed)
                } else {
                    let json_rows: Vec<serde_json::Value> = trace
                        .iter()
                        .map(|(id, path, d)| {
                            serde_json::json!({
                                "entity_id": id,
                                "path": path,
                                "depth": d,
                            })
                        })
                        .collect();
                    Ok(serde_json::to_string_pretty(&json_rows)
                        .unwrap_or_else(|_| r#"[]"#.to_string()))
                }
            }

            _ => Err(ToolError::Msg(format!(
                "unknown action '{}'. Use 'search_graph' or 'traverse_graph'",
                action
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::session_db::SessionDb;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_db() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "dirge-graph-tool-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.db")
    }

    fn make_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    fn setup_test_db(db_path: &PathBuf, session_id: &str) {
        let db = SessionDb::open(db_path).unwrap();
        crate::extras::entity_search::tests::setup_graph(&db.conn);
        // setup_graph inserts session 'ts' — add our test session
        db.conn
            .execute(
                "INSERT OR IGNORE INTO sessions (id, started_at, last_active)
             VALUES (?1, datetime('now'), datetime('now'))",
                rusqlite::params![session_id],
            )
            .unwrap();

        use crate::extras::entity_db;
        let e1 =
            entity_db::upsert_entity(&db.conn, session_id, None, "error", "E0308", None).unwrap();
        let e2 = entity_db::upsert_entity(&db.conn, session_id, None, "file", "src/main.rs", None)
            .unwrap();
        let _ = entity_db::insert_relation(&db.conn, e1, e2, "occurred_in", session_id).unwrap();
    }

    #[test]
    fn test_definition_includes_actions() {
        let tool = GraphTool::new(temp_db(), None, None, None);
        let rt = make_runtime();
        let def = rt.block_on(tool.definition(String::new()));
        assert!(def.description.contains("search_graph"));
        assert!(def.description.contains("traverse_graph"));
        assert!(def.description.contains("entity/relation"));
    }

    #[test]
    fn test_search_graph_finds_entity() {
        let db_path = temp_db();
        setup_test_db(&db_path, "test-session");
        let tool = GraphTool::new(db_path, None, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(GraphArgs {
            action: Some("search_graph".into()),
            query: Some("E0308".into()),
            kind: None,
            limit: None,
            entity_id: None,
            depth: None,
            compress: None,
        }));
        assert!(result.is_ok(), "search_graph failed: {:?}", result.err());
        let output = result.unwrap();
        assert!(output.contains("E0308"), "should find E0308: {output}");
    }

    #[test]
    fn test_search_graph_empty_for_no_match() {
        let db_path = temp_db();
        setup_test_db(&db_path, "test-session");
        let tool = GraphTool::new(db_path, None, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(GraphArgs {
            action: Some("search_graph".into()),
            query: Some("zzz_nonexistent_xyz".into()),
            kind: None,
            limit: None,
            entity_id: None,
            depth: None,
            compress: None,
        }));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.trim(), "[]");
    }

    #[test]
    fn test_traverse_graph_returns_paths() {
        let db_path = temp_db();
        setup_test_db(&db_path, "test-session");
        let tool = GraphTool::new(db_path, None, None, None);
        let rt = make_runtime();

        let search_result = rt
            .block_on(tool.call(GraphArgs {
                action: Some("search_graph".into()),
                query: Some("E0308".into()),
                kind: None,
                limit: None,
                entity_id: None,
                depth: None,
                compress: None,
            }))
            .unwrap();
        let results: Vec<serde_json::Value> = serde_json::from_str(&search_result).unwrap();
        let eid = results[0]["id"].as_i64().unwrap();

        let result = rt.block_on(tool.call(GraphArgs {
            action: Some("traverse_graph".into()),
            query: None,
            kind: None,
            limit: None,
            entity_id: Some(eid),
            depth: Some(2),
            compress: None,
        }));
        assert!(result.is_ok(), "traverse_graph failed: {:?}", result.err());
        let output = result.unwrap();
        assert!(
            output.contains("src/main.rs"),
            "should find the file: {output}"
        );
        assert!(
            output.contains("occurred_in"),
            "should show rel_type: {output}"
        );
    }

    #[test]
    fn test_traverse_graph_with_compress() {
        let db_path = temp_db();
        setup_test_db(&db_path, "test-session");
        let tool = GraphTool::new(db_path, None, None, None);
        let rt = make_runtime();

        let search_result = rt
            .block_on(tool.call(GraphArgs {
                action: Some("search_graph".into()),
                query: Some("E0308".into()),
                kind: None,
                limit: None,
                entity_id: None,
                depth: None,
                compress: None,
            }))
            .unwrap();
        let results: Vec<serde_json::Value> = serde_json::from_str(&search_result).unwrap();
        let eid = results[0]["id"].as_i64().unwrap();

        let result = rt.block_on(tool.call(GraphArgs {
            action: Some("traverse_graph".into()),
            query: None,
            kind: None,
            limit: None,
            entity_id: Some(eid),
            depth: Some(2),
            compress: Some(true),
        }));
        assert!(
            result.is_ok(),
            "traverse_graph compress failed: {:?}",
            result.err()
        );
        let output = result.unwrap();
        assert!(
            output.contains("entities:"),
            "compressed should have header: {output}"
        );
        assert!(
            !output.contains("\"path\":"),
            "compressed should not be JSON: {output}"
        );
    }

    #[test]
    fn test_invalid_action() {
        let tool = GraphTool::new(temp_db(), None, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(GraphArgs {
            action: Some("bogus_action".into()),
            query: None,
            kind: None,
            limit: None,
            entity_id: None,
            depth: None,
            compress: None,
        }));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown action"),
            "expected unknown action error: {err}"
        );
    }

    #[test]
    fn test_search_graph_missing_query() {
        let tool = GraphTool::new(temp_db(), None, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(GraphArgs {
            action: Some("search_graph".into()),
            query: None,
            kind: None,
            limit: None,
            entity_id: None,
            depth: None,
            compress: None,
        }));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("query"), "expected missing query error: {err}");
    }

    #[test]
    fn test_traverse_graph_missing_entity_id() {
        let tool = GraphTool::new(temp_db(), None, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(GraphArgs {
            action: Some("traverse_graph".into()),
            query: None,
            kind: None,
            limit: None,
            entity_id: None,
            depth: None,
            compress: None,
        }));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("entity_id"),
            "expected missing entity_id error: {err}"
        );
    }
}
