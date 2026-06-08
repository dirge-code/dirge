//! /retry handler.

use crate::session::MessageRole;
use crate::ui::events::render_session;
use crate::ui::slash::{SlashCtx, c_agent};

pub(crate) async fn cmd_retry(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let last_user = ctx
        .session
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .cloned();
    match last_user {
        Some(msg) => {
            let mut guard = ctx.session.messages.len();
            while let Some(last) = ctx.session.messages.last() {
                let was_user = last.role == MessageRole::User;
                ctx.session.pop_last_message();
                if was_user {
                    break;
                }
                guard = guard.saturating_sub(1);
                if guard == 0 {
                    break;
                }
            }
            ctx.input.buffer = msg.content.clone();
            ctx.input.cursor = msg.content.len();
            render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
            ctx.renderer
                .write_line("edit last message and press Enter to retry", c_agent())?;
        }
        None => {
            ctx.renderer
                .write_line("no previous message to retry", c_agent())?;
        }
    }
    Ok(())
}
