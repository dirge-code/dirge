//! /sessions <prefix> — load session by ID prefix.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::ui::events::{format_time, render_session, session_preview};
use crate::ui::slash::{SlashCtx, c_agent, c_result};

pub(crate) async fn cmd_sessions_switch(
    ctx: &mut SlashCtx<'_>,
    prefix: &str,
) -> anyhow::Result<()> {
    let sessions = crate::session::storage::find_sessions_by_prefix(prefix)?;
    if sessions.is_empty() {
        ctx.renderer
            .write_line(&format!("no session matching '{}'", prefix), c_agent())?;
    } else if sessions.len() == 1 {
        if let Some(s) = sessions.into_iter().next() {
            let msg_count = s.messages.len();
            if let Some(store) = ctx.bg_store.as_ref() {
                store.cancel_all();
            }
            crate::agent::review::maybe_fire_session_end(ctx.agent, ctx.session);
            let old_id = ctx.session.id.to_string();
            *ctx.session = s;
            crate::agent::review::maybe_fire_session_switch(
                ctx.agent,
                &ctx.session.id,
                &old_id,
                false,
            );
            let restored = ctx.session.current_prompt_name.clone();
            if let Some(name) = restored.as_deref() {
                if let Some(p) = ctx.context.prompts.get(name).cloned() {
                    ctx.context.set_prompt_layer(
                        Some(name.to_string()),
                        Some(p.body.clone()),
                        p.deny_tools.clone(),
                    );
                    crate::permission::apply_prompt_deny(
                        ctx.permission,
                        &ctx.context.current_prompt_deny_tools,
                    );
                }
            }

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

            render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
            let prompt_note = restored
                .map(|n| format!("; prompt: {}", n))
                .unwrap_or_default();
            ctx.renderer.write_line(
                &format!("loaded session ({} msgs{})", msg_count, prompt_note),
                c_agent(),
            )?;
        }
    } else {
        ctx.renderer
            .write_line(&format!("multiple sessions match '{}':", prefix), c_agent())?;
        for s in &sessions {
            let preview = session_preview(s, 60);
            let time = format_time(&s.updated_at);
            ctx.renderer.write_line(
                &format!(
                    "  {}  {}  {}msgs  {}  {}",
                    crate::text::head(&s.id, 8),
                    time,
                    s.messages.len(),
                    s.model,
                    preview
                ),
                c_result(),
            )?;
        }
    }
    Ok(())
}
