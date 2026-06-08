//! /allow command dispatch.

pub(crate) mod add;
pub(crate) mod clear;
pub(crate) mod list;
pub(crate) mod remove;
pub(crate) mod why;

use crate::ui::slash::{SlashCtx, c_error};

pub(crate) async fn cmd_allow(
    ctx: &mut SlashCtx<'_>,
    parts: &[&str],
    text: &str,
) -> anyhow::Result<()> {
    let sub = parts.get(1).copied().unwrap_or("list");
    let perm = match ctx.permission {
        Some(p) => p,
        None => {
            ctx.renderer.write_line(
                "permission system unavailable (--no-tools mode?)",
                c_error(),
            )?;
            return Ok(());
        }
    };
    match sub {
        "list" => list::cmd_allow_list(ctx, perm).await,
        "add" => add::cmd_allow_add(ctx, perm, text).await,
        "remove" => remove::cmd_allow_remove(ctx, perm, parts).await,
        "clear" => clear::cmd_allow_clear(ctx, perm).await,
        _ => {
            ctx.renderer.write_line(
                "usage: /allow [list|add <tool> <pattern>|remove <idx>|clear]",
                c_error(),
            )?;
            Ok(())
        }
    }
}
