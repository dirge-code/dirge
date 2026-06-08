use crate::ui::slash::{SlashCtx, c_error, c_result};

use super::{DEFAULT_TIMEOUT, require_session};

pub(super) async fn cmd_evaluate(ctx: &mut SlashCtx<'_>, args: &[&str]) -> anyhow::Result<()> {
    if args.is_empty() {
        ctx.renderer
            .write_line("usage: /debug evaluate <expression>", c_error())?;
        return Ok(());
    }
    let expression = args.join(" ");
    let mgr = require_session().await?;
    match mgr.evaluate(&expression, None, None, DEFAULT_TIMEOUT).await {
        Ok(result) => {
            ctx.renderer.write_line(
                &format!(
                    "{expression} = {}",
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| format!("{result:?}"))
                ),
                c_result(),
            )?;
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("evaluate failed: {e}"), c_error())?;
        }
    }
    Ok(())
}
