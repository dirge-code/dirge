//! /tasks handler.

use crate::ui::slash::{SlashCtx, c_result};
use crate::ui::theme;

pub(crate) async fn cmd_tasks(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let names = ctx.renderer.chat_names();
    if names.len() <= 1 {
        ctx.renderer.write_line(
            "no subagent chats yet — spawn one via the `task` tool.",
            c_result(),
        )?;
    } else {
        ctx.renderer.write_line("chat windows:", c_result())?;
        let active = ctx.renderer.active_chat();
        for (i, name) in names.iter().enumerate() {
            let marker = if i == active { "→" } else { " " };
            ctx.renderer
                .write_line(&format!("  {} [{}] {}", marker, i, name), c_result())?;
        }
        ctx.renderer.write_line(
            "  (Ctrl-N / Ctrl-P to cycle, Ctrl+X to close)",
            theme::dim(),
        )?;
    }

    let shells = crate::agent::tools::bg_shell::global().list();
    if !shells.is_empty() {
        ctx.renderer.write_line("background shells:", c_result())?;
        for s in &shells {
            ctx.renderer.write_line(
                &format!("  [{}] {} - {}", s.status.label(), s.id, s.command),
                c_result(),
            )?;
        }
    }
    Ok(())
}
