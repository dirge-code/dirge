//! /sessions delete <prefix> — delete session by ID prefix.

use crate::session::Session;
use crate::ui::events::{format_time, render_session, session_preview};
use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

pub(crate) async fn cmd_sessions_delete(
    ctx: &mut SlashCtx<'_>,
    prefix: &str,
) -> anyhow::Result<()> {
    let sessions = crate::session::storage::find_sessions_by_prefix(prefix)?;
    if sessions.is_empty() {
        ctx.renderer
            .write_line(&format!("no session matching '{}'", prefix), c_agent())?;
    } else if sessions.len() == 1 {
        if let Some(s) = sessions.into_iter().next() {
            let id = s.id.clone();
            let is_current = id == ctx.session.id;
            let preview = s
                .messages
                .last()
                .map(|m| format!("...{}", m.content.chars().take(40).collect::<String>()))
                .unwrap_or_default();
            if let Err(e) = crate::session::storage::delete_session(&id) {
                ctx.renderer
                    .write_line(&format!("failed to delete: {}", e), c_error())?;
            } else {
                ctx.renderer.write_line(
                    &format!("deleted session {} {}", crate::text::head(&id, 8), preview),
                    c_agent(),
                )?;
                // Deleting the session we're in would otherwise leave the
                // live session pointing at a removed file (a zombie). Boot
                // into a fresh empty session — same model/provider/cwd — so
                // there's always a real session backing the UI (dirge).
                if is_current {
                    let mut fresh = Session::new(
                        &ctx.session.provider,
                        &ctx.session.model,
                        ctx.session.context_window,
                    );
                    fresh.working_dir = ctx.session.working_dir.clone();
                    super::swap_to_session(ctx, fresh).await?;
                    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
                    ctx.renderer.write_line(
                        &format!(
                            "started a fresh session ({})",
                            crate::text::head(&ctx.session.id, 8)
                        ),
                        c_agent(),
                    )?;
                }
            }
        }
    } else {
        // Show each id at the shortest length that makes it unique among the
        // matches, so the user can retype a distinguishing prefix. (Compacted
        // sessions all share the `compacted-` head, so a fixed 8 chars was
        // identical for every one — dirge.)
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        let idlen = super::distinct_id_len(&ids);
        ctx.renderer.write_line(
            &format!(
                "multiple sessions match '{}', be more specific — try one of these ids:",
                prefix
            ),
            c_agent(),
        )?;
        for s in &sessions {
            let preview = session_preview(s, 60);
            let time = format_time(&s.updated_at);
            ctx.renderer.write_line(
                &format!(
                    "  {}  {}  {}msgs  {}  {}",
                    crate::text::head(&s.id, idlen),
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
