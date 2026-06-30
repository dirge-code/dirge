//! /btw handler.

use crate::ui::slash::{SlashCtx, c_error};

pub(crate) async fn cmd_btw(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let query = parts.get(1..).map(|p| p.join(" ")).unwrap_or_default();
    if query.is_empty() {
        ctx.renderer
            .write_line("usage: /btw <question>", c_error())?;
        return Ok(());
    }
    // dirge-nret: don't run the completion inline (it froze the loop for the
    // whole call). Defer to the loop, which resolves the model on-thread and
    // spawns the query as a task the `btw_phase` arm renders; the UI stays
    // responsive and Ctrl+C aborts. The loop parses the `DEFER_BTW:` prefix.
    Err(anyhow::anyhow!("DEFER_BTW:{}", query))
}
