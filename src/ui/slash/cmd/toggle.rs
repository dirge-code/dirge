//! /toggle handler.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

pub(crate) async fn cmd_toggle(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 {
        ctx.renderer
            .write_line("usage: /toggle <feature> [on|off]", c_agent())?;
        ctx.renderer.write_line("features:", c_agent())?;
        ctx.renderer.write_line(
            &format!(
                "  todo  {}",
                if *ctx.todo_tools_enabled { "on" } else { "off" }
            ),
            c_result(),
        )?;
    } else {
        let new_state = match parts.get(2).copied() {
            Some("on") => true,
            Some("off") => false,
            Some(other) => {
                ctx.renderer
                    .write_line(&format!("invalid: '{}', use on or off", other), c_error())?;
                return Ok(());
            }
            None => !*ctx.todo_tools_enabled,
        };
        if new_state == *ctx.todo_tools_enabled {
            ctx.renderer.write_line(
                &format!(
                    "todo tools already {}",
                    if new_state { "on" } else { "off" }
                ),
                c_agent(),
            )?;
        } else {
            *ctx.todo_tools_enabled = new_state;
            let model = ctx.client.completion_model(ctx.session.model.to_string());
            *ctx.agent = crate::provider::build_agent(
                model,
                ctx.cli,
                ctx.cfg,
                ctx.context,
                ctx.permission.clone(),
                ctx.ask_tx.clone(),
                ctx.question_tx.clone(),
                ctx.plan_tx.clone(),
                ctx.bg_store.clone(),
                #[cfg(feature = "lsp")]
                ctx.lsp_manager.cloned(),
                ctx.sandbox.clone(),
                #[cfg(feature = "mcp")]
                ctx.mcp_manager,
                #[cfg(feature = "semantic")]
                ctx.semantic_manager,
                Some(ctx.session.id.to_string()),
            )
            .await;
            ctx.renderer.write_line(
                &format!(
                    "todo tools: {}",
                    if *ctx.todo_tools_enabled { "on" } else { "off" }
                ),
                c_agent(),
            )?;
        }
    }
    Ok(())
}
