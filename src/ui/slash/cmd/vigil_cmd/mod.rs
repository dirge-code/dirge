//! /vigil command dispatch.

#[cfg(feature = "vigil")]
pub(crate) mod add;
#[cfg(feature = "vigil")]
pub(crate) mod pause;
#[cfg(feature = "vigil")]
pub(crate) mod remove;
#[cfg(feature = "vigil")]
pub(crate) mod rest;
#[cfg(feature = "vigil")]
pub(crate) mod resume;
#[cfg(feature = "vigil")]
pub(crate) mod start;
#[cfg(feature = "vigil")]
pub(crate) mod status;
#[cfg(feature = "vigil")]
pub(crate) mod stop;

use crate::ui::slash::SlashCtx;
#[cfg(feature = "vigil")]
use crate::ui::slash::c_error;

#[cfg(not(feature = "vigil"))]
use crate::ui::slash::c_agent;

pub(crate) async fn cmd_vigil(
    ctx: &mut SlashCtx<'_>,
    #[allow(unused_variables)] parts: &[&str],
    #[allow(unused_variables)] text: &str,
) -> anyhow::Result<()> {
    #[cfg(feature = "vigil")]
    {
        let sub = parts.get(1).copied().unwrap_or("status");
        match sub {
            "add" => add::cmd_vigil_add(ctx, parts, text).await,
            "start" => start::cmd_vigil_start(ctx, parts).await,
            "stop" => stop::cmd_vigil_stop(ctx, parts).await,
            "status" => status::cmd_vigil_status(ctx).await,
            "rest" => rest::cmd_vigil_rest(ctx, parts).await,
            "pause" => pause::cmd_vigil_pause(ctx, parts).await,
            "resume" => resume::cmd_vigil_resume(ctx, parts).await,
            "remove" => remove::cmd_vigil_remove(ctx, parts).await,
            _ => {
                ctx.renderer.write_line(
                    "usage: /vigil [add|start|stop|status|rest|pause|resume|remove]",
                    c_error(),
                )?;
                Ok(())
            }
        }
    }
    #[cfg(not(feature = "vigil"))]
    {
        ctx.renderer.write_line(
            "/vigil requires the 'vigil' feature: cargo build --features vigil",
            c_agent(),
        )?;
        Ok(())
    }
}
