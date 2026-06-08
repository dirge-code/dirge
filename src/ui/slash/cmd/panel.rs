//! /panel and /display handlers.

use crate::ui::renderer::{PanelMode, parse_display_spec};
use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_panel(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let arg = parts.get(1).copied().unwrap_or("").trim();
    let new_mode = match arg {
        "" => None,
        "on" => Some(PanelMode::On),
        "off" => Some(PanelMode::Off),
        "auto" => Some(PanelMode::Auto),
        "debug" => Some(PanelMode::Debug),
        other => {
            ctx.renderer.write_line(
                &format!("unknown /panel mode '{}' (use on|off|auto|debug)", other),
                c_error(),
            )?;
            return Ok(());
        }
    };
    if let Some(mode) = new_mode {
        if mode == PanelMode::Debug {
            ctx.renderer.set_right_panel_mode(mode);
        } else {
            ctx.renderer.set_panel_mode(mode);
        }
        ctx.renderer.render_viewport()?;
    }

    let left_mode = ctx.renderer.left_panel_mode();
    let right_mode = ctx.renderer.right_panel_mode();
    let left = ctx.renderer.left_panel_visible();
    let right = ctx.renderer.right_panel_visible();
    ctx.renderer.write_line(
        &format!(
            "left panel: {:?} ({})  right panel: {:?} ({}). Use /display for per-pane control.",
            left_mode,
            if left { "shown" } else { "hidden" },
            right_mode,
            if right { "shown" } else { "hidden" },
        ),
        c_agent(),
    )?;
    Ok(())
}

pub(crate) async fn cmd_display(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let spec = parts[1..].join(" ");
    if spec.trim().is_empty() {
        let left = ctx.renderer.left_panel_visible();
        let right = ctx.renderer.right_panel_visible();
        let mut shown = vec!["main"];
        if left {
            shown.insert(0, "left");
        }
        if right {
            shown.push("right");
        }
        ctx.renderer.write_line(
            &format!(
                "display: {} (usage: /display left|main|right)",
                shown.join("|")
            ),
            c_agent(),
        )?;
        return Ok(());
    }
    match parse_display_spec(&spec) {
        Ok(vis) => {
            ctx.renderer.set_pane_visibility(vis);
            ctx.renderer.render_viewport()?;
            let mut shown = vec!["main"];
            if vis.left {
                shown.insert(0, "left");
            }
            if vis.right {
                shown.push("right");
            }
            ctx.renderer
                .write_line(&format!("display: {}", shown.join("|")), c_agent())?;
        }
        Err(msg) => {
            ctx.renderer.write_line(&msg, c_error())?;
        }
    }
    Ok(())
}
