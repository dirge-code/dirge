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

// JS interop for the browser/Node build. Enabled only by the `wasm` feature
// on a wasm target, so native builds and `--all-features` native checks never
// see the `wasm_bindgen` macros.
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
