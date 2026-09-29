//! `memory.provider = "mcp"`: a [`MemoryProvider`] whose operations are tool
//! calls on a configured MCP server.
//!
//! Nothing here knows any particular server. `memory.mcp.operations` maps
//! each trait operation to a tool name, an argument template and an optional
//! JSON pointer into the result; this module fills the template, makes the
//! call and hands back the selected part. An operation the table leaves out
//! is refused with an error naming the missing entry: memory the user sent
//! to a server never lands in the builtin store instead.
//!
//! The trait is synchronous and is called both from async code and from the
//! blocking pool, while MCP is async. Calls therefore run on a dedicated
//! runtime owned by this module, with the caller waiting on a channel for a
//! bounded time, the same arrangement as the hybrid embedder. That runtime
//! also owns the connections: one per server name, opened on the first call
//! and reused across agent rebuilds, reopened after a transport failure.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use super::memory_provider::MemoryProvider;
use crate::config::{McpMemoryConfig, McpMemoryOperation, McpMemoryOperations};
#[allow(unused_imports)]
use crate::sync_util::LockExt;

/// Makes one tool call on the memory server. The seam the tests replace.
pub trait McpToolCaller: Send + Sync {
    /// Call `tool` with `arguments`. `Ok` is the tool's result as JSON: its
    /// structured content, else its text parsed as JSON, else the text as a
    /// string. A tool that reports an error is an `Err` carrying its text.
    fn call(&self, tool: &str, arguments: Map<String, Value>) -> Result<Value, String>;
}

/// Placeholder names an argument template may use.
const VARIABLES: &[&str] = &[
    "target",
    "content",
    "kind",
    "old_text",
    "query",
    "harsh",
    "success",
    "scope",
    "project",
    "project_root",
];

/// The values one operation fills its template with. Absent ones drop the
/// argument (whole-string placeholder) or read as empty (embedded).
#[derive(Default)]
struct Vars<'a> {
    target: Option<&'a str>,
    content: Option<&'a str>,
    kind: Option<&'a str>,
    old_text: Option<&'a str>,
    query: Option<&'a str>,
    harsh: Option<bool>,
    success: Option<bool>,
}

/// Which store the provider serves, as templates see it.
#[derive(Debug, Clone)]
pub struct Scope {
    /// `"project"` or `"global"`.
    pub name: &'static str,
    /// The project root; `None` for the global store.
    pub project_root: Option<std::path::PathBuf>,
}

/// Memory served by tool calls on an MCP server.
pub struct McpMemoryProvider {
    server: String,
    operations: McpMemoryOperations,
    scope: Scope,
    caller: Arc<dyn McpToolCaller>,
    /// The system-prompt text, fetched on first use and kept until
    /// [`MemoryProvider::refresh_snapshot`], so a frozen preamble does not
    /// cost a round-trip per read.
    prompt: Mutex<Option<String>>,
}

impl McpMemoryProvider {
    /// A provider over `caller`. Refuses an argument template that names an
    /// unknown placeholder, or a result pointer that is not a JSON pointer,
    /// so a typo fails at startup rather than on the model's first write.
    pub fn new(
        cfg: &McpMemoryConfig,
        scope: Scope,
        caller: Arc<dyn McpToolCaller>,
    ) -> Result<Self, String> {
        for (op, spec) in named_operations(&cfg.operations) {
            let Some(spec) = spec else { continue };
            if spec.tool.trim().is_empty() {
                return Err(format!("memory.mcp.operations.{op}.tool is empty"));
            }
            validate_template(&Value::Object(spec.arguments.clone()))
                .map_err(|e| format!("memory.mcp.operations.{op}.arguments: {e}"))?;
            if let Some(pointer) = &spec.result
                && !pointer.is_empty()
                && !pointer.starts_with('/')
            {
                return Err(format!(
                    "memory.mcp.operations.{op}.result {pointer:?} is not a JSON pointer \
                     (it must start with `/`)"
                ));
            }
        }
        Ok(Self {
            server: cfg.server.clone(),
            operations: cfg.operations.clone(),
            scope,
            caller,
            prompt: Mutex::new(None),
        })
    }

