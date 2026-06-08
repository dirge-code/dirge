//! /why — explain a permission decision.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::ui::slash::{SlashCtx, c_error, c_result};

pub(crate) async fn cmd_why(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
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

    let Some(tool) = parts.get(1).copied() else {
        ctx.renderer.write_line(
            "usage: /why <tool> [input]   e.g. /why bash cargo test   ·   /why write src/main.rs",
            c_error(),
        )?;
        return Ok(());
    };

    let input = parts.get(2..).map(|s| s.join(" ")).unwrap_or_default();
    let is_path = crate::permission::engine::is_path_tool_name(tool);
    let report = {
        let guard = perm.lock_ignore_poison();
        guard.explain(tool, &input, is_path)
    };
    for line in report.lines() {
        ctx.renderer.write_line(line, c_result())?;
    }
    Ok(())
}
