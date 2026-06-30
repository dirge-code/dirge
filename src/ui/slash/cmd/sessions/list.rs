//! /sessions — list recent sessions.

use crate::ui::events::{format_time, session_preview};
use crate::ui::slash::{SlashCtx, c_agent, c_result};

pub(crate) async fn cmd_sessions_list(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let sessions = crate::session::storage::find_recent_sessions(20)?;
    if sessions.is_empty() {
        ctx.renderer.write_line("no saved sessions", c_agent())?;
    } else {
        ctx.renderer
            .write_line(&format!("recent sessions ({}):", sessions.len()), c_agent())?;
        // Show ids at the shortest length that keeps them distinct, so
        // `compacted-<uuid>` sessions don't all render as "compacte" (dirge).
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        let idlen = super::distinct_id_len(&ids);
        let current = ctx.session.id.as_str();
        for s in &sessions {
            let preview = session_preview(s, 60);
            let time = format_time(&s.updated_at);
            // Mark the live session so it's identifiable in the list.
            let marker = if s.id.as_str() == current { "▸" } else { " " };
            ctx.renderer.write_line(
                &format!(
                    "{} {}  {}  {}msgs  {}  {}",
                    marker,
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