    /// Run operation `op`: fill its template, call its tool, select its
    /// result. An operation the config leaves out is an error.
    fn run(
        &self,
        op: &str,
        spec: Option<&McpMemoryOperation>,
        vars: &Vars<'_>,
    ) -> Result<Value, String> {
        let spec = spec.ok_or_else(|| {
            format!(
                "memory.provider \"mcp\" (server {:?}) has no `{op}` operation; \
                 add memory.mcp.operations.{op} to serve it",
                self.server
            )
        })?;
        let arguments = match fill(&Value::Object(spec.arguments.clone()), &|name| {
            self.lookup(name, vars)
        }) {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };
        let result = self
            .caller
            .call(&spec.tool, arguments)
            .map_err(|e| format!("memory {op} via MCP {}::{}: {e}", self.server, spec.tool))?;
        select(result, spec.result.as_deref()).ok_or_else(|| {
            format!(
                "memory {op} via MCP {}::{}: the result has nothing at {:?}",
                self.server,
                spec.tool,
                spec.result.as_deref().unwrap_or_default()
            )
        })
    }

    fn lookup(&self, name: &str, vars: &Vars<'_>) -> Option<Value> {
        let text = |v: Option<&str>| v.map(|s| Value::String(s.to_string()));
        match name {
            "target" => text(vars.target),
            "content" => text(vars.content),
            "kind" => text(vars.kind),
            "old_text" => text(vars.old_text),
            "query" => text(vars.query),
            "harsh" => vars.harsh.map(Value::Bool),
            "success" => vars.success.map(Value::Bool),
            "scope" => Some(Value::String(self.scope.name.to_string())),
            "project_root" => self
                .scope
                .project_root
                .as_ref()
                .map(|p| Value::String(p.display().to_string())),
            "project" => self
                .scope
                .project_root
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| Value::String(n.to_string_lossy().into_owned())),
            _ => None,
        }
    }

    fn fetch_prompt(&self) -> Result<String, String> {
        let value = self.run("prompt", self.operations.prompt.as_ref(), &Vars::default())?;
        Ok(match value {
            Value::Null => String::new(),
            Value::String(s) => s,
            other => serde_json::to_string_pretty(&other).unwrap_or_default(),
        })
    }
}

impl MemoryProvider for McpMemoryProvider {
    fn name(&self) -> &str {
        "mcp"
    }

    /// The `prompt` operation's text, injected verbatim. Without a `prompt`
    /// operation nothing is injected; a failed fetch is logged and injects
    /// nothing, and is retried on the next read.
    fn format_for_system_prompt(&self) -> String {
        if self.operations.prompt.is_none() {
            return String::new();
        }
        let mut cached = self.prompt.lock_ignore_poison();
        if let Some(text) = cached.as_ref() {
            return text.clone();
        }
        match self.fetch_prompt() {
            Ok(text) => {
                let text = if text.trim().is_empty() {
                    String::new()
                } else {
                    format!("\n{}\n", text.trim_end())
                };
                *cached = Some(text.clone());
                text
            }
            Err(e) => {
                tracing::warn!(target: "dirge::memory", error = %e, "MCP memory prompt fetch failed");
                String::new()
            }
        }
    }

    /// An array result is wrapped as `{entries, count}`, the builtin shape;
    /// an error comes back as `{error}` since `view` cannot fail.
    fn view(&self, target: &str) -> Value {
        let vars = Vars {
            target: Some(target),
            ..Vars::default()
        };
        match self.run("view", self.operations.view.as_ref(), &vars) {
            Ok(Value::Array(entries)) => serde_json::json!({
                "target": target,
                "count": entries.len(),
                "entries": entries,
            }),
            Ok(other) => other,
            Err(e) => serde_json::json!({ "error": e }),
        }
    }

