//! Minimal, swappable agent loop + tool seam for the wasm-able lib core.
//!
//! The native agent loop (`src/agent/agent_loop`) is entangled with the TUI,
//! SQLite session state, process spawning, and the terminal. This module is
//! the pared-down wasm slice of that surface: two small traits ([`Tool`] and
//! [`Completer`]) and an [`Agent`] that runs the standard tool-calling turn
//! loop (model returns tool calls -> execute -> feed results back -> repeat
//! until text) against whichever backend the caller supplies.
//!
//! Both seams are swappable:
//!
//! - [`Completer`] abstracts the model, so the loop is testable against a
//!   scripted mock natively and, on wasm, backed by rig's DeepSeek provider.
//! - [`Tool`] abstracts a single callable tool; the native shell/fs/process
//!   tools can be swapped for pure built-ins ([`EchoTool`]) or, later,
//!   JS-backed shims registered by the host — without touching the loop.

use std::sync::Arc;

use rig::completion::message::ToolCall;
use rig::completion::{AssistantContent, CompletionResponse, Message, ToolDefinition};
use rig::wasm_compat::WasmBoxedFuture;
use serde_json::Value;

/// A single tool the agent loop can dispatch.
///
/// The three accessors describe the tool to the model (name, description,
/// JSON Schema); [`Tool::call`] executes it. `call` returns a boxed future so
/// the trait stays dyn-compatible without `async_trait`, and uses rig's
/// `WasmBoxedFuture` so the `Send` bound disappears on wasm (where a
/// JS-backed tool's future is not `Send`).
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn call(&self, args: Value) -> WasmBoxedFuture<'_, Result<String, String>>;
}

/// A model backend the agent loop drives.
///
/// Takes the full transcript (in order) plus the tool definitions and returns
/// one completion. On wasm the DeepSeek adapter builds a rig completion
/// request from these; in tests a scripted mock returns canned responses.
pub trait Completer: Send + Sync {
    fn complete(
        &self,
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
    ) -> WasmBoxedFuture<'_, Result<CompletionResponse<()>, String>>;
}

/// Drives the tool-calling turn loop over a [`Completer`] and a set of tools.
pub struct Agent {
    completer: Arc<dyn Completer>,
    tools: Vec<Arc<dyn Tool>>,
    max_rounds: usize,
}

impl Agent {
    pub fn new(completer: Arc<dyn Completer>) -> Self {
        Self {
            completer,
            tools: Vec::new(),
            max_rounds: 8,
        }
    }

    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn max_rounds(mut self, max_rounds: usize) -> Self {
        self.max_rounds = max_rounds;
        self
    }

    /// Run one user turn: send the prompt, execute any tool calls the model
    /// makes, and return the final assistant text.
    pub async fn run(&self, prompt: String) -> Result<String, String> {
        let definitions: Vec<ToolDefinition> = self
            .tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters(),
            })
            .collect();

        let mut history: Vec<Message> = vec![Message::user(prompt)];

        for _ in 0..self.max_rounds {
            let response = self
                .completer
                .complete(history.clone(), definitions.clone())
                .await?;

            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut text: Option<String> = None;

            for item in response.choice {
                match item {
                    AssistantContent::Text(t) => text = Some(t.text),
                    AssistantContent::ToolCall(tc) => tool_calls.push(tc),
                    _ => {}
                }
            }

            if tool_calls.is_empty() {
                return text
                    .ok_or_else(|| "agent produced neither text nor a tool call".to_string());
            }

            // Assistant tool calls first, then their results, in order.
            for tc in &tool_calls {
                history.push(Message::from(tc.clone()));
            }
            for tc in &tool_calls {
                let result = self
                    .dispatch(&tc.function.name, tc.function.arguments.clone())
                    .await;
                let content = result.unwrap_or_else(|e| format!("tool error: {e}"));
                history.push(Message::tool_result(tc.id.as_str(), content));
            }
        }

        Err("agent exceeded the maximum number of tool-call rounds".to_string())
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<String, String> {
        for tool in &self.tools {
            if tool.name() == name {
                return tool.call(args).await;
            }
        }
        Err(format!("unknown tool: {name}"))
    }
}

