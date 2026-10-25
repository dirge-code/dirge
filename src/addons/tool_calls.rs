//! `dirge.harness/call-tool`: addon code calling dirge's own loop tools,
//! built-ins and MCP tools alike, the way Janet plugins do through
//! `harness/call-tool`. Permission checks stay inside each tool. Built only
//! with the `plugin` feature, whose tool bridge runs the calls.

use std::sync::Arc;

use serde_json::Value;
use tokio::runtime::Handle;

use super::port::ToolGateway;
use crate::agent::agent_loop::LoopTool;
use crate::plugin::tool_bridge;

/// The tools a call may reach, read when the call is made.
pub type ToolSource = Arc<dyn Fn() -> Vec<Arc<dyn LoopTool>> + Send + Sync>;

/// Why `tool` may not be called from addon code, or `None` if it may.
/// Decided by the registered tool itself: subagents are refused by name,
/// addon tools by their [`LoopTool::source`].
pub fn refusal(tool: &dyn LoopTool) -> Option<String> {
    let name = tool.name();
    if tool_bridge::NEVER_CALLABLE.contains(&name) {
        return Some(format!(
            "'{name}' cannot be called from an addon: subagents run isolated from addon code"
        ));
    }
    if tool.source() == Some(super::tool::SOURCE) {
        return Some(format!(
            "'{name}' is an addon tool and cannot be called from an addon: its handler \
             needs the addon isolate, which is blocked awaiting this call"
        ));
    }
    None
}

/// Loop tools driven on the runtime dirge started on.
pub struct LoopTools {
    handle: Handle,
    tools: ToolSource,
}

impl LoopTools {
    pub fn new(handle: Handle, tools: ToolSource) -> Self {
        Self { handle, tools }
    }

    /// The agent's published tool set on the current runtime; `None`
    /// outside one.
    pub fn live() -> Option<Self> {
        let tools: ToolSource = Arc::new(tool_bridge::live_tools);
        Handle::try_current()
            .ok()
            .map(|handle| Self::new(handle, tools))
    }
}

impl ToolGateway for LoopTools {
    fn names(&self) -> Vec<String> {
        (self.tools)()
            .iter()
            .filter(|t| refusal(t.as_ref()).is_none())
            .map(|t| t.name().to_string())
            .collect()
    }

    fn call(&self, name: &str, args: &Value) -> Result<String, String> {
        // The addon isolate is a plain thread. Anywhere else, blocking here
        // would park a runtime worker the call itself needs.
        if Handle::try_current().is_ok() {
            return Err("call-tool must not run on an async runtime thread".to_string());
        }
        let tools = (self.tools)();
        let refused = tools
            .iter()
            .find(|t| t.name() == name)
            .and_then(|t| refusal(t.as_ref()));
        if let Some(reason) = refused {
            return Err(reason);
        }
        self.handle.block_on(tool_bridge::execute_in(
            &tools,
            name,
            args.clone(),
            "addon-call-tool",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    use serde_json::json;

    use super::*;
    use crate::agent::agent_loop::LoopToolResult;
    use crate::agent::agent_loop::tool::{AbortSignal, LoopToolUpdate};

    /// A tool echoing its `text` argument, contributed by `source`.
    #[derive(Debug)]
    struct Echo {
        name: &'static str,
        source: Option<&'static str>,
    }

    fn built_in(name: &'static str) -> Arc<dyn LoopTool> {
        Arc::new(Echo { name, source: None })
    }

    fn addon(name: &'static str) -> Arc<dyn LoopTool> {
        Arc::new(Echo {
            name,
            source: Some(crate::addons::tool::SOURCE),
        })
    }

    impl LoopTool for Echo {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn label(&self) -> &str {
            "Echo"
        }
        fn parameters(&self) -> &Value {
            static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            P.get_or_init(|| json!({"type": "object"}))
        }
        fn source(&self) -> Option<&str> {
            self.source
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            args: Value,
            _signal: AbortSignal,
            _on_update: LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>> {
            Box::pin(async move {
                Ok(LoopToolResult {
                    content: vec![
                        json!({"type": "text", "text": format!("echo {}", args["text"])}),
                    ],
                    details: Value::Null,
                    terminate: None,
                })
            })
        }
    }

    fn gateway_over(
        rt: &tokio::runtime::Runtime,
        tools: fn() -> Vec<Arc<dyn LoopTool>>,
    ) -> LoopTools {
        LoopTools::new(rt.handle().clone(), Arc::new(tools))
    }

    fn gateway(rt: &tokio::runtime::Runtime) -> LoopTools {
        gateway_over(rt, || {
            vec![built_in("read"), addon("count-rows"), built_in("task")]
        })
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn a_built_in_named_like_a_skipped_addon_tool_stays_callable() {
        // An addon's `read`, skipped at build for colliding with the
        // built-in, leaves only the built-in in the registry.
        let rt = runtime();
        let gw = gateway_over(&rt, || vec![built_in("read")]);
        assert_eq!(gw.names(), vec!["read".to_string()]);
        let answer = std::thread::spawn(move || gw.call("read", &json!({"text": "hi"})))
            .join()
            .unwrap();
        assert_eq!(answer, Ok("echo \"hi\"".to_string()));
    }

    #[test]
    fn the_registered_tool_decides_not_its_name() {
        assert!(refusal(built_in("read").as_ref()).is_none());
        let reason = refusal(addon("read").as_ref()).expect("refused");
        assert!(reason.contains("addon isolate"), "{reason}");
    }

    #[test]
    fn built_in_tools_are_callable_addon_tools_and_subagents_are_not() {
        assert!(refusal(built_in("read").as_ref()).is_none());
        let own = refusal(addon("count-rows").as_ref()).expect("refused");
        assert!(own.contains("addon isolate"), "{own}");
        let task = refusal(built_in("task").as_ref()).expect("refused");
        assert!(task.contains("isolated"), "{task}");
    }

    #[test]
    fn names_advertise_only_what_is_callable() {
        let rt = runtime();
        assert_eq!(gateway(&rt).names(), vec!["read".to_string()]);
    }

    #[test]
    fn a_call_from_a_plain_thread_runs_the_tool_on_the_runtime() {
        let rt = runtime();
        let gw = gateway(&rt);
        let answer = std::thread::spawn(move || gw.call("read", &json!({"text": "hi"})))
            .join()
            .unwrap();
        assert_eq!(answer, Ok("echo \"hi\"".to_string()));
    }

    #[test]
    fn refused_and_unknown_tools_answer_errors() {
        let rt = runtime();
        let gw = gateway(&rt);
        let answers = std::thread::spawn(move || {
            [
                gw.call("count-rows", &json!({})),
                gw.call("task", &json!({})),
                gw.call("nowhere", &json!({})),
                gw.call("read", &json!("not an object")),
            ]
        })
        .join()
        .unwrap();
        assert!(answers[0].as_ref().unwrap_err().contains("addon tool"));
        assert!(answers[1].as_ref().unwrap_err().contains("isolated"));
        assert!(answers[2].as_ref().unwrap_err().contains("no tool named"));
        assert!(answers[3].as_ref().unwrap_err().contains("JSON object"));
    }

    #[test]
    fn calls_from_a_runtime_thread_are_refused() {
        let rt = runtime();
        let gw = gateway(&rt);
        let err = rt.block_on(async move { gw.call("read", &json!({})).unwrap_err() });
        assert!(err.contains("async runtime"), "{err}");
    }
}
