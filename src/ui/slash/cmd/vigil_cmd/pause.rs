//! /vigil pause — pause a vigil (keep config, don't fire).

use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_vigil_pause(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let name = parts.get(2).copied().unwrap_or("");
    if name.is_empty() {
        ctx.renderer
            .write_line("usage: /vigil pause <name>", c_error())?;
        return Ok(());
    }
    if let Some(tx) = ctx.vigil_ctl_tx {
        let _ = tx
            .send(crate::extras::vigil::types::VigilCtl::Pause {
                name: name.to_string(),
            })
            .await;
        ctx.renderer
            .write_line(&format!("vigil '{}' paused", name), c_agent())?;
    } else {
        ctx.renderer
            .write_line("vigil keeper not running", c_error())?;
    }
    Ok(())
}
