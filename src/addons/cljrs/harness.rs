//! The `dirge.harness` namespace addon code calls.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use cljrs_gc::GcPtr;
use cljrs_runtime::env::env::GlobalEnv;
use cljrs_value::{Arity, NativeFn, Value, ValueResult};
use serde_json::{Value as Json, json};

use super::bridge;
use crate::addons::policy;
use crate::addons::port::{Harness, Level};

/// Namespace addon code requires to reach dirge.
pub const HARNESS_NS: &str = "dirge.harness";

const BLOCKED_MCP_CALL: &str = "mcp-call is unavailable while dirge waits on this addon \
     (system-prompt and on-prompt hooks, load, shutdown); call it from a command, \
     a tool or a tool-call hook";

const BLOCKED_CALL_TOOL: &str = "call-tool is unavailable while dirge waits on this addon \
     (system-prompt and on-prompt hooks, load, shutdown); call it from a command, \
     a tool or a tool-call hook";

/// Register `dirge.harness` into `globals`.
///
/// - `(notify msg)` / `(notify msg level)`: a line in dirge's chat area;
///   `level` is `:info` (default), `:warn` or `:error`.
/// - `(log level msg)`: a `tracing` event on the `dirge::addon` target.
/// - `(cwd)`: dirge's working directory.
/// - `(version)`: the dirge version string.
/// - `(mcp-servers)`: names of the MCP servers dirge is connected to.
/// - `(mcp-call server tool)` / `(mcp-call server tool args)`: call an MCP
///   tool through dirge's own connection. Answers the tool result
///   (`{:content [...] :isError bool}`), or `{:error msg}` when the call
///   could not be made. Blocks until the server answers. Refused (an
///   `{:error}` answer) while dirge's event loop is waiting on the addon, as
///   it is for `:dirge/system-prompt`, `:dirge/on-prompt`, loading and
///   shutdown: the call would need that loop to make progress.
/// - `(json-parse text)`: JSON text as data (object keys as keywords), or
///   nil when it is not JSON.
/// - `(panel op)`: change a box in the side panel; `op` is
///   `{:op :show :id :title :lines [...]}` (or `:markdown "..."` instead of
///   `:lines`), `{:op :append :id :text :face}`, `{:op :focus :id :title}` or
///   `{:op :close :id}`. Answers true when the change was delivered.
/// - `(tools)`: names of the dirge tools `call-tool` reaches.
/// - `(call-tool name)` / `(call-tool name args)`: run a dirge tool (a
///   built-in or an MCP tool) with the `args` map. Answers `{:ok text}` or
///   `{:error msg}`. Blocks until the tool answers. Addon tools and `task`
///   are refused, and so is any call while dirge's event loop waits on the
///   addon, as for `mcp-call`. In a build without the `plugin` feature
///   `(tools)` is empty and every call answers `{:error}` saying so.
/// - `(refresh!)`: once the current call returns, ask every addon again for
///   its tools, hooks and commands, without shutting it down, and hand the
///   changes to the running agent. For definitions changed at a REPL.
pub fn install(
    globals: &Arc<GlobalEnv>,
    harness: Harness,
    caller_on_runtime: Rc<Cell<bool>>,
    refresh_requested: Rc<Cell<bool>>,
) {
    let Harness {
        sink,
        panels,
        mcp,
        tools,
    } = harness;
    define(globals, "notify", Arity::Variadic { min: 1 }, move |args| {
        let level = args.get(1).map_or(Level::Info, level_of);
        sink.notify(level, &text(&args[0]));
        Ok(Value::Nil)
    });
    define(globals, "log", Arity::Fixed(2), |args| {
        let message = text(&args[1]);
        match level_of(&args[0]) {
            Level::Error => tracing::error!(target: "dirge::addon", "{message}"),
            Level::Warn => tracing::warn!(target: "dirge::addon", "{message}"),
            Level::Info => tracing::info!(target: "dirge::addon", "{message}"),
        }
        Ok(Value::Nil)
    });
    define(globals, "cwd", Arity::Fixed(0), |_| {
        let cwd = std::env::current_dir().unwrap_or_default();
        Ok(Value::Str(GcPtr::new(cwd.display().to_string())))
    });
    define(globals, "version", Arity::Fixed(0), |_| {
        Ok(Value::Str(GcPtr::new(
            env!("CARGO_PKG_VERSION").to_string(),
        )))
    });
    let gateway = mcp.clone();
    define(globals, "mcp-servers", Arity::Fixed(0), move |_| {
        Ok(bridge::to_clj(&Json::from(gateway.servers())))
    });
    let mcp_blocked = caller_on_runtime.clone();
    define(
        globals,
        "mcp-call",
        Arity::Variadic { min: 2 },
        move |args| {
            if mcp_blocked.get() {
                return Ok(bridge::to_clj(&json!({ "error": BLOCKED_MCP_CALL })));
            }
            let params = args.get(2).map_or_else(|| json!({}), bridge::to_json);
            let answer = mcp
                .call(&text(&args[0]), &text(&args[1]), &params)
                .unwrap_or_else(|error| json!({ "error": error }));
            Ok(bridge::to_clj(&answer))
        },
    );
    define(globals, "json-parse", Arity::Fixed(1), |args| {
        Ok(serde_json::from_str::<Json>(&text(&args[0]))
            .map_or(Value::Nil, |data| bridge::to_clj(&data)))
    });
    define(
        globals,
        "panel",
        Arity::Fixed(1),
        move |args| match policy::panel_request(&bridge::to_json(&args[0])) {
            Ok(request) => Ok(Value::Bool(panels.panel(request))),
            Err(error) => {
                tracing::warn!(target: "dirge::addon", %error, "panel op ignored");
                Ok(Value::Bool(false))
            }
        },
    );
    define(globals, "refresh!", Arity::Fixed(0), move |_| {
        refresh_requested.set(true);
        Ok(Value::Bool(true))
    });
    let catalog = tools.clone();
    define(globals, "tools", Arity::Fixed(0), move |_| {
        Ok(bridge::to_clj(&Json::from(catalog.names())))
    });
    define(
        globals,
        "call-tool",
        Arity::Variadic { min: 1 },
        move |args| {
            if caller_on_runtime.get() {
                return Ok(bridge::to_clj(&json!({ "error": BLOCKED_CALL_TOOL })));
            }
            let params = args.get(1).map_or_else(|| json!({}), bridge::to_json);
            let answer = match tools.call(&text(&args[0]), &params) {
                Ok(output) => json!({ "ok": output }),
                Err(error) => json!({ "error": error }),
            };
            Ok(bridge::to_clj(&answer))
        },
    );
    globals.mark_loaded(HARNESS_NS);
}

fn define(
    globals: &Arc<GlobalEnv>,
    name: &str,
    arity: Arity,
    f: impl Fn(&[Value]) -> ValueResult<Value> + 'static,
) {
    let native = NativeFn::with_closure(format!("{HARNESS_NS}/{name}"), arity, f);
    globals.intern(
        HARNESS_NS,
        Arc::from(name),
        Value::NativeFunction(GcPtr::new(native)),
    );
}

/// A string argument as text; a keyword without its colon; anything else
/// printed.
fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.get().clone(),
        Value::Keyword(k) => k.get().full_name(),
        other => other.to_string(),
    }
}

fn level_of(v: &Value) -> Level {
    match v {
        Value::Keyword(k) => Level::parse(&k.get().name),
        Value::Str(s) => Level::parse(s.get()),
        _ => Level::Info,
    }
}
