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

    // Restore the pre-profile effort override captured when a profile's
    // `reasoning` frontmatter was applied (GH #828) — the effort sibling of
    // the route restore above, and deliberately independent of it: a refused
    // model restore must not leave the profile's reasoning behind. Restoring
    // the pre-profile value also discards any `/effort` issued WHILE the
    // profile was active, exactly as the route restore discards a mid-profile
    // `/model`. Done before `rebuild_agent` so the rebuild installs the
    // restored override (or, for `Some(None)`, re-seeds the provider config
    // default) on the live agent.
    restore_profile_reasoning(
        &mut ctx.session.effort_override,
        &mut ctx.context.effort_before_agent,
    );

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

/// Put `session.effort_override` back to the value captured when an agent
/// profile first applied its `reasoning` frontmatter (GH #828). A no-op
/// when nothing was captured (no active profile ever set `reasoning`), so a
/// profile without the key leaves the session's effort untouched in both
/// directions.
pub(crate) fn restore_profile_reasoning(
    effort_override: &mut Option<crate::agent::agent_loop::types::ThinkingLevel>,
    effort_before_agent: &mut Option<Option<crate::agent::agent_loop::types::ThinkingLevel>>,
) {
    if let Some(prior) = effort_before_agent.take() {
        *effort_override = prior;
    }
}
