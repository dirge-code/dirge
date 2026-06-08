//! /sandbox snapshot handler.

use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

pub(crate) async fn cmd_sandbox_snapshot(
    ctx: &mut SlashCtx<'_>,
    parts: &[&str],
) -> anyhow::Result<()> {
    let action = parts.get(2).copied().unwrap_or("help");
    match action {
        "save" => {
            let name = parts.get(3).copied().unwrap_or("");
            if name.is_empty() {
                ctx.renderer
                    .write_line("usage: /sandbox snapshot save <name>", c_error())?;
                return Ok(());
            }
            match ctx.sandbox.save_snapshot(name) {
                Ok(()) => {
                    ctx.renderer
                        .write_line(&format!("snapshot '{name}' saved."), c_result())?;
                }
                Err(e) => {
                    ctx.renderer
                        .write_line(&format!("snapshot save failed: {e}"), c_error())?;
                }
            }
        }
        "list" => match ctx.sandbox.list_snapshots() {
            Ok(names) if names.is_empty() => {
                ctx.renderer
                    .write_line("no snapshots saved yet.", c_agent())?;
            }
            Ok(names) => {
                ctx.renderer.write_line("snapshots:", c_agent())?;
                for name in &names {
                    ctx.renderer.write_line(&format!("  {name}"), c_result())?;
                }
            }
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("list snapshots failed: {e}"), c_error())?;
            }
        },
        "restore" => {
            let name = parts.get(3).copied().unwrap_or("");
            if name.is_empty() {
                ctx.renderer
                    .write_line("usage: /sandbox snapshot restore <name>", c_error())?;
                return Ok(());
            }
            match ctx.sandbox.restore_snapshot(name) {
                Ok(()) => {
                    ctx.renderer.write_line(
                        &format!("snapshot '{name}' restored — restart the VM to use it."),
                        c_result(),
                    )?;
                }
                Err(e) => {
                    ctx.renderer
                        .write_line(&format!("snapshot restore failed: {e}"), c_error())?;
                }
            }
        }
        "delete" => {
            let name = parts.get(3).copied().unwrap_or("");
            if name.is_empty() {
                ctx.renderer
                    .write_line("usage: /sandbox snapshot delete <name>", c_error())?;
                return Ok(());
            }
            match ctx.sandbox.delete_snapshot(name) {
                Ok(()) => {
                    ctx.renderer
                        .write_line(&format!("snapshot '{name}' deleted."), c_result())?;
                }
                Err(e) => {
                    ctx.renderer
                        .write_line(&format!("snapshot delete failed: {e}"), c_error())?;
                }
            }
        }
        "help" | "--help" | "-h" => {
            ctx.renderer.write_line("snapshot commands:", c_agent())?;
            ctx.renderer.write_line(
                "  /sandbox snapshot save <name>      —   save current VM state",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot list             —   list saved snapshots",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot restore <name>    —   restore (stop VM first)",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot delete <name>     —   delete a snapshot",
                c_result(),
            )?;
        }
        _ => {
            ctx.renderer.write_line(
                &format!("unknown snapshot command: {action} (try /sandbox snapshot help)"),
                c_error(),
            )?;
        }
    }
    Ok(())
}
