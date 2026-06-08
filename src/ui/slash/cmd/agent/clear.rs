//! /agent off — deactivate the active agent profile.

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use compact_str::CompactString;

use crate::ui::slash::{SlashCtx, c_agent};

use super::rebuild_agent;

pub(crate) async fn cmd_agent_clear(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if ctx.context.agent_layer.is_none() {
        ctx.renderer
            .write_line("no active agent to clear", c_agent())?;
        return Ok(());
    }
    ctx.context.clear_agent_layer();
    crate::permission::apply_prompt_deny(ctx.permission, &ctx.context.current_prompt_deny_tools);

    let restored_model = ctx.context.model_before_agent.take();
    if let Some(model) = &restored_model {
        ctx.session.model = CompactString::new(model.as_str());
        ctx.session.provider = ctx.cli.resolve_provider(ctx.cfg);
        ctx.session.context_window = ctx.cfg.resolve_context_window(ctx.session.model.as_str());
    }

    rebuild_agent(ctx).await;

    let msg = match &restored_model {
        Some(m) => format!("agent deactivated · model restored to {m}"),
        None => "agent deactivated".to_string(),
    };
    ctx.renderer.write_line(&msg, c_agent())?;
    Ok(())
}
