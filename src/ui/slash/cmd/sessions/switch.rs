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
            // Resolve to the chain tip so resuming a folded conversation
            // by prefix lands on the live state, not the stale pre-fold
            // file the rotation left behind.
            let s = crate::session::storage::load_session_tip(&s.id).unwrap_or(s);
            let msg_count = s.messages.len();
            let restored = s.current_prompt_name.clone();
            super::swap_to_session(ctx, s).await?;

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
        // Distinguishing-length ids so the user can retype a unique prefix
        // (compacted sessions share the `compacted-` head — dirge).
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        let idlen = super::distinct_id_len(&ids);
        ctx.renderer
            .write_line(&format!("multiple sessions match '{}':", prefix), c_agent())?;
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
