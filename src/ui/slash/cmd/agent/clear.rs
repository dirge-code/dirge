//! /agent off — deactivate the active agent profile.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::provider::apply_model_route;
use crate::ui::slash::{SlashCtx, c_agent, c_error};

use super::rebuild_agent;

pub(crate) async fn cmd_agent_clear(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if ctx.context.agent_layer.is_none() {
        ctx.renderer
            .write_line("no active agent to clear", c_agent())?;
        return Ok(());
    }
    ctx.context.clear_agent_layer();
    crate::permission::apply_prompt_deny(ctx.permission, &ctx.context.current_prompt_deny_tools);

    // Restore the captured (provider, model) PAIR, not just the id: the profile
    // may have moved the live client to another provider, and re-inferring the
    // route from the id alone can land on a different alias of the same family
    // — or refuse outright when the pre-agent provider was a built-in with no
    // `providers` entry to infer from (dirge-fhr5).
    let restored = ctx.context.route_before_agent.take();
    let mut restore_error = None;
    if let Some(route) = restored.clone()
        && let Err(refusal) = apply_model_route(ctx.cfg, ctx.client, ctx.session, route)
    {
        restore_error = Some(format!(
            "{refusal} Staying on model '{}' at '{}'.",
            ctx.session.model, ctx.session.provider,
        ));
    }

    rebuild_agent(ctx).await;

    if let Some(err) = restore_error {
        ctx.renderer
            .write_line(&format!("agent deactivated · {err}"), c_error())?;
        return Ok(());
    }
    let msg = match &restored {
        Some(route) => format!("agent deactivated · model restored to {}", route.model()),
        None => "agent deactivated".to_string(),
    };
    ctx.renderer.write_line(&msg, c_agent())?;
    Ok(())
}
