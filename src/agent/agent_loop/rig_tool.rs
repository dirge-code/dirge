//! Phase 4.5b — adapter from `rig::tool::ToolDyn` to our pi-style
//! `LoopTool`.
//!
//! Every dirge tool already implements `rig::Tool` (which auto-
//! derives `ToolDyn`). This adapter wraps any `Box<dyn ToolDyn>`
//! so it can be registered with the new loop's `Context.tools`
//! without per-tool re-implementation.
//!
//! Surface mapping (`rig::ToolDyn` → `LoopTool`):
//!
//! | LoopTool method  | Source                                          |
//! |------------------|-------------------------------------------------|
//! | `name()`         | Cached at construction from `ToolDefinition`    |
//! | `description()`  | Cached at construction from `ToolDefinition`    |
//! | `label()`        | Same as `name` — rig has no separate label      |
//! | `parameters()`   | Cached at construction from `ToolDefinition`    |
//! | `execution_mode()` | Configured per-adapter; defaults to None       |
//! | `prepare_arguments()` | Identity — rig tools self-parse via serde   |
//! | `execute()`      | JSON-encode args; call `inner.call(s)`; wrap    |
//!
//! Rig's `ToolDyn::definition()` is async, so we EAGERLY resolve
//! the definition once at adapter construction. `RigToolAdapter::new`
//! is therefore async. Callers build the adapter when they build
//! the agent (existing dirge code already does this asynchronously).
//!
//! Tools that mutate shared state (filesystem, bash) should set
//! `execution_mode = Sequential` to force the whole batch
//! sequential per phase 3's umbrella dispatcher. Read-only tools
//! (grep, list_dir, find_files) can leave it at the default and
//! benefit from parallel dispatch.

use std::pin::Pin;

use rig::tool::{ToolDyn, ToolError};
use serde_json::Value;

use super::result::LoopToolResult;
use super::schema_flatten::{FlattenDecision, analyze_schema, flatten_schema, nest_arguments};
use super::tool::{AbortSignal, LoopTool, LoopToolUpdate};

use super::types::ToolExecutionMode;
#[cfg(test)]
use std::sync::Arc;

/// Wraps a `Box<dyn rig::ToolDyn>` and exposes it as a `LoopTool`.
///
/// Built via [`RigToolAdapter::new`] which eagerly resolves the
/// tool's definition (name, description, parameters). The wrapped
/// tool is shared via `Arc` because `LoopTool` impls are stored as
/// `Arc<dyn LoopTool>` in `Context.tools` (see `types.rs`).
///
/// Schema flattening: if the tool's JSON Schema has >10 leaf params
/// or >2 levels of nesting, the adapter auto-flattens it at
/// construction time (Reasonix tools.ts:36-38). The LLM sees flat
/// dot-notation keys; `prepare_arguments` re-nests them at dispatch.
pub struct RigToolAdapter {
    inner: Box<dyn ToolDyn>,
    name: String,
    description: String,
    parameters: Value,
    /// Flattened parameters stored when the schema warrants flattening.
    /// Port of Reasonix `InternalTool.flatSchema` (tools.ts:37).
    flat_parameters: Option<Value>,
    execution_mode: Option<ToolExecutionMode>,
}

impl std::fmt::Debug for RigToolAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RigToolAdapter")
            .field("name", &self.name)
            .field("execution_mode", &self.execution_mode)
            .field("has_flat_schema", &self.flat_parameters.is_some())
            .finish()
    }
}