/// Pure built-in tool that echoes its `text` argument back. Proves the loop
/// end-to-end with no native dependencies; real tools (shell/fs/process) are
/// swapped in behind the same [`Tool`] seam.
pub struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo the provided text back to the caller."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" }
            },
            "required": ["text"]
        })
    }

    fn call(&self, args: Value) -> WasmBoxedFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let text = args.get("text").and_then(Value::as_str).unwrap_or_default();
            Ok(format!("echo: {text}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use rig::OneOrMany;
    use rig::completion::message::{ToolCall, ToolFunction};
    use rig::completion::{AssistantContent, CompletionResponse, Message, ToolDefinition, Usage};
    use rig::wasm_compat::WasmBoxedFuture;
    use serde_json::json;

    use super::{Agent, Completer, EchoTool, Tool};

    fn text_response(text: &str) -> CompletionResponse<()> {
        CompletionResponse {
            choice: OneOrMany::one(AssistantContent::text(text)),
            usage: Usage::new(),
            raw_response: (),
            message_id: None,
        }
    }

    fn tool_call_response(name: &str, args: serde_json::Value) -> CompletionResponse<()> {
        CompletionResponse {
            choice: OneOrMany::one(AssistantContent::ToolCall(ToolCall {
                id: "call_1".to_string(),
                call_id: None,
                function: ToolFunction {
                    name: name.to_string(),
                    arguments: args,
                },
                signature: None,
                additional_params: None,
            })),
            usage: Usage::new(),
            raw_response: (),
            message_id: None,
        }
    }

    /// Records every transcript it sees and returns a scripted response per
    /// call, so tests assert both the final text and the transcript shape.
    struct ScriptedCompleter {
        script: Mutex<VecDeque<CompletionResponse<()>>>,
        seen: Mutex<Vec<Vec<Message>>>,
    }

    impl ScriptedCompleter {
        fn new(responses: Vec<CompletionResponse<()>>) -> Self {
            Self {
                script: Mutex::new(responses.into_iter().collect()),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Completer for ScriptedCompleter {
        fn complete(
            &self,
            messages: Vec<Message>,
            _tools: Vec<ToolDefinition>,
        ) -> WasmBoxedFuture<'_, Result<CompletionResponse<()>, String>> {
            self.seen.lock().unwrap().push(messages);
            let next = self.script.lock().unwrap().pop_front();
            Box::pin(async move { next.ok_or_else(|| "script exhausted".to_string()) })
        }
    }

    #[tokio::test]
    async fn agent_dispatches_tool_then_returns_text() {
        let completer = Arc::new(ScriptedCompleter::new(vec![
            tool_call_response("echo", json!({ "text": "hi" })),
            text_response("done"),
        ]));
        let agent = Agent::new(completer.clone()).with_tool(Arc::new(EchoTool));

        let out = agent.run("please echo".to_string()).await.unwrap();
        assert_eq!(out, "done");

        let seen = completer.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "expected two model calls");
        assert_eq!(seen[0].len(), 1, "first call: only the user prompt");
        assert_eq!(
            seen[1].len(),
            3,
            "second call: user + assistant tool call + tool result"
        );
    }

    #[tokio::test]
    async fn agent_feeds_unknown_tool_error_back_to_model() {
        let completer = Arc::new(ScriptedCompleter::new(vec![
            tool_call_response("nope", json!({})),
            text_response("recovered"),
        ]));
        let agent = Agent::new(completer.clone()).with_tool(Arc::new(EchoTool));

        let out = agent.run("hi".to_string()).await.unwrap();
        assert_eq!(out, "recovered");

        // The dispatch error is fed back as a tool result, so the model gets a
        // second call and the transcript grows to [user, tool call, result].
        let seen = completer.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].len(), 3);
    }

    #[tokio::test]
    async fn agent_stops_after_max_rounds() {
        let script: Vec<CompletionResponse<()>> = (0..5)
            .map(|_| tool_call_response("echo", json!({ "text": "x" })))
            .collect();
        let completer = Arc::new(ScriptedCompleter::new(script));
        let agent = Agent::new(completer.clone())
            .with_tool(Arc::new(EchoTool))
            .max_rounds(2);

        let err = agent.run("hi".to_string()).await.unwrap_err();
        assert!(err.contains("rounds"), "unexpected error: {err}");
    }

    #[test]
    fn echo_tool_exposes_name_and_schema() {
        assert_eq!(EchoTool.name(), "echo");
        assert_eq!(EchoTool.parameters()["type"], "object");
        assert_eq!(EchoTool.parameters()["required"][0], "text");
    }
}
