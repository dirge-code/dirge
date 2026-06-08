//! /regen-prompts handler.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::ui::slash::cmd::agent;
use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_regen_prompts(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match crate::context::prompts::regen() {
        Ok(()) => {
            ctx.context.prompts = crate::context::prompts::load();
            if let Some(name) = ctx
                .context
                .prompt_layer
                .as_ref()
                .and_then(|p| p.name.clone())
                && let Some(p) = ctx.context.prompts.get(&name).cloned()
            {
                ctx.context.set_prompt_layer(
                    Some(name),
                    Some(p.body.clone()),
                    p.deny_tools.clone(),
                );
                crate::permission::apply_prompt_deny(
                    ctx.permission,
                    &ctx.context.current_prompt_deny_tools,
                );
            }
            agent::rebuild_agent(ctx).await;
            ctx.renderer.write_line(
                "default prompts regenerated; agent rebuilt with refreshed prompt",
                c_agent(),
            )?;
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("failed to regenerate prompts: {}", e), c_error())?;
        }
    }
    Ok(())
}
