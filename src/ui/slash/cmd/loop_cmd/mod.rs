//! /loop command dispatch.

pub(crate) mod start;
pub(crate) mod status;
pub(crate) mod stop;

use crate::ui::slash::{SlashCtx, c_error};

pub(crate) async fn cmd_loop(
    ctx: &mut SlashCtx<'_>,
    parts: &[&str],
    text: &str,
) -> anyhow::Result<()> {
    let sub = parts.get(1).copied().unwrap_or("status");
    match sub {
        "start" => start::cmd_loop_start(ctx, parts, text).await,
        "stop" => stop::cmd_loop_stop(ctx).await,
        "status" => status::cmd_loop_status(ctx).await,
        _ => {
            ctx.renderer
                .write_line("usage: /loop [start <prompt>|stop|status]", c_error())?;
            Ok(())
        }
    }
}
