//! /vigil status — show all vigils and their state.

use crate::ui::slash::{SlashCtx, c_agent, c_error};

use tokio::sync::oneshot;

pub(crate) async fn cmd_vigil_status(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let Some(ctl_tx) = ctx.vigil_ctl_tx else {
        ctx.renderer
            .write_line("vigil keeper not running", c_error())?;
        return Ok(());
    };

    let (tx, rx) = oneshot::channel();
    let _ = ctl_tx
        .send(crate::extras::vigil::types::VigilCtl::StatusReq { respond_to: tx })
        .await;

    let statuses = match rx.await {
        Ok(s) => s,
        Err(_) => {
            ctx.renderer
                .write_line("vigil keeper did not respond", c_error())?;
            return Ok(());
        }
    };

    if statuses.is_empty() {
        ctx.renderer.write_line("no vigils configured", c_agent())?;
        return Ok(());
    }

    for info in &statuses {
        let trigger = info.trigger.as_str();
        let state = if info.paused { "paused" } else { "active" };
        ctx.renderer.write_line(
            &format!(
                "  {}  trigger={}  reap={}s  {}",
                info.name, trigger, info.reap_interval_secs, state
            ),
            c_agent(),
        )?;
    }

    let active = statuses.iter().filter(|i| !i.paused).count();
    let paused = statuses.iter().filter(|i| i.paused).count();
    ctx.renderer.write_line(
        &format!(
            "{} vigil(s): {} active, {} paused",
            statuses.len(),
            active,
            paused,
        ),
        c_agent(),
    )?;

    Ok(())
}
