//! /agent command dispatch and shared helpers.

pub(crate) mod clear;
pub(crate) mod list;
pub(crate) mod switch;

use crate::ui::slash::SlashCtx;

/// Rebuild the agent from the current session model and context.
/// Shared by agent activation/deactivation and /regen-prompts.
pub(crate) async fn rebuild_agent(ctx: &mut SlashCtx<'_>) {
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
}

pub(crate) async fn cmd_agent(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 || parts[0] == "/agents" {
        return list::cmd_agent_list(ctx, parts).await;
    }
    let arg = parts[1].trim();
    if matches!(arg, "off" | "none" | "default") {
        return clear::cmd_agent_clear(ctx).await;
    }
    switch::cmd_agent_switch(ctx, arg).await
}
