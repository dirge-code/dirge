use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

use super::get_manager;

pub(super) async fn cmd_sessions(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let mgr = match get_manager() {
        Some(m) => m,
        None => {
            ctx.renderer
                .write_line("no debug session manager", c_error())?;
            return Ok(());
        }
    };
    match mgr.active_summary().await {
        Some(s) => {
            ctx.renderer.write_line(
                &format!(
                    "active session: id={} adapter={} status={:?}",
                    s.id, s.adapter_name, s.status,
                ),
                c_result(),
            )?;
            if let Some(reason) = &s.stop_reason {
                ctx.renderer
                    .write_line(&format!("  stop reason: {reason}"), c_result())?;
            }
            if let Some(tid) = s.thread_id {
                ctx.renderer
                    .write_line(&format!("  thread: {tid}"), c_result())?;
            }
        }
        None => {
            ctx.renderer
                .write_line("no active debug session", c_agent())?;
        }
    }
    Ok(())
}
