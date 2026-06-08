//! /prompt command dispatch.

pub(crate) mod default;
pub(crate) mod list;
pub(crate) mod switch;

use crate::ui::slash::SlashCtx;

pub(crate) async fn cmd_prompt(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 {
        return list::cmd_prompt_list(ctx).await;
    }
    if parts[1] == "default" && !ctx.context.prompts.contains_key("default") {
        return default::cmd_prompt_default(ctx).await;
    }
    switch::cmd_prompt_switch(ctx, parts[1].trim()).await
}