    fn add(&self, target: &str, content: &str, kind: Option<&str>) -> Result<Value, String> {
        let vars = Vars {
            target: Some(target),
            content: Some(content),
            kind,
            ..Vars::default()
        };
        self.run("add", self.operations.add.as_ref(), &vars)
    }

    fn queue_for_review(
        &self,
        target: &str,
        content: &str,
        kind: Option<&str>,
    ) -> Result<Value, String> {
        let vars = Vars {
            target: Some(target),
            content: Some(content),
            kind,
            ..Vars::default()
        };
        self.run(
            "queue_for_review",
            self.operations.queue_for_review.as_ref(),
            &vars,
        )
    }

    fn replace(
        &self,
        target: &str,
        old_text: &str,
        content: &str,
        kind: Option<&str>,
    ) -> Result<Value, String> {
        let vars = Vars {
            target: Some(target),
            old_text: Some(old_text),
            content: Some(content),
            kind,
            ..Vars::default()
        };
        self.run("replace", self.operations.replace.as_ref(), &vars)
    }

    fn supersede(
        &self,
        target: &str,
        old_text: &str,
        content: &str,
        kind: Option<&str>,
        harsh: bool,
    ) -> Result<Value, String> {
        let vars = Vars {
            target: Some(target),
            old_text: Some(old_text),
            content: Some(content),
            kind,
            harsh: Some(harsh),
            ..Vars::default()
        };
        self.run("supersede", self.operations.supersede.as_ref(), &vars)
    }

    fn remove(&self, target: &str, old_text: &str) -> Result<Value, String> {
        let vars = Vars {
            target: Some(target),
            old_text: Some(old_text),
            ..Vars::default()
        };
        self.run("remove", self.operations.remove.as_ref(), &vars)
    }

    fn restore(&self, target: &str, old_text: &str) -> Result<Value, String> {
        let vars = Vars {
            target: Some(target),
            old_text: Some(old_text),
            ..Vars::default()
        };
        self.run("restore", self.operations.restore.as_ref(), &vars)
    }

    fn expand(&self, old_text: &str) -> Result<Value, String> {
        let vars = Vars {
            old_text: Some(old_text),
            ..Vars::default()
        };
        self.run("expand", self.operations.expand.as_ref(), &vars)
    }

    /// An array result is wrapped as `{results}`, the shape pre-recall and
    /// the builtin store use.
    fn search(&self, query: &str) -> Result<Value, String> {
        let vars = Vars {
            query: Some(query),
            ..Vars::default()
        };
        Ok(
            match self.run("search", self.operations.search.as_ref(), &vars)? {
                Value::Array(results) => serde_json::json!({
                    "query": query,
                    "count": results.len(),
                    "results": results,
                }),
                other => other,
            },
        )
    }

    fn record_outcome(&self, target: &str, old_text: &str, success: bool) -> Result<Value, String> {
        let vars = Vars {
            target: Some(target),
            old_text: Some(old_text),
            success: Some(success),
            ..Vars::default()
        };
        self.run(
            "record_outcome",
            self.operations.record_outcome.as_ref(),
            &vars,
        )
    }

    /// Drops the cached prompt text; the next read fetches it again.
    fn refresh_snapshot(&self) -> Result<(), String> {
        *self.prompt.lock_ignore_poison() = None;
        Ok(())
    }
}

