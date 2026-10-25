//! Facts about a session, read where they live.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use super::domain::StartFacts;

/// The MCP servers connected once the configured ones have had up to the
/// given time to connect.
pub type McpServersFn =
    Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = Vec<String>> + Send>> + Send + Sync>;

/// dirge's working directory; empty when it cannot be read.
pub fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// What a run of `session_id` knows when it is spawned. `first_prompt` is
/// true when the session has no earlier conversation.
pub fn start_facts(session_id: Option<&str>, first_prompt: bool) -> StartFacts {
    StartFacts {
        session_id: session_id.map(str::to_string),
        cwd: cwd(),
        first_prompt,
    }
}

/// The MCP servers dirge is connected to, as addon code reaches them.
#[cfg(all(feature = "addons", feature = "mcp"))]
pub fn mcp_servers() -> McpServersFn {
    Arc::new(|wait| Box::pin(crate::addons::mcp::connected(wait)))
}

/// No MCP servers: this build has no MCP client.
#[cfg(all(feature = "addons", not(feature = "mcp")))]
pub fn mcp_servers() -> McpServersFn {
    Arc::new(|_| Box::pin(async { Vec::new() }))
}
