use crate::ui::slash::{SlashCtx, c_error, c_result};

use super::{DEFAULT_TIMEOUT, require_session};

pub(super) async fn cmd_continue(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let mgr = require_session().await?;
    let signal = crate::agent::agent_loop::tool::AbortSignal::new();
    match mgr.continue_(0, &signal, DEFAULT_TIMEOUT).await {
        Ok(outcome) => {
            ctx.renderer.write_line(
                &format!(
                    "continue → {:?} (stop reason: {})",
                    outcome.status,
                    outcome.stop_reason.as_deref().unwrap_or("none"),
                ),
                c_result(),
            )?;
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("continue failed: {e}"), c_error())?;
        }
    }
    Ok(())
}
