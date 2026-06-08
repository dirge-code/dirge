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
