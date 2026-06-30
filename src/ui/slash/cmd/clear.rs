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
    // Todos are durable issues now, so /clear doesn't delete them — it just
    // resyncs the panel mirror to this session's live board (clearing the
    // conversation doesn't change what work is still open).
    let db_path = crate::extras::dirge_paths::ProjectPaths::new(std::path::Path::new(
        ctx.session.working_dir.as_str(),
    ))
    .session_db_path();
    crate::agent::tools::todo::refresh_board(&db_path, Some(ctx.session.id.as_str()));
    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
    Ok(())
}
