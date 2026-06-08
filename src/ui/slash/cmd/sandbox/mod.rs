//! /sandbox command dispatch.

pub(crate) mod attach;
pub(crate) mod reboot;
pub(crate) mod snapshot;

use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

pub(crate) async fn cmd_sandbox(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let sub = parts.get(1).copied().unwrap_or("help");
    match sub {
        "attach" | "ssh" => attach::cmd_sandbox_attach(ctx).await?,
        "snapshot" => snapshot::cmd_sandbox_snapshot(ctx, parts).await?,
        "reboot" | "start" => reboot::cmd_sandbox_reboot(ctx).await?,
        "help" | "--help" | "-h" => {
            ctx.renderer.write_line("sandbox commands:", c_agent())?;
            ctx.renderer.write_line(
                "  /sandbox attach        —   shell into the microVM",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox reboot/start —   boot/restart the microVM",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot save <name>   —   save VM state",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot list         —   list saved snapshots",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot restore <name> —   restore (VM must be stopped)",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot delete <name> —   delete a snapshot",
                c_result(),
            )?;
        }
        _ => {
            ctx.renderer.write_line(
                &format!("unknown sandbox sub-command: {sub} (try /sandbox help)"),
                c_error(),
            )?;
        }
    }
    Ok(())
}
