//! /plugins handler — list all loaded plugins with their file paths and hooks.

use crate::ui::slash::{SlashCtx, c_error};

#[cfg(feature = "plugin")]
use crate::sync_util::LockExt;
#[cfg(feature = "plugin")]
use crate::ui::slash::{c_agent, c_result};
#[cfg(feature = "plugin")]
use crate::ui::theme;

pub(crate) async fn cmd_plugins(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let renderer = &mut *ctx.renderer;

    #[cfg(feature = "plugin")]
    if let Some(pm_arc) = crate::plugin::hook::global() {
        let plugins = {
            let mgr = pm_arc.lock_ignore_poison();
            mgr.list_plugins()
        };

        if plugins.is_empty() {
            renderer.write_line("no plugins loaded", c_error())?;
            return Ok(());
        }

        renderer.write_line(&format!("loaded {} plugin(s):", plugins.len()), c_agent())?;

        for p in &plugins {
            // Derive the source directory from the first file's parent
            let source = p
                .files
                .first()
                .and_then(|f| f.parent())
                .and_then(|d| d.to_str())
                .unwrap_or("?");

            let file_list: String = p
                .files
                .iter()
                .filter_map(|f| f.file_name().and_then(|n| n.to_str()))
                .collect::<Vec<_>>()
                .join(", ");

            renderer.write_line(&format!("  {}", p.stem), c_result())?;
            renderer.write_line(&format!("    source : {}", source), theme::dim())?;
            renderer.write_line(&format!("    files  : {}", file_list), theme::dim())?;
            if !p.hooks_registered.is_empty() {
                renderer.write_line(
                    &format!("    hooks  : {}", p.hooks_registered.join(", ")),
                    theme::dim(),
                )?;
            }
        }
    }

    #[cfg(not(feature = "plugin"))]
    {
        renderer.write_line(
            "plugins are disabled in this build (enable the 'plugin' feature)",
            c_error(),
        )?;
    }

    Ok(())
}
