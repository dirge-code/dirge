//! /clear handler.

use crate::ui::events::render_session;
use crate::ui::slash::SlashCtx;

pub(crate) async fn cmd_clear(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    crate::agent::review::maybe_fire_session_end(ctx.agent, ctx.session);
    ctx.session.messages.clear();
    ctx.session.total_estimated_tokens = 0;
    ctx.session.compactions.clear();
    ctx.session.message_store.clear();
    ctx.session.tree.entries.clear();
    ctx.session.tree.leaf_id = None;
    crate::agent::tools::modified::clear_modified();
    crate::agent::tools::snapshots::clear();
    crate::agent::tools::todo::clear();
    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
    Ok(())
}
