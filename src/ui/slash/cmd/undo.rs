//! /undo handler.

use crate::ui::events::render_session;
use crate::ui::slash::{SlashCtx, c_agent, c_error, undo_last};

pub(crate) async fn cmd_undo(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let outcome = undo_last(ctx.session);
    if outcome.removed > 0 {
        render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
        ctx.renderer.write_line(
            &format!("removed {} message(s)", outcome.removed),
            c_agent(),
        )?;
        if outcome.had_tool_calls {
            ctx.renderer.write_line(
                "warning: tool side effects (file writes, bash, MCP) were NOT reverted",
                c_error(),
            )?;
        }
    } else {
        ctx.renderer.write_line("nothing to undo", c_agent())?;
    }
    Ok(())
}
