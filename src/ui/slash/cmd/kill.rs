//! /kill handler.

use crate::agent::tools::task::{KillOutcome, kill_subagent};
use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_kill(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let prefix = parts.get(1).copied().unwrap_or("").trim();
    if prefix.is_empty() {
        ctx.renderer
            .write_line("usage: /kill <id-prefix>", c_error())?;
        return Ok(());
    }
    match kill_subagent(prefix) {
        KillOutcome::Killed(id) => {
            ctx.renderer
                .write_line(&format!("killed {}", id), c_agent())?;
        }
        KillOutcome::NotFound => {
            ctx.renderer
                .write_line(&format!("no subagent matches '{}'", prefix), c_error())?;
        }
        KillOutcome::Ambiguous(ids) => {
            ctx.renderer.write_line(
                &format!("ambiguous prefix '{}'; matches: {}", prefix, ids.join(" ")),
                c_error(),
            )?;
        }
    }
    Ok(())
}
