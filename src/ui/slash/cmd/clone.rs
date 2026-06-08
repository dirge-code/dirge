//! /clone handler.

use crate::ui::events::render_session;
use crate::ui::slash::{SlashCtx, c_agent, c_error};
use crate::ui::tree::{self, short_id};

pub(crate) async fn cmd_clone(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    ctx.session.ensure_tree_initialized();
    ctx.session.ensure_message_store_initialized();

    let arg = parts.get(1).copied().unwrap_or("").trim();
    if arg.is_empty() {
        ctx.renderer
            .write_line("usage: /clone <id-prefix>", c_error())?;
    } else {
        match tree::resolve_id_prefix(ctx.session, arg) {
            Ok(id) => match ctx.session.clone_at(&id) {
                Ok(()) => {
                    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
                    ctx.renderer
                        .write_line(&format!("cloned path through {}", short_id(&id)), c_agent())?;
                }
                Err(e) => ctx
                    .renderer
                    .write_line(&format!("/clone: {}", e), c_error())?,
            },
            Err(e) => ctx
                .renderer
                .write_line(&format!("/clone: {}", e), c_error())?,
        }
    }
    Ok(())
}
