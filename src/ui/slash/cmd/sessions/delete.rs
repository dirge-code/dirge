//! /sessions delete <prefix> — delete session by ID prefix.

use crate::ui::events::{format_time, session_preview};
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
            let preview = s
                .messages
                .last()
                .map(|m| format!("...{}", &m.content.chars().take(40).collect::<String>()))
                .unwrap_or_default();
            if let Err(e) = crate::session::storage::delete_session(&id) {
                ctx.renderer
                    .write_line(&format!("failed to delete: {}", e), c_error())?;
            } else {
                ctx.renderer.write_line(
                    &format!("deleted session {} {}", crate::text::head(&id, 8), preview),
                    c_agent(),
                )?;
            }
        }
    } else {
        ctx.renderer.write_line(
            &format!("multiple sessions match '{}', be more specific", prefix),
            c_agent(),
        )?;
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
