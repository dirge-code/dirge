//! /loop status — show loop state.

use crate::ui::slash::{SlashCtx, c_agent};

pub(crate) async fn cmd_loop_status(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    #[cfg(feature = "loop")]
    {
        if let Some(ls) = ctx.loop_state.as_ref() {
            let status = if ls.active { "active" } else { "stopped" };
            ctx.renderer.write_line(
                &format!(
                    "loop {}: {} ({})",
                    status,
                    ls.iteration_label(),
                    ls.plan_file.display()
                ),
                c_agent(),
            )?;
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
