//! /help handler.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::ui::slash::{SlashCtx, c_agent, c_result};
use crate::ui::theme;

pub(crate) async fn cmd_help(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let renderer = &mut *ctx.renderer;
    renderer.write_line("keyboard shortcuts:", c_agent())?;
    renderer.write_line(
        "  Enter                   submit message / interrupt running agent",
        c_result(),
    )?;
    renderer.write_line("  Ctrl+C                  interrupt", c_result())?;
    renderer.write_line("  Alt+Enter               insert newline", c_result())?;
    renderer.write_line("  Up / Down               history", c_result())?;
    renderer.write_line(
        "  Tab                     complete slash command",
        c_result(),
    )?;
    renderer.write_line("  Right                   accept ghost suffix", c_result())?;
    renderer.write_line("  Ctrl+L                  clear screen", c_result())?;
    renderer.write_line("  Ctrl+Up / Ctrl+Down     scroll up/down", c_result())?;
    renderer.write_line("  PgUp / PgDn             page up/down", c_result())?;
    renderer.write_line(
        "  Ctrl+N / Ctrl+P         next / previous chat (subagent windows)",
        c_result(),
    )?;
    renderer.write_line("  Ctrl+X                  close chat window", c_result())?;
    renderer.write_line(
        "  Ctrl+K                  kill subagent on focused tab",
        c_result(),
    )?;
    renderer.write_line(
        "  Ctrl+O                  expand collapsed tool result",
        c_result(),
    )?;
    renderer.write_line(
        "  Esc-Esc (idle)          open rewind picker (truncate history)",
        c_result(),
    )?;
    renderer.write_line(
        "  ! / !! cmd              run shell command interactively (visible=feed agent / invisible=live only)",
        c_result(),
    )?;
    renderer.write_line(
        "  (type while agent runs to queue a follow-up message)",
        c_result(),
    )?;

    renderer.write_line("", c_agent())?;
    renderer.write_line("command-line options:", c_agent())?;
    renderer.write_line("  dirge --help", c_result())?;
    renderer.write_line("  man dirge.1    OR    dirge help", c_result())?;
    renderer.write_line("  (docs/ for guides on agents, permissions, skills, prompts, DAP, plugins, themes, and microVMs)",
        theme::dim(),
    )?;

    renderer.write_line("", c_agent())?;
    let cmds = crate::ui::slash::slash_command_names();
    renderer.write_line(&format!("slash commands ({}):", cmds.len()), c_agent())?;
    for cmd in &cmds {
        renderer.write_line(&format!("  {}", cmd), c_result())?;
    }

    let aliases = crate::ui::slash::aliases::display_entries(ctx.cfg);
    if !aliases.is_empty() {
        renderer.write_line("", c_agent())?;
        renderer.write_line("slash aliases (from your config):", c_agent())?;
        for line in &aliases {
            renderer.write_line(&format!("  {line}"), c_result())?;
        }
    }

    #[cfg(feature = "plugin")]
    if let Some(pm_arc) = crate::plugin::hook::global() {
        let cmds = {
            let mut mgr = pm_arc.lock_ignore_poison();
            mgr.list_commands()
        };
        if !cmds.is_empty() {
            renderer.write_line("", c_agent())?;
            renderer.write_line("plugin commands:", c_agent())?;
            for (cmd, handler) in cmds {
                renderer.write_line(&format!("  /{:<20} -> {}", cmd, handler), c_result())?;
            }
        }
    }

    Ok(())
}
