//! /sessions command dispatch.

pub(crate) mod delete;
pub(crate) mod list;
pub(crate) mod switch;

use crate::ui::slash::SlashCtx;

pub(crate) async fn cmd_sessions(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 {
        return list::cmd_sessions_list(ctx).await;
    }
    if parts[1] == "delete" && parts.len() >= 3 {
        return delete::cmd_sessions_delete(ctx, parts[2].trim()).await;
    }
    switch::cmd_sessions_switch(ctx, parts[1].trim()).await
}
