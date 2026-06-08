//! /tree handler.

use crate::ui::events::render_session;
use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};
use crate::ui::tree::{self, short_id};

pub(crate) async fn cmd_tree(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    ctx.session.ensure_tree_initialized();
    ctx.session.ensure_message_store_initialized();

    let arg = parts.get(1).copied().unwrap_or("").trim();
    if arg.is_empty() {
        if ctx.session.tree.entries.is_empty() {
            ctx.renderer.write_line("(empty session)", c_agent())?;
        } else {
            for line in tree::render_tree(ctx.session) {
                ctx.renderer.write_line(&line, c_result())?;
            }
        }
    } else {
        match tree::resolve_id_prefix(ctx.session, arg) {
            Ok(id) => {
                if let Err(e) = ctx.session.switch_to_leaf(&id) {
                    ctx.renderer
                        .write_line(&format!("switch failed: {}", e), c_error())?;
                } else {
                    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
                    ctx.renderer
                        .write_line(&format!("switched to leaf {}", short_id(&id)), c_agent())?;
                }
            }
            Err(e) => ctx
                .renderer
                .write_line(&format!("/tree: {}", e), c_error())?,
        }
    }
    Ok(())
}
