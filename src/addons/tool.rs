//! Addon tools as agent-loop tools.
//!
//! [`AddonLoopTool`] adapts one [`ToolSpec`] onto [`LoopTool`]. A call is
//! authorized exactly like a Janet plugin tool (the `plugin_tool` umbrella,
//! `Operation::Plugin`: ask by default, never builtin-allowed), then run on
//! the host's runtime from a blocking task, since the isolate answers
//! synchronously.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::agent::agent_loop::result::LoopToolResult;
use crate::agent::agent_loop::tool::{AbortSignal, LoopTool, LoopToolUpdate};
use crate::agent::tools::{Scope, enforce};
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
#[allow(unused_imports)]
use crate::sync_util::LockExt;

use super::domain::ToolSpec;
use super::host::AddonHost;

/// Permission umbrella shared with Janet plugin tools.
const PERMISSION_TOOL: &str = "plugin_tool";

/// [`LoopTool::source`] of every addon tool: `/addons reload` swaps exactly
/// these in a live agent.
pub const SOURCE: &str = "addon";

pub struct AddonLoopTool {
    spec: ToolSpec,
    host: Arc<AddonHost>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
}

impl std::fmt::Debug for AddonLoopTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddonLoopTool")
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

impl AddonLoopTool {
    pub fn new(
        spec: ToolSpec,
        host: Arc<AddonHost>,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        Self {
            spec,
            host,
            permission,
            ask_tx,
        }
    }
}

/// Every tool the host offers, adapted.
pub fn loop_tools(
    host: &Arc<AddonHost>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
) -> Vec<AddonLoopTool> {
    host.tools()
        .iter()
        .cloned()
        .map(|spec| AddonLoopTool::new(spec, host.clone(), permission.clone(), ask_tx.clone()))
        .collect()
}

impl LoopTool for AddonLoopTool {
    fn name(&self) -> &str {
        self.spec.model_name()
    }

    fn description(&self) -> &str {
        &self.spec.description
    }

    fn label(&self) -> &str {
        self.spec.model_name()
    }

    fn parameters(&self) -> &Value {
        &self.spec.input_schema
    }

    fn source(&self) -> Option<&str> {
        Some(SOURCE)
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        args: Value,
        signal: AbortSignal,
        _on_update: LoopToolUpdate,
    ) -> Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>> {
        Box::pin(async move {
            if signal.is_cancelled() {
                return Err("addon tool aborted before execution".to_string());
            }
            let name = self.spec.model_name();
            if let Some(perm) = self.permission.as_ref() {
                let denied = perm
                    .lock_ignore_poison()
                    .any_prompt_denied(&[name, PERMISSION_TOOL]);
                if denied {
                    return Err(format!(
                        "Addon tool `{name}` is denied by the active prompt's `deny_tools` \
                         frontmatter. Switch with `/prompt <other>` to use it."
                    ));
                }
            }
            enforce(
                &self.permission,
                &self.ask_tx,
                PERMISSION_TOOL,
                Scope::Raw(name),
            )
            .await
            .map_err(|e| e.to_string())?;
            let host = self.host.clone();
            let spec = self.spec.clone();
            let (content, details) =
                tokio::task::spawn_blocking(move || host.call_tool(&spec, &args))
                    .await
                    .map_err(|e| format!("addon tool task failed: {e}"))??;
            Ok(LoopToolResult {
                content,
                details,
                terminate: None,
            })
        })
    }
}