impl RigToolAdapter {
    /// Build an adapter, eagerly resolving the tool's definition.
    ///
    /// Async because `rig::ToolDyn::definition` is. We resolve
    /// with an empty prompt; rig tools that condition their
    /// definition on the user prompt will see "" here. None of
    /// dirge's current tools do that — if a future tool ever
    /// needs prompt-conditional definitions, the loop will need
    /// to expose a per-turn definition rebuild path. Documented
    /// as a deferred concern.
    ///
    /// Auto-analyzes the schema; if it's deep (>2) or wide (>10
    /// leaves), flattens it and stores the flat variant so
    /// `prepare_arguments` can re-nest at dispatch (Reasonix
    /// tools.ts:36-38).
    pub async fn new(inner: Box<dyn ToolDyn>) -> Self {
        let def = inner.definition(String::new()).await;
        let flat_parameters = match analyze_schema(&def.parameters) {
            FlattenDecision {
                should_flatten: true,
                ..
            } => Some(flatten_schema(&def.parameters)),
            _ => None,
        };
        Self {
            inner,
            name: def.name,
            description: def.description,
            parameters: def.parameters,
            flat_parameters,
            execution_mode: None,
        }
    }
    /// that mutate shared filesystem state or process state
    /// (bash, edit, write, apply_patch) to prevent concurrent
    /// races. Phase 3's umbrella dispatcher detects this and
    /// forces the WHOLE batch sequential.
    pub fn with_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.execution_mode = Some(mode);
        self
    }

    /// Construct directly from owned strings + a tool — useful
    /// in tests that want to bypass the async `definition` call.
    /// Production callers should use `new`.
    #[cfg(test)]
    fn from_parts(
        inner: Box<dyn ToolDyn>,
        name: String,
        description: String,
        parameters: Value,
    ) -> Self {
        Self {
            inner,
            name,
            description,
            parameters,
            flat_parameters: None,
            execution_mode: None,
        }
    }
}

impl LoopTool for RigToolAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn label(&self) -> &str {
        // rig has no separate label; use the name. If a future
        // dirge concept introduces UI-display labels per tool,
        // add a `with_label` builder.
        &self.name
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn flat_parameters(&self) -> Option<&Value> {
        self.flat_parameters.as_ref()
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.execution_mode
    }

    /// Re-nests flat dot-notation args when the schema was
    /// flattened. Port of Reasonix `dispatch()` re-nest
    /// (tools.ts:126-130). Otherwise identity.
    fn prepare_arguments(&self, args: Value) -> Value {
        if self.flat_parameters.is_some() {
            nest_arguments(&args)
        } else {
            args
        }
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        args: Value,
        _signal: AbortSignal,
        _on_update: LoopToolUpdate,
    ) -> Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>> {
        Box::pin(async move {
            // Rig's `call` takes a JSON string. Serialize the
            // already-parsed `Value` back to string.
            let args_string = match serde_json::to_string(&args) {
                Ok(s) => s,
                Err(e) => return Err(format!("rig adapter: arg serialization failed: {e}")),
            };

            match self.inner.call(args_string).await {
                Ok(output_text) => {
                    // Rig tools return a plain string (the model-
                    // facing payload). Wrap it as a single text
                    // content block. `details` carries the raw
                    // string so structured consumers (e.g. UI
                    // file-card rendering) can dispatch on it.
                    Ok(LoopToolResult {
                        content: vec![serde_json::json!({
                            "type": "text",
                            "text": output_text,
                        })],
                        details: Value::String(output_text),
                        // Rig has no terminate hint. None — the
                        // afterToolCall hook can still mark
                        // terminate per pi:1184.
                        terminate: None,
                    })
                }
                Err(err) => Err(format_tool_error(err)),
            }
        })
    }
}

