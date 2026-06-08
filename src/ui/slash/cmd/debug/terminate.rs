use crate::ui::slash::{SlashCtx, c_error, c_result};

use super::{DEFAULT_TIMEOUT, require_session};

pub(super) async fn cmd_terminate(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let mgr = require_session().await?;
    match mgr.terminate(DEFAULT_TIMEOUT).await {
        Ok(summary) => {
            ctx.renderer.write_line(
                &format!(
                    "debug session terminated. exit code: {}",
                    summary.exit_code.map_or("none".into(), |c| c.to_string()),
                ),
                c_result(),
            )?;
            let _ = mgr.disconnect(false, DEFAULT_TIMEOUT).await;
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("terminate failed: {e}"), c_error())?;
        }
    }
    Ok(())
}