fn named_operations(
    ops: &McpMemoryOperations,
) -> [(&'static str, Option<&McpMemoryOperation>); 11] {
    [
        ("view", ops.view.as_ref()),
        ("add", ops.add.as_ref()),
        ("queue_for_review", ops.queue_for_review.as_ref()),
        ("replace", ops.replace.as_ref()),
        ("supersede", ops.supersede.as_ref()),
        ("remove", ops.remove.as_ref()),
        ("restore", ops.restore.as_ref()),
        ("expand", ops.expand.as_ref()),
        ("search", ops.search.as_ref()),
        ("record_outcome", ops.record_outcome.as_ref()),
        ("prompt", ops.prompt.as_ref()),
    ]
}

/// The placeholder names in `s`, in order: every `{name}` whose name is an
/// identifier. Other braces are literal text.
fn placeholders(s: &str) -> Vec<(usize, usize, &str)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(open) = s[from..].find('{').map(|i| from + i) {
        let Some(close) = s[open..].find('}').map(|i| open + i) else {
            break;
        };
        let name = &s[open + 1..close];
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            out.push((open, close + 1, name));
            from = close + 1;
        } else {
            from = open + 1;
        }
    }
    out
}

fn validate_template(template: &Value) -> Result<(), String> {
    match template {
        Value::String(s) => {
            for (_, _, name) in placeholders(s) {
                if !VARIABLES.contains(&name) {
                    return Err(format!(
                        "unknown placeholder {{{name}}}; known: {}",
                        VARIABLES.join(", ")
                    ));
                }
            }
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(validate_template),
        Value::Object(map) => map.values().try_for_each(validate_template),
        _ => Ok(()),
    }
}

/// `template` with its placeholders filled from `lookup`. A string that is
/// exactly one placeholder becomes the value itself, or `None` (dropped from
/// its object or array) when the value is absent.
fn fill(template: &Value, lookup: &dyn Fn(&str) -> Option<Value>) -> Option<Value> {
    match template {
        Value::String(s) => {
            let found = placeholders(s);
            if let [(0, end, name)] = found.as_slice()
                && *end == s.len()
            {
                return lookup(name);
            }
            let mut out = String::with_capacity(s.len());
            let mut at = 0;
            for (start, end, name) in found {
                out.push_str(&s[at..start]);
                match lookup(name) {
                    Some(Value::String(text)) => out.push_str(&text),
                    Some(Value::Null) | None => {}
                    Some(other) => out.push_str(&other.to_string()),
                }
                at = end;
            }
            out.push_str(&s[at..]);
            Some(Value::String(out))
        }
        Value::Array(items) => Some(Value::Array(
            items.iter().filter_map(|v| fill(v, lookup)).collect(),
        )),
        Value::Object(map) => Some(Value::Object(
            map.iter()
                .filter_map(|(k, v)| fill(v, lookup).map(|v| (k.clone(), v)))
                .collect(),
        )),
        other => Some(other.clone()),
    }
}

/// The part of `result` at `pointer`; the whole result without one.
fn select(result: Value, pointer: Option<&str>) -> Option<Value> {
    match pointer {
        None | Some("") => Some(result),
        Some(p) => result.pointer(p).cloned(),
    }
}

/// A tool result as JSON: structured content if any, else the text blocks
/// joined and parsed as JSON, else that text as a string. `isError` results
/// are an `Err` with their text.
fn result_value(result: rmcp::model::CallToolResult) -> Result<Value, String> {
    let text: String = result
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if result.is_error.unwrap_or(false) {
        return Err(if text.is_empty() {
            "the tool returned an error".to_string()
        } else {
            text
        });
    }
    if let Some(structured) = result.structured_content {
        return Ok(structured);
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

// ── The live caller ─────────────────────────────────────────────────

/// Calls tools on a real MCP server over a connection this module owns.
pub struct LiveCaller {
    server: String,
    config: crate::extras::mcp::config::McpServerConfig,
}

impl LiveCaller {
    pub fn new(server: String, config: crate::extras::mcp::config::McpServerConfig) -> Self {
        Self { server, config }
    }
}

type Connections = tokio::sync::Mutex<
    std::collections::HashMap<String, Arc<crate::extras::mcp::client::SharedConnection>>,
>;

/// The runtime every memory tool call and connection lives on, and the
/// connections by server name.
fn worker() -> Result<&'static (tokio::runtime::Runtime, Connections), String> {
    static WORKER: std::sync::OnceLock<Result<(tokio::runtime::Runtime, Connections), String>> =
        std::sync::OnceLock::new();
    WORKER
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("dirge-mcp-memory")
                .enable_all()
                .build()
                .map(|rt| (rt, Connections::default()))
                .map_err(|e| format!("could not start the MCP memory runtime: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

impl McpToolCaller for LiveCaller {
    fn call(&self, tool: &str, arguments: Map<String, Value>) -> Result<Value, String> {
        let (runtime, connections) = worker()?;
        let timeouts = crate::timeout::Timeouts::get();
        let server = self.server.clone();
        let config = self.config.clone();
        let params =
            rmcp::model::CallToolRequestParams::new(tool.to_string()).with_arguments(arguments);
        let (reply, answer) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let result = async {
                let conn = {
                    let mut open = connections.lock().await;
                    match open.get(&server) {
                        Some(conn) => conn.clone(),
                        None => {
                            let conn = crate::extras::mcp::client::connect(server.clone(), &config)
                                .await
                                .map_err(|e| e.to_string())?;
                            open.insert(server.clone(), conn.clone());
                            conn
                        }
                    }
                };
                let peer = conn.current_peer().await;
                match tokio::time::timeout(timeouts.mcp_call, peer.call_tool(params)).await {
                    Ok(Ok(result)) => result_value(result),
                    Ok(Err(e)) => {
                        // A dead transport is reopened on the next call.
                        if crate::extras::mcp::tool::is_transport_failure(&e) {
                            connections.lock().await.remove(&server);
                        }
                        Err(e.to_string())
                    }
                    Err(_) => Err(format!("timed out after {}s", timeouts.mcp_call.as_secs())),
                }
            }
            .await;
            let _ = reply.send(result);
        });
        // A hair above connect + call so a wedged task cannot park the
        // caller forever.
        let wait = timeouts.mcp_init + timeouts.mcp_call + std::time::Duration::from_secs(5);
        answer
            .recv_timeout(wait)
            .unwrap_or_else(|_| Err(format!("no answer within {}s", wait.as_secs())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type Answer = Box<dyn Fn(&str, &Map<String, Value>) -> Result<Value, String> + Send + Sync>;

    /// A fake server: records every call and answers from a closure.
    struct FakeCaller {
        calls: Mutex<Vec<(String, Map<String, Value>)>>,
        answer: Answer,
    }

    impl FakeCaller {
        fn new(
            answer: impl Fn(&str, &Map<String, Value>) -> Result<Value, String> + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                answer: Box::new(answer),
            })
        }

        fn calls(&self) -> Vec<(String, Map<String, Value>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl McpToolCaller for FakeCaller {
        fn call(&self, tool: &str, arguments: Map<String, Value>) -> Result<Value, String> {
            let answer = (self.answer)(tool, &arguments);
            self.calls
                .lock()
                .unwrap()
                .push((tool.to_string(), arguments));
            answer
        }
    }

    fn config(operations: Value) -> McpMemoryConfig {
        serde_json::from_value(json!({ "server": "notes", "operations": operations })).unwrap()
    }

    fn project_scope() -> Scope {
        Scope {
            name: "project",
            project_root: Some(std::path::PathBuf::from("/work/acme")),
        }
    }

    fn provider(operations: Value, caller: Arc<FakeCaller>) -> McpMemoryProvider {
        McpMemoryProvider::new(&config(operations), project_scope(), caller).unwrap()
    }

    #[test]
    fn add_fills_the_template_and_drops_absent_values() {
        let caller = FakeCaller::new(|_, _| Ok(json!({ "success": true })));
        let p = provider(
            json!({ "add": {
                "tool": "remember",
                "arguments": {
                    "text": "{content}",
                    "type": "{kind}",
                    "tags": ["dirge", "{target}", "project:{project}"],
                    "scope": "{scope}",
                    "root": "{project_root}",
                    "fixed": 3
                }
            }}),
            caller.clone(),
        );
        let resp = p.add("pitfalls", "never force-push", None).unwrap();
        assert_eq!(resp, json!({ "success": true }));
        let calls = caller.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "remember");
        assert_eq!(
            Value::Object(calls[0].1.clone()),
            json!({
                "text": "never force-push",
                "tags": ["dirge", "pitfalls", "project:acme"],
                "scope": "project",
                "root": "/work/acme",
                "fixed": 3
            }),
            "an absent kind drops its key; embedded placeholders interpolate"
        );
    }

    #[test]
    fn booleans_keep_their_json_type() {
        let caller = FakeCaller::new(|_, _| Ok(json!("ok")));
        let p = provider(
            json!({
                "supersede": { "tool": "sup", "arguments": { "old": "{old_text}", "harsh": "{harsh}" } },
                "record_outcome": { "tool": "mark", "arguments": { "ok": "{success}", "note": "success={success}" } }
            }),
            caller.clone(),
        );
        p.supersede("memory", "old", "new", Some("semantic"), true)
            .unwrap();
        p.record_outcome("memory", "old", false).unwrap();
        let calls = caller.calls();
        assert_eq!(calls[0].1["harsh"], json!(true));
        assert_eq!(calls[1].1["ok"], json!(false));
        assert_eq!(calls[1].1["note"], json!("success=false"));
    }

    #[test]
    fn missing_operations_are_refused_not_served_elsewhere() {
        let caller = FakeCaller::new(|_, _| Ok(Value::Null));
        let p = provider(json!({}), caller.clone());
        let err = p.add("memory", "x", None).unwrap_err();
        assert!(err.contains("no `add` operation"), "{err}");
        assert!(err.contains("memory.mcp.operations.add"), "{err}");
        assert!(p.remove("memory", "x").is_err());
        assert!(p.restore("memory", "x").is_err());
        assert!(p.expand("x").is_err());
        assert!(p.search("x").is_err());
        assert!(p.queue_for_review("memory", "x", None).is_err());
        assert!(p.supersede("memory", "a", "b", None, false).is_err());
        assert!(p.record_outcome("memory", "a", true).is_err());
        assert!(p.view("memory")["error"].is_string());
        assert_eq!(p.format_for_system_prompt(), "");
        assert!(caller.calls().is_empty(), "nothing may reach the server");
    }

    #[test]
    fn result_pointer_selects_and_arrays_take_the_builtin_shape() {
        let caller = FakeCaller::new(|tool, _| match tool {
            "find" => Ok(json!({ "data": [{ "content": "use cargo" }] })),
            "list" => Ok(json!(["a", "b"])),
            _ => Ok(json!({})),
        });
        let p = provider(
            json!({
                "search": { "tool": "find", "arguments": { "q": "{query}" }, "result": "/data" },
                "view": { "tool": "list" },
                "expand": { "tool": "other", "result": "/missing" }
            }),
            caller.clone(),
        );
        let found = p.search("build tool").unwrap();
        assert_eq!(found["results"][0]["content"], "use cargo");
        assert_eq!(found["count"], 1);
        let view = p.view("memory");
        assert_eq!(view["entries"], json!(["a", "b"]));
        assert_eq!(view["count"], 2);
        let err = p.expand("x").unwrap_err();
        assert!(err.contains("/missing"), "{err}");
    }

    #[test]
    fn tool_errors_surface_with_server_and_tool() {
        let caller = FakeCaller::new(|_, _| Err("quota exceeded".into()));
        let p = provider(json!({ "add": { "tool": "remember" } }), caller);
        let err = p.add("memory", "x", None).unwrap_err();
        assert!(err.contains("notes::remember"), "{err}");
        assert!(err.contains("quota exceeded"), "{err}");
    }

    #[test]
    fn prompt_is_fetched_once_until_refreshed() {
        let caller = FakeCaller::new(|_, _| Ok(json!({ "text": "- prefers tabs" })));
        let p = provider(
            json!({ "prompt": { "tool": "digest", "result": "/text" } }),
            caller.clone(),
        );
        assert_eq!(p.format_for_system_prompt(), "\n- prefers tabs\n");
        assert_eq!(p.format_for_system_prompt(), "\n- prefers tabs\n");
        assert_eq!(caller.calls().len(), 1, "the snapshot is cached");
        p.refresh_snapshot().unwrap();
        p.format_for_system_prompt();
        assert_eq!(caller.calls().len(), 2, "refresh re-fetches");
    }

    #[test]
    fn a_failed_prompt_fetch_injects_nothing_and_retries() {
        let caller = FakeCaller::new(|_, _| Err("down".into()));
        let p = provider(json!({ "prompt": { "tool": "digest" } }), caller.clone());
        assert_eq!(p.format_for_system_prompt(), "");
        assert_eq!(p.format_for_system_prompt(), "");
        assert_eq!(caller.calls().len(), 2, "a failure is not cached");
    }

    #[test]
    fn construction_rejects_unknown_placeholders_and_bad_pointers() {
        let caller = FakeCaller::new(|_, _| Ok(Value::Null));
        let typo = config(json!({ "add": { "tool": "t", "arguments": { "x": "{contnet}" } } }));
        let err = McpMemoryProvider::new(&typo, project_scope(), caller.clone())
            .err()
            .unwrap();
        assert!(err.contains("{contnet}"), "{err}");
        assert!(err.contains("operations.add"), "{err}");

        let pointer = config(json!({ "search": { "tool": "t", "result": "data" } }));
        let err = McpMemoryProvider::new(&pointer, project_scope(), caller.clone())
            .err()
            .unwrap();
        assert!(err.contains("JSON pointer"), "{err}");

        let empty = config(json!({ "view": { "tool": " " } }));
        assert!(McpMemoryProvider::new(&empty, project_scope(), caller).is_err());
    }

    #[test]
    fn literal_braces_are_not_placeholders() {
        let caller = FakeCaller::new(|_, _| Ok(Value::Null));
        let p = provider(
            json!({ "add": { "tool": "t", "arguments": { "q": "{ not a var } {content}" } } }),
            caller.clone(),
        );
        p.add("memory", "x", None).unwrap();
        assert_eq!(caller.calls()[0].1["q"], json!("{ not a var } x"));
    }

    #[test]
    fn global_scope_has_no_project() {
        let caller = FakeCaller::new(|_, _| Ok(Value::Null));
        let p = McpMemoryProvider::new(
            &config(json!({ "add": { "tool": "t", "arguments": {
                "scope": "{scope}", "project": "{project}", "tag": "p:{project}"
            } } })),
            Scope {
                name: "global",
                project_root: None,
            },
            caller.clone(),
        )
        .unwrap();
        p.add("memory", "x", None).unwrap();
        assert_eq!(
            Value::Object(caller.calls()[0].1.clone()),
            json!({ "scope": "global", "tag": "p:" })
        );
    }

    #[test]
    fn tool_results_become_json() {
        use rmcp::model::{CallToolResult, ContentBlock};
        let text = CallToolResult::success(vec![ContentBlock::text(r#"{"id": 7}"#)]);
        assert_eq!(result_value(text).unwrap(), json!({ "id": 7 }));
        let plain = CallToolResult::success(vec![ContentBlock::text("stored")]);
        assert_eq!(result_value(plain).unwrap(), json!("stored"));
        let mut structured = CallToolResult::success(vec![ContentBlock::text("ignored")]);
        structured.structured_content = Some(json!({ "ok": true }));
        assert_eq!(result_value(structured).unwrap(), json!({ "ok": true }));
        let mut failed = CallToolResult::success(vec![ContentBlock::text("no such entry")]);
        failed.is_error = Some(true);
        assert_eq!(result_value(failed).unwrap_err(), "no such entry");
    }
}
