//! /btw handler.

use crossterm::style::Color;

use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_btw(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let query = parts.get(1..).map(|p| p.join(" ")).unwrap_or_default();
    if query.is_empty() {
        ctx.renderer
            .write_line("usage: /btw <question>", c_error())?;
    } else {
        let model = ctx.client.completion_model(ctx.session.model.to_string());
        ctx.renderer
            .write_line(&format!("btw: {}", query), Color::DarkGrey)?;
        match model.btw_query(query).await {
            Ok(response) => {
                ctx.renderer.write_line("", Color::White)?;
                let max_width = ctx.renderer.line_width();
                let styled =
                    crate::ui::markdown::markdown_to_styled(&response, max_width, c_agent());
                for span in styled {
                    ctx.renderer.write(&span.text, span.color)?;
                }
                ctx.renderer.write_line("", Color::White)?;
            }
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("btw error: {}", e), c_error())?;
            }
        }
    }
    Ok(())
}
