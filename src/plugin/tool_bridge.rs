//! Let Janet plugins invoke dirge's own tools.
//!
//! Plugins can *register* tools (`harness/register-tool`) and *intercept*
//! them (`on-tool-start` / `on-tool-end`), but until now could not *call*
//! one. A plugin that wanted a tool's output had to reimplement it, which
//! means it also had to reimplement the permission checks, and could never
//! reach MCP or semantic tools at all.
//!
//! This module owns the policy and the tokio-side responder. The Janet FFI
//! lives in [`super::worker`] with the other C functions.
//!
//! Two hazards shape the design.
//!
//! **Deadlock.** `harness/call-tool` blocks the Janet worker thread until
//! the responder answers. Any tool that itself needs that thread can never
//! be answered, so plugin-registered tools (which run through
//! [`super::extension::JanetLoopTool`]) are refused by name rather than
//! attempted. For the same reason the responder dispatches
//! [`LoopTool::execute`] directly instead of going through the loop's
//! hook-firing path — an `on-tool-start` hook would re-enter the blocked
//! worker. maki solves the identical problem with `Emit::Silent`.
//!
//! **Permission.** dirge's `check_perm*` runs *inside* each tool, after the
//! plugin pre-hook, so dispatching straight to `execute` keeps the gate
//! intact: a plugin calling `bash` still prompts the user. That is the
//! reason this is a thin call rather than a reimplementation of dispatch.

use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

use crate::agent::agent_loop::tool::{AbortSignal, LoopToolUpdate};
use crate::agent::agent_loop::{LoopTool, LoopToolResult};

/// A tool invocation forwarded from the Janet worker to the tokio runtime.
/// Mirrors [`super::worker::LspRequest`]: the worker blocks on `reply`
/// while the responder does the async work.
#[derive(Debug)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub struct ToolCallRequest {
    pub name: String,
    /// Raw JSON object of arguments, as the plugin wrote it.
    pub args_json: String,
    pub reply: std::sync::mpsc::Sender<Result<String, String>>,
}

static TOOL_CALL_TX: OnceLock<tokio::sync::mpsc::UnboundedSender<ToolCallRequest>> =
    OnceLock::new();

/// Install the sender the Janet C function reads. Called once from
/// `main.rs`, mirroring `install_sandbox_exec_tx`.
#[cfg(feature = "plugin")]
pub fn install_tool_call_tx(tx: tokio::sync::mpsc::UnboundedSender<ToolCallRequest>) {
    let _ = TOOL_CALL_TX.set(tx);
}

#[cfg(feature = "plugin")]
pub fn tool_call_tx() -> Option<&'static tokio::sync::mpsc::UnboundedSender<ToolCallRequest>> {
    TOOL_CALL_TX.get()
}

/// The live `LoopTool` registry.
///
/// Republished on every agent build rather than captured once: the agent is
/// rebuilt at run boundaries (model switch, `prepare-next-run`), and MCP
/// tools can attach late. A stale snapshot would silently lose them.
static REGISTRY: OnceLock<Mutex<Vec<Arc<dyn LoopTool>>>> = OnceLock::new();

fn registry() -> &'static Mutex<Vec<Arc<dyn LoopTool>>> {
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Publish the tool set the agent was just built with.
pub fn publish_registry(tools: &[Arc<dyn LoopTool>]) {
    if let Ok(mut guard) = registry().lock() {
        *guard = tools.to_vec();
    }
}

