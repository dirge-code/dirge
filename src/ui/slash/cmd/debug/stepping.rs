use crate::ui::slash::{SlashCtx, c_error};

use super::{DEFAULT_TIMEOUT, print_stop, require_session};

pub(super) async fn cmd_step_over(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let mgr = require_session().await?;
    let signal = crate::agent::agent_loop::tool::AbortSignal::new();
    match mgr.step_over(0, &signal, DEFAULT_TIMEOUT).await {
        Ok(summary) => {
            print_stop(ctx, &summary).await?;
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("step failed: {e}"), c_error())?;
        }
    }
    Ok(())
}

pub(super) async fn cmd_step_in(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let mgr = require_session().await?;
    let signal = crate::agent::agent_loop::tool::AbortSignal::new();
    match mgr.step_in(0, &signal, DEFAULT_TIMEOUT).await {
        Ok(summary) => {
            print_stop(ctx, &summary).await?;
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("step_in failed: {e}"), c_error())?;
        }
    }
    Ok(())
}

pub(super) async fn cmd_step_out(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let mgr = require_session().await?;
    let signal = crate::agent::agent_loop::tool::AbortSignal::new();
    match mgr.step_out(0, &signal, DEFAULT_TIMEOUT).await {
        Ok(summary) => {
            print_stop(ctx, &summary).await?;
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("step_out failed: {e}"), c_error())?;
        }
    }
    Ok(())
}
