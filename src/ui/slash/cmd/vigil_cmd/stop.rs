//! /vigil stop — stop one vigil or all vigils.

use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_vigil_stop(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let name = parts.get(2).copied().unwrap_or("");

    let Some(ctl_tx) = ctx.vigil_ctl_tx else {
        ctx.renderer
            .write_line("vigil keeper not running", c_error())?;
        return Ok(());
    };

    if name.is_empty() {
        let _ = ctl_tx
            .send(crate::extras::vigil::types::VigilCtl::PauseAll)
            .await;
        ctx.renderer.write_line("stopped all vigils", c_agent())?;
    } else {
        let _ = ctl_tx
            .send(crate::extras::vigil::types::VigilCtl::Pause {
                name: name.to_string(),
            })
            .await;
        ctx.renderer
            .write_line(&format!("vigil '{name}' stopped"), c_agent())?;
    }
    Ok(())
}
