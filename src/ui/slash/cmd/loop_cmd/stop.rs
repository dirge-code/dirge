//! /loop stop — stop the active loop.

use crate::ui::slash::{SlashCtx, c_agent};

pub(crate) async fn cmd_loop_stop(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    #[cfg(feature = "loop")]
    {
        if let Some(ls) = ctx.loop_state.as_mut() {
            ls.active = false;
            ctx.renderer.write_line("loop stopped", c_agent())?;
        } else {
            ctx.renderer.write_line("no active loop", c_agent())?;
        }
    }
    #[cfg(not(feature = "loop"))]
    ctx.renderer.write_line(
        "/loop requires the 'loop' feature: cargo build --features loop",
        c_agent(),
    )?;
    Ok(())
}
