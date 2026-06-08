//! /fork handler.

use crate::session::MessageRole;
use crate::ui::events::render_session;
use crate::ui::slash::{SlashCtx, c_agent, c_error};
use crate::ui::tree::{self, short_id};

pub(crate) async fn cmd_fork(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    ctx.session.ensure_tree_initialized();
    ctx.session.ensure_message_store_initialized();

    let arg = parts.get(1).copied().unwrap_or("").trim();
    let target_id = if arg.is_empty() {
        let last_user = ctx
            .session
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.id.clone());
        match last_user {
            Some(id) => Ok(id),
            None => Err("no user message on current branch".to_string()),
        }
    } else {
        tree::resolve_id_prefix(ctx.session, arg)
    };
    match target_id {
        Ok(id) => match ctx.session.fork_at(&id) {
            Ok(original) => {
                ctx.input.set_text(&original.content);
                render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
                ctx.renderer.write_line(
                    &format!(
                        "forked at {} — original prompt restored to editor",
                        short_id(&id)
                    ),
                    c_agent(),
                )?;
            }
            Err(e) => ctx
                .renderer
                .write_line(&format!("/fork: {}", e), c_error())?,
        },
        Err(e) => ctx
            .renderer
            .write_line(&format!("/fork: {}", e), c_error())?,
    }
    Ok(())
}
