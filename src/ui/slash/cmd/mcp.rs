//! /mcp handler.

use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

#[cfg(feature = "mcp")]
pub(crate) async fn cmd_mcp(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let Some(mgr) = ctx.mcp_manager else {
        ctx.renderer
            .write_line("no MCP servers configured", c_agent())?;
        return Ok(());
    };
    let connections = mgr.connections_snapshot();
    if connections.is_empty() {
        ctx.renderer
            .write_line("no MCP servers connected", c_agent())?;
    } else if parts.len() == 1 {
        ctx.renderer.write_line("MCP servers:", c_agent())?;
        for (server_name, conn) in &connections {
            match crate::extras::mcp::client::list_tools(conn).await {
                Ok(tools) => {
                    ctx.renderer.write_line(
                        &format!("  {} ({} tools)", server_name, tools.len()),
                        c_result(),
                    )?;
                }
                Err(e) => {
                    ctx.renderer
                        .write_line(&format!("  {} (error: {})", server_name, e), c_error())?;
                }
            }
        }
    } else {
        let name = parts[1].trim();
        if let Some(conn) = connections.iter().find(|(n, _)| n == name).map(|(_, c)| c) {
            match crate::extras::mcp::client::list_tools(conn).await {
                Ok(tools) => {
                    if tools.is_empty() {
                        ctx.renderer
                            .write_line(&format!("server '{}' has no tools", name), c_agent())?;
                    } else {
                        ctx.renderer
                            .write_line(&format!("tools on '{}':", name), c_agent())?;
                        for tool in &tools {
                            let desc = tool.description.as_deref().unwrap_or("");
                            ctx.renderer
                                .write_line(&format!("  {}  {}", tool.name, desc), c_result())?;
                        }
                    }
                }
                Err(e) => {
                    ctx.renderer.write_line(
                        &format!("error listing tools on '{}': {}", name, e),
                        c_error(),
                    )?;
                }
            }
        } else {
            ctx.renderer
                .write_line(&format!("unknown MCP server: '{}'", name), c_error())?;
        }
    }
    Ok(())
}
