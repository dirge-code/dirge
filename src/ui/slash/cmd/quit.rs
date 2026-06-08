//! /quit handler.

use crate::ui::slash::SlashCtx;

pub(crate) async fn cmd_quit(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    *ctx.is_running = false;
    Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "quit").into())
}
