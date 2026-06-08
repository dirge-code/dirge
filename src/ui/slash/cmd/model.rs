//! /model, /reasoning handlers.

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use compact_str::CompactString;

use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_model(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 {
        ctx.renderer
            .write_line(&format!("current model: {}", ctx.session.model), c_agent())?;
    } else {
        let new_model = CompactString::new(parts[1].trim());
        let model = ctx.client.completion_model(new_model.to_string());
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
        ctx.session.model = new_model.clone();
        ctx.session.provider = ctx.cli.resolve_provider(ctx.cfg);
        let new_ctx = ctx.cfg.resolve_context_window(new_model.as_str());
        let old_ctx = ctx.session.context_window;
        if new_ctx != old_ctx {
            ctx.session.context_window = new_ctx;
        }
        ctx.renderer
            .write_line(&format!("switched to model: {}", new_model), c_agent())?;
        let reserve = ctx.cfg.resolve_reserve_tokens();
        let budget = new_ctx.saturating_sub(reserve);
        if new_ctx < old_ctx && ctx.session.total_estimated_tokens > budget {
            ctx.renderer.write_line(
                &format!(
                    "warning: session uses ~{}k tokens but new model's context budget is ~{}k. Run /compress before the next prompt or the next turn may overflow.",
                    ctx.session.total_estimated_tokens / 1_000,
                    budget / 1_000,
                ),
                c_error(),
            )?;
        }
    }
    Ok(())
}

pub(crate) async fn cmd_reasoning(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    *ctx.show_reasoning = !*ctx.show_reasoning;
    ctx.renderer.write_line(
        &format!(
            "reasoning visibility: {}",
            if *ctx.show_reasoning { "on" } else { "off" }
        ),
        c_agent(),
    )?;
    Ok(())
}
