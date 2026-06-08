//! /mode handler.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::permission::SecurityMode;
use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

pub(crate) async fn cmd_mode(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let current_mode = ctx
        .permission
        .as_ref()
        .map(|p| p.lock_ignore_poison().mode())
        .unwrap_or(SecurityMode::Standard);

    if parts.len() < 2 {
        ctx.renderer.write_line("security mode:", c_agent())?;
        ctx.renderer
            .write_line(&format!("  current: {}", current_mode), c_result())?;
        ctx.renderer.write_line("", c_agent())?;
        ctx.renderer.write_line(
            "  /mode standard      use configured permission rules",
            c_result(),
        )?;
        ctx.renderer
            .write_line("  /mode restrictive   default all tools to ask", c_result())?;
        ctx.renderer.write_line(
            "  /mode accept        auto-accept within working directory",
            c_result(),
        )?;
        ctx.renderer.write_line(
            "  /mode yolo          auto-accept ALL operations",
            c_result(),
        )?;
        ctx.renderer.write_line("", c_agent())?;
    } else {
        match parts[1] {
            "standard" => {
                if let Some(p) = ctx.permission {
                    p.lock_ignore_poison().set_mode(SecurityMode::Standard);
                    ctx.renderer
                        .write_line("security mode: standard", c_agent())?;
                } else {
                    ctx.renderer
                        .write_line("permission system not active", c_error())?;
                }
            }
            "restrictive" => {
                if let Some(p) = ctx.permission {
                    p.lock_ignore_poison().set_mode(SecurityMode::Restrictive);
                    ctx.renderer
                        .write_line("security mode: restrictive", c_agent())?;
                } else {
                    ctx.renderer
                        .write_line("permission system not active", c_error())?;
                }
            }
            "accept" => {
                if let Some(p) = ctx.permission {
                    p.lock_ignore_poison().set_mode(SecurityMode::Accept);
                    ctx.renderer
                        .write_line("security mode: accept (auto-allow within CWD)", c_agent())?;
                } else {
                    ctx.renderer
                        .write_line("permission system not active", c_error())?;
                }
            }
            "yolo" => {
                if let Some(p) = ctx.permission {
                    let deny_n = p.lock_ignore_poison().deny_rule_count();
                    p.lock_ignore_poison().set_mode(SecurityMode::Yolo);
                    ctx.renderer
                        .write_line("security mode: YOLO (all operations allowed)", c_agent())?;
                    if deny_n > 0 {
                        ctx.renderer.write_line(
                            &format!(
                                "warning: your config has {deny_n} deny rule(s) — Yolo IGNORES them. Switch back with /mode standard to honor deny rules.",
                            ),
                            c_error(),
                        )?;
                    }
                } else {
                    ctx.renderer
                        .write_line("permission system not active", c_error())?;
                }
            }
            _ => {
                ctx.renderer
                    .write_line(&format!("unknown mode: {}", parts[1]), c_error())?;
            }
        }
    }
    Ok(())
}
