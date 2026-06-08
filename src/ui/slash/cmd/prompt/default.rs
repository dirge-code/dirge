//! /prompt default — clear the active prompt layer.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::ui::slash::{SlashCtx, c_agent};

use super::super::agent::rebuild_agent;

pub(crate) async fn cmd_prompt_default(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if ctx.context.prompt_layer.is_none() {
        ctx.renderer
            .write_line("no active prompt to clear", c_agent())?;
    } else {
        ctx.context.clear_prompt_layer();
        crate::permission::apply_prompt_deny(
            ctx.permission,
            &ctx.context.current_prompt_deny_tools,
        );
        ctx.session.current_prompt_name = None;

        rebuild_agent(ctx).await;
    }
    Ok(())
}
