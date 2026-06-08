//! /prompt <name> — switch to a named prompt.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

use super::super::agent::rebuild_agent;

pub(crate) async fn cmd_prompt_switch(ctx: &mut SlashCtx<'_>, name: &str) -> anyhow::Result<()> {
    let Some(p) = ctx.context.prompts.get(name).cloned() else {
        ctx.renderer
            .write_line(&format!("unknown prompt: '{}'", name), c_error())?;
        let mut sorted: Vec<String> = ctx.context.prompts.keys().cloned().collect();
        sorted.sort();
        if !sorted.is_empty() {
            ctx.renderer.write_line("available prompts:", c_agent())?;
            for p in &sorted {
                ctx.renderer.write_line(&format!("  {}", p), c_result())?;
            }
        }
        return Ok(());
    };

    ctx.context.set_prompt_layer(
        Some(name.to_string()),
        Some(p.body.clone()),
        p.deny_tools.clone(),
    );
    crate::permission::apply_prompt_deny(ctx.permission, &ctx.context.current_prompt_deny_tools);
    ctx.session.current_prompt_name = Some(name.to_string());

    rebuild_agent(ctx).await;

    ctx.renderer
        .write_line(&format!("active prompt: {}", name), c_agent())?;
    Ok(())
}
