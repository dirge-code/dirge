//! /addons handler: list the Clojure addons, and hand `/addons reload` and
//! the slash commands addons register to the event loop as jobs
//! ([`crate::ui::addon_phase`]).

#[cfg(feature = "addons")]
use std::sync::Arc;

#[cfg(feature = "addons")]
use crate::addons::domain::CommandSpec;
#[cfg(feature = "addons")]
use crate::addons::host::AddonHost;
#[cfg(feature = "addons")]
use crate::ui::addon_phase::AddonJob;
use crate::ui::slash::{SlashCtx, SlashOutcome, c_error};
#[cfg(feature = "addons")]
use crate::ui::slash::{c_agent, c_result};
#[cfg(feature = "addons")]
use crate::ui::theme;

/// `/addons [list|reload]`. `reload` answers a job for the event loop.
pub(crate) fn cmd_addons(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<SlashOutcome> {
    #[cfg(not(feature = "addons"))]
    {
        let _ = parts;
        ctx.renderer.write_line(
            "addons are disabled in this build (enable the 'addons' feature)",
            c_error(),
        )?;
        Ok(SlashOutcome::Handled)
    }

    #[cfg(feature = "addons")]
    match parts.get(1).copied() {
        None | Some("list") => list(ctx).map(|()| SlashOutcome::Handled),
        Some("reload") => Ok(SlashOutcome::DeferAddon(AddonJob::Reload {
            settings: ctx.cfg.addons.clone().unwrap_or_default(),
        })),
        Some(other) => {
            ctx.renderer
                .write_line(&format!("unknown /addons subcommand: {other}"), c_error())?;
            ctx.renderer
                .write_line("usage: /addons [list|reload]", c_agent())?;
            Ok(SlashOutcome::Handled)
        }
    }
}

#[cfg(feature = "addons")]
fn list(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let renderer = &mut *ctx.renderer;
    let Some(host) = crate::addons::global() else {
        renderer.write_line(
            "no addons loaded: put an addon under .dirge/addons/ or ~/.config/dirge/addons/, then /addons reload",
            c_error(),
        )?;
        return Ok(());
    };
    let addons = host.addons();
    let hook_keys = host.hook_keys();
    renderer.write_line(&format!("loaded {} addon(s):", addons.len()), c_agent())?;
    if let Some(endpoint) = host.repl_endpoint() {
        renderer.write_line(&format!("  nREPL    : {endpoint}"), theme::dim())?;
    }
    for addon in &addons {
        let status = addon
            .health
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("?");
        renderer.write_line(&format!("  {} ({status})", addon.id), c_result())?;
        renderer.write_line(
            &format!("    manifest : {}", addon.manifest.display()),
            theme::dim(),
        )?;
        let rows = [
            (
                "tools   ",
                addon
                    .tools
                    .iter()
                    .map(|t| t.exposed_name.clone())
                    .collect::<Vec<_>>(),
            ),
            (
                "hooks   ",
                hook_keys
                    .iter()
                    .find(|(id, _)| *id == addon.id)
                    .map(|(_, keys)| keys.clone())
                    .unwrap_or_else(|| addon.hooks.iter().map(|h| h.key().to_string()).collect()),
            ),
            (
                "commands",
                addon
                    .commands
                    .iter()
                    .map(|c| format!("/{}", c.name))
                    .collect(),
            ),
        ];
        for (label, names) in rows {
            if !names.is_empty() {
                renderer
                    .write_line(&format!("    {label} : {}", names.join(", ")), theme::dim())?;
            }
        }
    }
    let failures = host.failures();
    if !failures.is_empty() {
        renderer.write_line("failed to load:", c_error())?;
        for failure in &failures {
            renderer.write_line(
                &format!("  {}: {}", failure.manifest.display(), failure.error),
                c_error(),
            )?;
        }
    }
    Ok(())
}

/// `/name args` for a command an addon registered, as a job for the event
/// loop. `text` is the whole typed line; the handler gets what follows the
/// command's name.
#[cfg(feature = "addons")]
pub(crate) fn command_job(host: Arc<AddonHost>, command: CommandSpec, text: &str) -> SlashOutcome {
    SlashOutcome::DeferAddon(AddonJob::Command {
        host,
        command,
        args: command_args(text).to_string(),
    })
}

/// The text typed after a command's name.
#[cfg(feature = "addons")]
fn command_args(text: &str) -> &str {
    text.trim_start()
        .split_once(char::is_whitespace)
        .map_or("", |(_, rest)| rest)
}

#[cfg(all(test, feature = "addons"))]
mod tests {
    use super::*;
    use crate::addons::host::tests::ScriptedRuntime;

    #[test]
    fn a_command_hands_the_loop_a_job_with_the_text_after_its_name() {
        let runtime = Arc::new(ScriptedRuntime::default());
        let host = Arc::new(AddonHost::new(runtime.clone(), Vec::new(), Vec::new()));
        let command = CommandSpec {
            addon_id: "a".into(),
            name: "rows".into(),
            description: String::new(),
        };
        let outcome = command_job(host, command, "/rows 1 2  3");
        let SlashOutcome::DeferAddon(AddonJob::Command { command, args, .. }) = outcome else {
            panic!("a command job, got {outcome:?}");
        };
        assert_eq!((command.name.as_str(), args.as_str()), ("rows", "1 2  3"));
        assert!(
            runtime.calls.lock().unwrap().is_empty(),
            "the handler has not run"
        );
    }

    #[test]
    fn command_args_are_empty_for_a_bare_command() {
        assert_eq!(command_args("/rows"), "");
        assert_eq!(command_args("  /rows x"), "x");
    }
}
