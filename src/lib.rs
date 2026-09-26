//! `dirge-agent` library crate.
//!
//! The binary (`src/main.rs`) is a native TUI. This library exposes the pure,
//! wasm-able core for non-native targets. On `wasm32-unknown-unknown` only
//! modules that compile without a terminal, SQLite, process spawning, or a
//! native TLS backend are available.
//!
//! Build the wasm slice with:
//!
//! ```sh
//! cargo build --lib --target wasm32-unknown-unknown --no-default-features --features wasm
//! ```

// The inlined llmtrim engine is self-contained and wasm-verified (see its
// module docs). It is the first slice of the wasm-able core; provider and
// agent-loop modules follow as they shed their native dependencies.
pub mod llmtrim;

// Minimal, swappable agent loop + tool seam (see its module docs). Pure Rust,
// so it is natively testable via `cargo test --lib` and, on wasm, driven by
// the DeepSeek-backed export below.
pub mod agent_core;

// JS interop for the browser/Node build. Enabled only by the `wasm` feature
// on a wasm target, so native builds and `--all-features` native checks never
// see the `wasm_bindgen` macros.
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
use std::sync::Arc;
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
use wasm_bindgen::prelude::*;

/// Approximate token count for a text string, using dirge's llmtrim
/// BPE-shaped estimator (the same counter used for Anthropic/Google requests
/// on the native side). Returns the token count as a JS number.
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
#[wasm_bindgen]
pub fn token_count(text: &str) -> u32 {
    crate::llmtrim::tokenizer::counter_for(crate::llmtrim::ir::ProviderKind::Anthropic, None)
        .map(|counter| counter.count(text) as u32)
        .unwrap_or(0)
}

/// BYOK chat completion over DeepSeek. Takes a caller-supplied API key so no
/// credential leaves the JS side (browser BYOK), then runs dirge's rig-based
/// DeepSeek provider over the wasm `fetch` transport. Returns the assistant
/// text as a JS Promise.
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
#[wasm_bindgen]
pub async fn chat(api_key: String, prompt: String) -> Result<String, JsValue> {
    use rig::client::CompletionClient;
    use rig::completion::{AssistantContent, CompletionModel};
    use rig::providers::deepseek;

    let client = deepseek::Client::new(api_key).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let model = client.completion_model(deepseek::DEEPSEEK_V4_FLASH);
    let request = model.completion_request(prompt).build();
    let response = model
        .completion(request)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    for item in response.choice {
        if let AssistantContent::Text(text) = item {
            return Ok(text.text);
        }
    }
    Ok(String::new())
}

/// DeepSeek backend for the [`crate::agent_core::Agent`] loop. Adapts a rig
/// DeepSeek completion model to the loop's [`crate::agent_core::Completer`]
/// seam (full transcript + tool definitions -> one completion).
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
struct DeepSeekCompleter {
    model: rig::providers::deepseek::CompletionModel,
}

#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
impl DeepSeekCompleter {
    fn new(api_key: String) -> Result<Self, String> {
        use rig::client::CompletionClient;
        use rig::providers::deepseek;

        let client = deepseek::Client::new(api_key).map_err(|e| e.to_string())?;
        Ok(Self {
            model: client.completion_model(deepseek::DEEPSEEK_V4_FLASH),
        })
    }
}

#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
impl crate::agent_core::Completer for DeepSeekCompleter {
    fn complete(
        &self,
        messages: Vec<rig::completion::Message>,
        tools: Vec<rig::completion::ToolDefinition>,
    ) -> rig::wasm_compat::WasmBoxedFuture<
        '_,
        Result<rig::completion::CompletionResponse<()>, String>,
    > {
        use rig::completion::{CompletionModel, CompletionResponse};

        let (last, prior) = match messages.split_last() {
            Some(split) => split,
            None => {
                return Box::pin(async move { Err("empty transcript".to_string()) });
            }
        };
        let request = self
            .model
            .completion_request(last.clone())
            .messages(prior.to_vec())
            .tools(tools)
            .build();

        Box::pin(async move {
            let response = self
                .model
                .completion(request)
                .await
                .map_err(|e| e.to_string())?;
            Ok(CompletionResponse {
                choice: response.choice,
                usage: response.usage,
                raw_response: (),
                message_id: response.message_id,
            })
        })
    }
}

