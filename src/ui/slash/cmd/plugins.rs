//! /plugins handler — list, load, and manage plugins.

use crate::ui::slash::{SlashCtx, c_error};

#[cfg(feature = "plugin")]
use crate::sync_util::LockExt;
#[cfg(feature = "plugin")]
use crate::ui::slash::{c_agent, c_result};
#[cfg(feature = "plugin")]
use crate::ui::theme;

pub(crate) async fn cmd_plugins(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let sub = parts.get(1).copied();

    match sub {
        Some("load") => cmd_load(ctx, &parts[2..]).await?,
        None => cmd_list(ctx).await?,
        Some(other) => {
            ctx.renderer
                .write_line(&format!("unknown /plugins subcommand: {other}"), c_error())?;
            #[cfg(feature = "plugin")]
            print_usage(ctx).await?;
        }
    }
    Ok(())
}

/// List all loaded plugins.
pub(crate) async fn cmd_list(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
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

/// /plugins load <path>          — load a single plugin file or directory
/// /plugins load all             — load every .janet file from ~/.config/dirge/plugins/ and .dirge/plugins/
async fn cmd_load(ctx: &mut SlashCtx<'_>, args: &[&str]) -> anyhow::Result<()> {
    #[cfg(not(feature = "plugin"))]
    {
        let _ = args;
        ctx.renderer.write_line(
            "plugins are disabled in this build (enable the 'plugin' feature)",
            c_error(),
        )?;
        Ok(())
    }

    #[cfg(feature = "plugin")]
    {
        let target = match args.first() {
            Some(s) if !s.is_empty() => *s,
            _ => {
                ctx.renderer
                    .write_line("usage: /plugins load <path|all>", c_error())?;
                return Ok(());
            }
        };

        let pm_arc = match crate::plugin::hook::global() {
            Some(pm) => pm,
            None => {
                ctx.renderer
                    .write_line("plugin manager not available", c_error())?;
                return Ok(());
            }
        };

        if target == "all" {
            load_all(ctx, &pm_arc).await?;
        } else {
            load_one(ctx, &pm_arc, target).await?;
        }
        Ok(())
    }
}

#[cfg(feature = "plugin")]
async fn load_one(
    ctx: &mut SlashCtx<'_>,
    pm_arc: &std::sync::Arc<std::sync::Mutex<crate::plugin::PluginManager>>,
    target: &str,
) -> anyhow::Result<()> {
    let path = std::path::Path::new(target);
    if !path.exists() {
        ctx.renderer
            .write_line(&format!("plugin not found: {target}"), c_error())?;
        return Ok(());
    }

    let loaded = {
        let mut mgr = pm_arc.lock_ignore_poison();
        crate::plugin::load_plugin(&mut mgr, path)
    };

    match loaded {
        Ok(desc) => {
            ctx.renderer.write_line(
                &format!(
                    "loaded plugin '{}' ({} file(s), {} hook(s))",
                    desc.stem,
                    desc.files.len(),
                    desc.hooks_registered.len()
                ),
                c_result(),
            )?;
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("failed to load plugin: {e}"), c_error())?;
        }
    }

    Ok(())
}

#[cfg(feature = "plugin")]
async fn load_all(
    ctx: &mut SlashCtx<'_>,
    pm_arc: &std::sync::Arc<std::sync::Mutex<crate::plugin::PluginManager>>,
) -> anyhow::Result<()> {
    use std::collections::HashSet;
    use std::path::PathBuf;

    // Project plugins (.dirge/plugins) loaded first so they take
    // precedence over ~/.config/dirge/plugins/ when two plugins share
    // the same name. The project dir is resolved via ProjectPaths
    // (git-root walk-up / DIRGE_PROJECT_ROOT), matching the startup
    // loader, so a subdirectory launch still finds it (dirge-vpma.17).
    let project_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let candidate_dirs: Vec<PathBuf> = vec![
        crate::extras::dirge_paths::ProjectPaths::new(&project_cwd).plugins_dir(),
        crate::session::storage::config_path().join("plugins"),
    ];
    let search_dirs = crate::plugin::filter_existing_dirs(&candidate_dirs);

    if search_dirs.is_empty() {
        ctx.renderer.write_line(
            "no plugin directories found — add .janet files to .dirge/plugins/ or ~/.config/dirge/plugins/",
            c_error(),
        )?;
        return Ok(());
    }

    let mut loaded = 0u32;
    let mut skipped = 0u32;
    let mut errors: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for dir in &search_dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                errors.push(format!("cannot read {}: {e}", dir.display()));
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let is_janet_file = path.is_file() && path.extension().is_some_and(|e| e == "janet");
            let is_plugin_dir = path.is_dir();
            if !is_janet_file && !is_plugin_dir {
                continue;
            }

            let name = if is_plugin_dir {
                path.file_name()
            } else {
                path.file_stem()
            }
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();

            if !seen.insert(name.clone()) {
                // cwd dir processed first — this is a config-dir duplicate
                skipped += 1;
                ctx.renderer.write_line(
                    &format!(
                        "  skipping {} (overridden by .dirge/plugins/{})",
                        path.display(),
                        name,
                    ),
                    theme::dim(),
                )?;
                continue;
            }

            let res = {
                let mut mgr = pm_arc.lock_ignore_poison();
                crate::plugin::load_plugin(&mut mgr, &path)
            };
            match res {
                Ok(desc) => {
                    loaded += 1;
                    ctx.renderer.write_line(
                        &format!(
                            "  {} → loaded '{}' ({} hook(s))",
                            path.display(),
                            desc.stem,
                            desc.hooks_registered.len(),
                        ),
                        c_result(),
                    )?;
                }
                Err(e) => {
                    errors.push(format!("{}: {e}", path.display()));
                }
            }
        }
    }

    let summary = if skipped > 0 {
        format!("done: {loaded} loaded, {skipped} skipped")
    } else {
        format!("done: {loaded} loaded")
    };
    ctx.renderer.write_line(&summary, c_agent())?;

    for err in &errors {
        ctx.renderer
            .write_line(&format!("  error: {err}"), c_error())?;
    }

    Ok(())
}

#[cfg(feature = "plugin")]
async fn print_usage(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    ctx.renderer
        .write_line("usage: /plugins [load <path|all>]", c_agent())?;
    ctx.renderer
        .write_line("  /plugins                 list loaded plugins", c_result())?;
    ctx.renderer.write_line(
        "  /plugins load <path>     load a plugin file or directory",
        c_result(),
    )?;
    ctx.renderer.write_line(
        "  /plugins load all        load every .janet file from .dirge/plugins/ and ~/.config/dirge/plugins/",
        c_result(),
    )?;
    Ok(())
}
