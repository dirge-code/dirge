//! /memory handler — reload the frozen snapshot mid-session.

use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_memory(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let sub = parts.get(1).copied().unwrap_or("").trim();
    match sub {
        "reload" => {
            let provider = match ctx.agent.memory_provider() {
                Some(p) => p,
                None => {
                    ctx.renderer
                        .write_line("no memory provider loaded", c_error())?;
                    return Ok(());
                }
            };
            match provider.refresh_snapshot() {
                Ok(()) => {
                    ctx.renderer
                        .write_line("memory snapshot refreshed", c_agent())?;
                }
                Err(e) => {
                    ctx.renderer
                        .write_line(&format!("refresh failed: {e}"), c_error())?;
                }
            }
        }
        "" => {
            ctx.renderer.write_line(
                "/memory reload   — refresh the frozen snapshot so recent writes appear in the prompt",
                c_agent(),
            )?;
        }
        other => {
            ctx.renderer
                .write_line(&format!("unknown /memory sub-command: {other}"), c_error())?;
        }
    }
    Ok(())
}