fn snapshot() -> Vec<Arc<dyn LoopTool>> {
    registry()
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// True once a registry has been published *and* the responder is wired,
/// so `(harness/tools?)` reflects runtime availability rather than mere
/// compile-time presence — the same contract `harness/lsp?` has.
#[cfg(feature = "plugin")]
pub fn is_live() -> bool {
    let wired = TOOL_CALL_TX
        .get()
        .map(|tx| !tx.is_closed())
        .unwrap_or(false);
    wired && !snapshot().is_empty()
}

/// Tools that must never be reached from a plugin, independent of the
/// plugin-registered check below.
const NEVER_CALLABLE: &[&str] = &[
    // Subagents run isolated — no tool access, no plugin hooks (see
    // docs/plugins.md). Reaching one from inside a plugin would smuggle a
    // whole agent run behind that boundary.
    "task",
];

/// Names of tools backed by Janet handlers, which cannot be called while
/// the Janet worker is blocked waiting for this very reply.
///
/// Cached rather than read from the `PluginManager` on demand, and that is
/// load-bearing: the hook dispatcher holds the PluginManager lock for the
/// duration of a Janet call, so a `harness/*` C function that asks for the
/// same (non-reentrant) lock deadlocks the agent outright. The responder
/// thread is no safer — the worker it must answer is blocked holding it.
/// `publish_plugin_tool_names` fills this from the agent build path, where
/// the lock is genuinely free.
static PLUGIN_TOOL_NAMES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn plugin_tool_names_cell() -> &'static Mutex<Vec<String>> {
    PLUGIN_TOOL_NAMES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Publish the plugin-registered tool names. Called from
/// `build_loop_tools`, which already has them in hand.
pub fn publish_plugin_tool_names(names: Vec<String>) {
    if let Ok(mut guard) = plugin_tool_names_cell().lock() {
        *guard = names;
    }
}

fn plugin_tool_names() -> Vec<String> {
    plugin_tool_names_cell()
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// Why `name` may not be called, or `None` if it may.
///
/// Split out from dispatch so the policy is unit-testable without a
/// runtime, a registry or a Janet worker.
pub fn refusal(name: &str, plugin_tools: &[String]) -> Option<String> {
    if NEVER_CALLABLE.contains(&name) {
        return Some(format!(
            "'{name}' cannot be called from a plugin: subagents run isolated from \
             plugin hooks and tool access"
        ));
    }
    if plugin_tools.iter().any(|n| n == name) {
        return Some(format!(
            "'{name}' is a plugin-registered tool and cannot be called from a plugin: \
             its handler needs the Janet worker, which is blocked awaiting this call"
        ));
    }
    None
}

/// Render a tool's LLM-visible content as plain text.
fn flatten(result: &LoopToolResult) -> String {
    let mut out = String::new();
    for block in &result.content {
        let text = match block {
            Value::String(s) => Some(s.as_str()),
            Value::Object(map) => map.get("text").and_then(|t| t.as_str()),
            _ => None,
        };
        if let Some(text) = text {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    // A tool that returned only structured details still owes the caller
    // something readable.
    if out.is_empty() && !result.details.is_null() {
        out = result.details.to_string();
    }
    out
}

/// Look up and run one tool. Errors are returned, never panicked.
async fn dispatch(name: &str, args_json: &str) -> Result<String, String> {
    if let Some(reason) = refusal(name, &plugin_tool_names()) {
        return Err(reason);
    }

    let args: Value = if args_json.trim().is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(args_json).map_err(|e| format!("arguments are not valid JSON: {e}"))?
    };
    if !args.is_object() {
        return Err("arguments must be a JSON object".to_string());
    }

    let tool = snapshot()
        .into_iter()
        .find(|t| t.name() == name)
        .ok_or_else(|| format!("no tool named '{name}'"))?;

    let args = tool.prepare_arguments(args);
    // A no-op progress sink: streaming updates have nowhere to go while the
    // caller is a blocked synchronous Janet call.
    let on_update: LoopToolUpdate = Arc::new(|_: &LoopToolResult| {});

    match tool
        .execute("plugin-call-tool", args, AbortSignal::new(), on_update)
        .await
    {
        Ok(result) => Ok(flatten(&result)),
        Err(e) => Err(e),
    }
}

/// Drain `harness/call-tool` requests and answer them against the live
/// registry. Runs until the channel closes at worker shutdown. Mirrors
/// [`super::spawn_lsp_responder`].
#[cfg(feature = "plugin")]
pub fn spawn_tool_responder(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ToolCallRequest>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let answer = dispatch(&req.name, &req.args_json).await;
            let _ = req.reply.send(answer);
        }
    })
}

/// JSON array of `{name, description, parameters}` for every live tool,
/// minus the ones a plugin may not call — so what is advertised is what is
/// callable, which is the invariant maki's `interpreter_tools` predicate
/// exists to hold.
pub fn list_json() -> String {
    let refused = plugin_tool_names();
    let entries: Vec<Value> = snapshot()
        .iter()
        .filter(|t| refusal(t.name(), &refused).is_none())
        .map(|t| {
            serde_json::json!({
                "name": t.name(),
                "description": t.description(),
                "parameters": t.parameters(),
            })
        })
        .collect();
    Value::Array(entries).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_tools_are_callable() {
        assert!(refusal("read", &[]).is_none());
        assert!(refusal("bash", &[]).is_none());
    }

    /// The deadlock guard. A plugin tool's handler runs on the Janet
    /// worker, which is blocked waiting for this call to return, so
    /// attempting it would hang until the call timed out.
    #[test]
    fn plugin_registered_tools_are_refused() {
        let plugins = vec!["code_execution".to_string()];
        let reason = refusal("code_execution", &plugins).expect("must refuse");
        assert!(reason.contains("Janet worker"), "{reason}");
    }

    #[test]
    fn subagents_are_refused() {
        let reason = refusal("task", &[]).expect("must refuse");
        assert!(reason.contains("isolated"), "{reason}");
    }

    #[test]
    fn refusal_is_exact_not_substring() {
        // `read_minified` must not be refused because `read` is in a list.
        assert!(refusal("read_minified", &["read".to_string()]).is_none());
    }

    #[test]
    fn flatten_reads_text_blocks() {
        let result = LoopToolResult {
            content: vec![
                serde_json::json!({"type": "text", "text": "one"}),
                serde_json::json!({"type": "text", "text": "two"}),
            ],
            details: Value::Null,
            terminate: None,
        };
        assert_eq!(flatten(&result), "one\ntwo");
    }

    #[test]
    fn flatten_falls_back_to_details() {
        let result = LoopToolResult {
            content: vec![],
            details: serde_json::json!({"count": 2}),
            terminate: None,
        };
        assert_eq!(flatten(&result), r#"{"count":2}"#);
    }
}
