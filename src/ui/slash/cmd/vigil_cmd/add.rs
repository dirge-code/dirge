//! /vigil add toll|watcher|harbinger <name> [key=value ...] — add a new vigil.

use crate::extras::dirge_paths::ProjectPaths;
use crate::extras::vigil_db::VigilStore;
use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_vigil_add(
    ctx: &mut SlashCtx<'_>,
    parts: &[&str],
    _text: &str,
) -> anyhow::Result<()> {
    let trigger = parts.get(2).copied().unwrap_or("");
    let name = parts.get(3).copied().unwrap_or("");
    if trigger.is_empty() || name.is_empty() {
        ctx.renderer.write_line(
            "usage: /vigil add toll|watcher|harbinger <name> [key=value ...]",
            c_error(),
        )?;
        return Ok(());
    }

    let entry = match build_entry(trigger, name, &parts[4..]) {
        Ok(e) => e,
        Err(msg) => {
            ctx.renderer.write_line(&msg, c_error())?;
            return Ok(());
        }
    };

    let json = match serde_json::to_string(&entry) {
        Ok(j) => j,
        Err(e) => {
            ctx.renderer
                .write_line(&format!("failed to serialize: {e}"), c_error())?;
            return Ok(());
        }
    };

    let paths = ProjectPaths::new(
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    );
    match VigilStore::open(&paths) {
        Ok(store) => {
            if let Err(e) = store.upsert(name, &json) {
                ctx.renderer
                    .write_line(&format!("failed to save: {e}"), c_error())?;
                return Ok(());
            }
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("cannot open vigil store: {e}"), c_error())?;
            return Ok(());
        }
    }

    ctx.renderer.write_line(
        &format!("vigil '{name}' (trigger: {trigger}) added"),
        c_agent(),
    )?;
    Ok(())
}

fn build_entry(
    trigger: &str,
    name: &str,
    args: &[&str],
) -> Result<crate::config::VigilEntry, String> {
    use crate::config::{SocketMode, VigilEntry, VigilRite, VigilTrigger};

    let parsed: std::collections::HashMap<String, String> = args
        .iter()
        .filter_map(|a| a.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    let trigger = match trigger {
        "toll" => {
            let secs = parsed
                .get("interval_secs")
                .and_then(|v| v.parse().ok())
                .unwrap_or(30);
            VigilTrigger::Toll {
                interval_secs: secs,
            }
        }
        "watcher" => {
            let path = parsed
                .get("path")
                .cloned()
                .unwrap_or_else(|| ".".to_string());
            VigilTrigger::Watcher { path }
        }
        "harbinger" => {
            let address = parsed
                .get("address")
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:9000".to_string());
            let protocol = parsed.get("protocol").cloned().unwrap_or_default();
            VigilTrigger::Harbinger {
                address,
                protocol,
                socket_mode: SocketMode::Commands,
                commands: std::collections::HashMap::new(),
            }
        }
        other => {
            return Err(format!(
                "unknown trigger '{other}'. use: toll, watcher, or harbinger"
            ));
        }
    };

    let reap_interval_secs = parsed
        .get("reap_interval_secs")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);

    let prompt = parsed.get("prompt").cloned().unwrap_or_default();

    Ok(VigilEntry {
        name: name.to_string(),
        trigger,
        reap_interval_secs,
        prompt,
        procession: None,
        rite: Some(VigilRite {
            cmd: None,
            git_dirty: false,
        }),
    })
}
