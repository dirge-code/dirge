//! /prompt — list available prompts.

use crate::context::prompts::PromptSource;
use crate::ui::slash::{SlashCtx, c_agent, c_result};

pub(crate) async fn cmd_prompt_list(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let mut sorted: Vec<String> = ctx.context.prompts.keys().cloned().collect();
    sorted.sort();

    if sorted.is_empty() {
        ctx.renderer.write_line("no prompts available", c_agent())?;
    } else {
        let current = ctx
            .context
            .current_prompt_name
            .as_deref()
            .unwrap_or("(none)")
            .to_string();
        ctx.renderer.write_line(
            &format!("available prompts (current: {}):", current),
            c_agent(),
        )?;
        let max_name = sorted.iter().map(|n| n.len()).max().unwrap_or(0);
        for name in &sorted {
            let prompt = ctx.context.prompts.get(name);
            // Provenance badge for non-embedded prompts so a global/project
            // override of a built-in is visible. Mirrors `/agents`.
            let badge = match prompt.map(|p| p.source) {
                Some(s) if s != PromptSource::Embedded => format!(" [{}]", s.label()),
                _ => String::new(),
            };
            match prompt.and_then(|p| p.description.as_deref()) {
                Some(d) => ctx.renderer.write_line(
                    &format!("  {:<width$}{}  {}", name, badge, d, width = max_name),
                    c_result(),
                )?,
                None if badge.is_empty() => ctx
                    .renderer
                    .write_line(&format!("  {}", name), c_result())?,
                None => ctx.renderer.write_line(
                    &format!("  {:<width$}{}", name, badge, width = max_name),
                    c_result(),
                )?,
            }
        }
        ctx.renderer.write_line("", c_agent())?;
        ctx.renderer
            .write_line("usage: /prompt <name>  |  /prompt default", c_result())?;
    }
    Ok(())
}
