//! /sandbox reboot / start handler.

use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

pub(crate) async fn cmd_sandbox_reboot(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if !ctx.sandbox.is_microvm() {
        ctx.renderer.write_line(
            "microVM sandbox not active — start dirge with --sandbox microvm.",
            c_error(),
        )?;
        return Ok(());
    }
    ctx.renderer.write_line("rebooting microVM...", c_agent())?;
    match ctx.sandbox.reboot_microvm().await {
        Ok(()) => {
            ctx.renderer
                .write_line("microVM rebooted — fresh VM is ready.", c_result())?;
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("reboot failed: {e}"), c_error())?;
        }
    }
    Ok(())
}
