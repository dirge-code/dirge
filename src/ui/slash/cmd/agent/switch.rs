//! /agent <name> — activate a named agent profile.

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use compact_str::CompactString;

use crate::context::agent_defs::resolve_model_alias as resolve_agent_model;
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
        ctx.context.model_before_agent = Some(ctx.session.model.to_string());
    }
    ctx.context.set_agent_layer(def.clone());
    crate::permission::apply_prompt_deny(ctx.permission, &ctx.context.current_prompt_deny_tools);

    let resolved_model = resolve_agent_model(ctx.cfg, def.model.as_deref());
    if let Some(model) = &resolved_model {
        ctx.session.model = CompactString::new(model.as_str());
        ctx.session.provider = ctx.cli.resolve_provider(ctx.cfg);
        ctx.session.context_window = ctx.cfg.resolve_context_window(ctx.session.model.as_str());
    }

    rebuild_agent(ctx).await;

    let mut summary = format!("active agent: {}", def.name);
    if let Some(m) = &resolved_model {
        summary.push_str(&format!("  · model {m}"));
    }
    ctx.renderer.write_line(&summary, c_agent())?;
    Ok(())
}
