//! /agent <name> — activate a named agent profile.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::context::agent_defs::resolve_model_alias as resolve_agent_model;
use crate::provider::{ModelRoute, apply_model_route, resolve_model_route};
use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

use super::rebuild_agent;

pub(crate) async fn cmd_agent_switch(ctx: &mut SlashCtx<'_>, arg: &str) -> anyhow::Result<()> {
    let Some(def) = ctx.context.agent_defs.get(arg).cloned() else {
        ctx.renderer
            .write_line(&format!("unknown agent: '{}'", arg), c_error())?;
        if !ctx.context.agent_defs.is_empty() {
            ctx.renderer.write_line("available agents:", c_agent())?;
            for a in ctx.context.agent_defs.iter() {
                ctx.renderer
                    .write_line(&format!("  {}", a.name), c_result())?;
            }
        }
        return Ok(());
    };

    if ctx.context.agent_layer.is_none() {
        // Capture the PAIR, not just the model: `/agent off` has to put the
        // session back on the client it left, and the id alone can't say which
        // provider that was (dirge-fhr5).
        ctx.context.route_before_agent = Some(ModelRoute::pinned(
            ctx.session.provider.to_string(),
            ctx.session.model.to_string(),
        ));
    }
    ctx.context.set_agent_layer(def.clone());
    crate::permission::apply_prompt_deny(ctx.permission, &ctx.context.current_prompt_deny_tools);

    // A profile pinning a model from another family has to move the CLIENT too
    // — renaming the model on the live client sent e.g. `glm-5.2` to a
    // ChatGPT/Codex endpoint and 400'd every turn (dirge-fhr5, the `/agent`
    // sibling of #711). Same routing decision `/model` makes.
    let resolved_model = resolve_agent_model(ctx.cfg, def.model.as_deref());
    let mut switched_to = None;
    if let Some(model) = &resolved_model {
        let route = resolve_model_route(ctx.cfg, ctx.session.provider.as_str(), model);
        match apply_model_route(ctx.cfg, ctx.client, ctx.session, route) {
            Ok(alias) => switched_to = alias,
            // Keep the profile active (its prompt and tool policy are still
            // valid) but leave the session on a model that works, mirroring
            // `/model`'s refusal.
            Err(refusal) => ctx.renderer.write_line(
                &format!(
                    "agent '{}': {refusal} Keeping model '{}' on '{}'.",
                    def.name, ctx.session.model, ctx.session.provider,
                ),
                c_error(),
            )?,
        }
    }

    rebuild_agent(ctx).await;

    let mut summary = format!("active agent: {}", def.name);
    if resolved_model.is_some() {
        summary.push_str(&format!("  · model {}", ctx.session.model));
    }
    if let Some(alias) = &switched_to {
        summary.push_str(&format!("  ·  {alias}"));
    }
    ctx.renderer.write_line(&summary, c_agent())?;
    Ok(())
}