/// BYOK agent turn over DeepSeek with tool-calling. Builds the minimal
/// [`crate::agent_core::Agent`] loop with the built-in `EchoTool` and returns
/// the final assistant text as a JS Promise. The tool set is swappable; this
/// export is the seed for a JS-registered tool registry (shell/fs/process
/// shims supplied by the host).
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
#[wasm_bindgen]
pub async fn agent_chat(api_key: String, prompt: String) -> Result<String, JsValue> {
    use std::sync::Arc;

    let completer = Arc::new(DeepSeekCompleter::new(api_key).map_err(|e| JsValue::from_str(&e))?);
    let agent =
        crate::agent_core::Agent::new(completer).with_tool(Arc::new(crate::agent_core::EchoTool));
    agent.run(prompt).await.map_err(|e| JsValue::from_str(&e))
}

/// A [`crate::agent_core::Tool`] whose implementation lives in JS. The host
/// supplies a function `(argsJson: string) => string | Promise<string>`; the
/// loop calls it exactly like a native tool, so shell/fs/process shims can be
/// registered from the browser/Node side with no Rust changes.
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
struct JsTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    func: js_sys::Function,
}

#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
impl crate::agent_core::Tool for JsTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> rig::wasm_compat::WasmBoxedFuture<'_, Result<String, String>> {
        let func = self.func.clone();
        let args_json = args.to_string();
        Box::pin(async move {
            let arg = JsValue::from_str(&args_json);
            let result = func
                .call1(&JsValue::NULL, &arg)
                .map_err(|e| format!("tool invocation failed: {e:?}"))?;
            // Promise::resolve adopts a thenable, so this also awaits the
            // result when the host function returns a plain string.
            let resolved = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(&result))
                .await
                .map_err(|e| format!("tool rejected: {e:?}"))?;
            Ok(resolved
                .as_string()
                .unwrap_or_else(|| format!("{resolved:?}")))
        })
    }
}

/// A stateful wasm agent the host extends with JS-implemented tools. Build one,
/// register tools with `add_js_tool`, then `run` a prompt; the DeepSeek-backed
/// loop dispatches to whichever tools the model calls.
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
#[wasm_bindgen]
pub struct AgentHandle {
    api_key: String,
    tools: Vec<Arc<dyn crate::agent_core::Tool>>,
}

#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
#[wasm_bindgen]
impl AgentHandle {
    #[wasm_bindgen(constructor)]
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            tools: Vec::new(),
        }
    }

    /// Register the built-in `EchoTool` so the agent has a tool without the
    /// host supplying any JS.
    pub fn add_echo_tool(&mut self) {
        self.tools.push(Arc::new(crate::agent_core::EchoTool));
    }

    /// Register a JS-implemented tool. `parameters` is a JSON Schema string;
    /// `func` is `(argsJson: string) => string | Promise<string>`.
    pub fn add_js_tool(
        &mut self,
        name: String,
        description: String,
        parameters: String,
        func: js_sys::Function,
    ) -> Result<(), JsValue> {
        let parameters: serde_json::Value = serde_json::from_str(&parameters)
            .map_err(|e| JsValue::from_str(&format!("invalid tool schema: {e}")))?;
        self.tools.push(Arc::new(JsTool {
            name,
            description,
            parameters,
            func,
        }));
        Ok(())
    }

    /// Dispatch a registered tool directly (without the model). Exposes the JS
    /// tool round-trip for tests and host-side use.
    pub async fn call_tool(&self, name: String, args: String) -> Result<String, JsValue> {
        let args: serde_json::Value = serde_json::from_str(&args)
            .map_err(|e| JsValue::from_str(&format!("invalid tool args: {e}")))?;
        for tool in &self.tools {
            if tool.name() == name {
                return tool.call(args).await.map_err(|e| JsValue::from_str(&e));
            }
        }
        Err(JsValue::from_str(&format!("unknown tool: {name}")))
    }

    /// Run one user turn with the current tool set over DeepSeek, returning the
    /// final assistant text.
    pub async fn run(&self, prompt: String) -> Result<String, JsValue> {
        let completer = Arc::new(
            DeepSeekCompleter::new(self.api_key.clone()).map_err(|e| JsValue::from_str(&e))?,
        );
        let mut agent = crate::agent_core::Agent::new(completer);
        for tool in &self.tools {
            agent = agent.with_tool(tool.clone());
        }
        agent.run(prompt).await.map_err(|e| JsValue::from_str(&e))
    }
}