/// Convert rig's `ToolError` to the `String` shape `LoopTool::execute`
/// returns. Schema errors are caught earlier by `prepare_tool_call`'s
/// repair layer; this function handles runtime tool errors. If a
/// schema error leaks through (defense-in-depth), wrap it in a
/// model-readable retry hint rather than leaking raw serde diagnostics.
fn format_tool_error(err: ToolError) -> String {
    let raw = err.to_string();
    if raw.contains("missing field") || raw.contains("expected") || raw.contains("invalid type") {
        format!(
            "Tool input rejected: the arguments did not match the tool's schema.\n\
             Try: re-check the tool's required fields and types, then retry.\n\
             Details: {raw}"
        )
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::ToolDefinition;
    use rig::tool::Tool;
    use serde::{Deserialize, Serialize};

    /// Mock rig tool that echoes its input back. Mirrors the
    /// shape of a real dirge tool (impl rig::Tool); rig
    /// auto-derives ToolDyn from this.
    #[derive(Debug, Clone)]
    struct EchoTool;

    #[derive(Debug, Deserialize, Serialize)]
    struct EchoArgs {
        value: String,
    }

    #[derive(Debug, thiserror::Error)]
    enum EchoError {
        #[error("echo failed: {0}")]
        Generic(String),
    }

    impl Tool for EchoTool {
        const NAME: &'static str = "echo";
        type Error = EchoError;
        type Args = EchoArgs;
        type Output = String;

        async fn definition(&self, _prompt: String) -> ToolDefinition {
            ToolDefinition {
                name: "echo".to_string(),
                description: "Echo the input back".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "value": {"type": "string"}
                    },
                    "required": ["value"]
                }),
            }
        }

        async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
            Ok(format!("echoed: {}", args.value))
        }
    }

    /// Tool that returns an error from `call`.
    #[derive(Debug, Clone)]
    struct FailingTool;

    impl Tool for FailingTool {
        const NAME: &'static str = "failing";
        type Error = EchoError;
        type Args = EchoArgs;
        type Output = String;

        async fn definition(&self, _prompt: String) -> ToolDefinition {
            ToolDefinition {
                name: "failing".to_string(),
                description: "Always fails".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
            Err(EchoError::Generic("synthetic failure".to_string()))
        }
    }

    /// Build a `LoopToolUpdate` callback that records calls. None
    /// of the tests below trigger it (rig has no on-update
    /// channel), but the signature requires a closure.
    fn dummy_update() -> LoopToolUpdate {
        Arc::new(|_partial: &LoopToolResult| {})
    }

    /// Adapter resolves definition at construction and exposes
    /// the cached fields via the sync `LoopTool` accessors.
    #[tokio::test]
    async fn adapter_caches_definition_at_construction() {
        let adapter = RigToolAdapter::new(Box::new(EchoTool)).await;
        assert_eq!(adapter.name(), "echo");
        assert_eq!(adapter.description(), "Echo the input back");
        assert_eq!(adapter.label(), "echo"); // == name; no separate label
        assert_eq!(adapter.parameters()["type"], "object");
        // execution_mode is None by default — picks up the loop's
        // config-level default (Parallel).
        assert!(adapter.execution_mode().is_none());
    }

    /// `with_execution_mode` overrides the default to Sequential.
    /// Tools that mutate shared state set this to force the
    /// batch sequential.
    #[tokio::test]
    async fn adapter_with_execution_mode_overrides_default() {
        let adapter = RigToolAdapter::new(Box::new(EchoTool))
            .await
            .with_execution_mode(ToolExecutionMode::Sequential);
        assert_eq!(
            adapter.execution_mode(),
            Some(ToolExecutionMode::Sequential)
        );
    }

    /// Happy-path execute: args round-trip through serde and the
    /// echoed output appears in `LoopToolResult.content`.
    #[tokio::test]
    async fn execute_happy_path_wraps_output() {
        let adapter = RigToolAdapter::new(Box::new(EchoTool)).await;
        let result = adapter
            .execute(
                "call-1",
                serde_json::json!({"value": "hello"}),
                AbortSignal::new(),
                dummy_update(),
            )
            .await
            .expect("execute should succeed");

        // Content has one text block with the echoed text.
        assert_eq!(result.content.len(), 1);
        assert_eq!(result.content[0]["type"], "text");
        assert_eq!(result.content[0]["text"], "echoed: hello");
        // Details carry the raw output string.
        assert_eq!(result.details, Value::String("echoed: hello".to_string()));
        // No terminate hint from rig.
        assert!(result.terminate.is_none());
    }

    /// Tool error → adapter returns Err(string). The dispatcher
    /// then wraps this in `create_error_tool_result` and marks
    /// `is_error: true` so the LLM sees the failure.
    #[tokio::test]
    async fn execute_propagates_tool_error() {
        let adapter = RigToolAdapter::new(Box::new(FailingTool)).await;
        let result = adapter
            .execute(
                "call-1",
                serde_json::json!({"value": "x"}),
                AbortSignal::new(),
                dummy_update(),
            )
            .await;
        let err_string = result.expect_err("execute should fail");
        assert!(
            err_string.contains("synthetic failure"),
            "expected error string to mention the underlying message: {err_string}"
        );
    }

    /// Malformed args (missing required field) → rig's serde
    /// deserialization fails inside `call()`. The adapter
    /// surfaces that as Err. This is rig's normal behavior; the
    /// adapter doesn't add extra schema validation (deferred to
    /// phase 2's prepare_tool_call which we already noted skips
    /// schema validation).
    #[tokio::test]
    async fn execute_with_malformed_args_returns_error() {
        let adapter = RigToolAdapter::new(Box::new(EchoTool)).await;
        let result = adapter
            .execute(
                "call-1",
                serde_json::json!({}), // missing `value`
                AbortSignal::new(),
                dummy_update(),
            )
            .await;
        assert!(result.is_err(), "missing required arg should produce error");
    }

    /// `prepare_arguments` is identity — rig tools self-parse so
    /// no shim is needed. Verifies the default `LoopTool` impl
    /// passes through unchanged (matches pi's "no prepareArguments
    /// hook" default at agent-loop.ts:548-560).
    #[tokio::test]
    async fn prepare_arguments_is_identity() {
        let adapter = RigToolAdapter::new(Box::new(EchoTool)).await;
        let input = serde_json::json!({"value": "x", "extra": "y"});
        let output = adapter.prepare_arguments(input.clone());
        assert_eq!(output, input);
    }

    /// `from_parts` lets tests build adapters without paying the
    /// async definition resolution. Verifies the constructor
    /// works.
    #[tokio::test]
    async fn from_parts_builds_adapter() {
        let adapter = RigToolAdapter::from_parts(
            Box::new(EchoTool),
            "custom_name".to_string(),
            "custom desc".to_string(),
            serde_json::json!({"type": "object"}),
        );
        assert_eq!(adapter.name(), "custom_name");
        assert_eq!(adapter.description(), "custom desc");
    }

    /// Integration test: wrap a REAL dirge tool (`ReadTool`) and
    /// verify the adapter path produces the same output as the
    /// rig direct path.
    ///
    /// Setup: create a temp file with known content. Read via
    /// rig directly (`tool.call(json_args)`). Read via adapter
    /// (`adapter.execute(value_args, ...)`). The text content
    /// returned by each path must match.
    #[tokio::test]
    async fn adapter_matches_rig_path_for_real_dirge_tool() {
        use crate::agent::tools::ReadTool;
        use rig::tool::ToolDyn;

        // Set up a temp file with known content. Reuses the
        // same TestDir pattern as fs_atomic tests for cleanup.
        let dir = std::env::temp_dir().join(format!(
            "dirge_rig_tool_test_{}_{}",
            std::process::id(),
            crate::time_util::now_unix_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("sample.txt");
        std::fs::write(&target, b"hello from integration test\n").unwrap();
        let path_str = target.to_string_lossy().into_owned();

        // Path A: rig direct call via ToolDyn (the dyn-safe
        // surface dirge already uses everywhere). Disambiguate
        // against `Tool::call` (which takes typed args) since
        // both traits are in scope.
        let tool_a = ReadTool::new(None, None);
        let rig_args = serde_json::json!({"path": path_str}).to_string();
        let rig_output = <ReadTool as ToolDyn>::call(&tool_a, rig_args)
            .await
            .expect("rig direct call should succeed");

        // Path B: through the adapter.
        let tool_b = ReadTool::new(None, None);
        let adapter = RigToolAdapter::new(Box::new(tool_b)).await;
        let adapter_result = adapter
            .execute(
                "call-1",
                serde_json::json!({"path": path_str}),
                AbortSignal::new(),
                dummy_update(),
            )
            .await
            .expect("adapter execute should succeed");

        // The text content (LLM-visible payload) MUST match
        // verbatim. Adapter wraps rig's string output in a
        // single text block; extract and compare.
        assert_eq!(adapter_result.content.len(), 1);
        let adapter_text = adapter_result.content[0]["text"]
            .as_str()
            .expect("text field");
        assert_eq!(
            adapter_text, rig_output,
            "adapter must produce the same text as the rig direct path"
        );

        // Cleanup.
        let _ = std::fs::remove_dir_all(&dir);
    }
}
