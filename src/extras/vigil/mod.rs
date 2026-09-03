//! Vigil heartbeat/wakeup runtime.
//!
//! Public API:
//! - `VigilKeeper::from_entries()` — build keeper from config entries.
//! - `VigilKeeper::run()` — start the reaper and all triggers, return when shutdown.
//!
//! Internal modules:
//! - `types` — VigilEvent, VigilInstance, VigilCtl
//! - `rite` — gate check evaluation
//! - `dispatch` — commands-mode template substitution
//! - `toll` — timer trigger
//! - `watcher` — filesystem trigger
//! - `harbinger` — socket trigger
//! - `reaper` — event drain + coalesce + observance dispatch

pub mod dispatch;
pub mod harbinger;
pub mod reaper;
pub mod rite;
pub mod toll;
pub mod types;
pub mod watcher;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::VigilEntry;

use self::reaper::Observance;
use self::types::{
    HookDispatchRequest, TriggerKind, VigilCtl, VigilEvent, VigilInstance, VigilReapInput,
};

/// Simple runtime state for vigil mode — exposed to the UI loop so it knows
/// whether to sleep between observances and carries pending observance data
/// so the post-turn handler can dispatch on-vigil-observance with :response.
#[allow(dead_code)] // consumed by the interactive TUI loop (slice 3)
pub struct VigilState {
    pub active: bool,
    /// If set, the current agent turn is a vigil observance. The post-turn
    /// handler reads this to dispatch `on-vigil-observance` with the agent's
    /// response text. Cleared after dispatch.
    pub pending_observance: Option<PendingObservance>,
}

/// Metadata for a vigil observance that will fire after the agent turn.
#[derive(Debug, Clone)]
#[allow(dead_code)] // consumed by the interactive TUI loop (slice 3)
pub struct PendingObservance {
    pub vigil_name: String,
    pub event_count: usize,
    pub running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Format the `on-vigil-observance` hook context string, escaping the vigil
/// name and agent response for the Janet `@{...}` template.
pub fn observance_context(vigil_name: &str, event_count: usize, response: &str) -> String {
    let escaped_name = vigil_name.replace('\\', "\\\\").replace('"', "\\\"");
    let escaped_response = response.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "@{{:vigil \"{}\" :count {} :response \"{}\" :exit :ok}}",
        escaped_name, event_count, escaped_response
    )
}

/// The vigil-keeper — owns all active vigils, starts triggers, runs the reaper.
pub struct VigilKeeper {
    pub vigils: Vec<VigilInstance>,
    #[allow(dead_code)]
    pub ctl_tx: Option<mpsc::Sender<VigilCtl>>,
    pub observance_rx: Option<mpsc::Receiver<Observance>>,
    /// Untyped wake channel — fires on every observance so the select! loop
    /// (which can't cfg-gate arms) can wake and drain the typed receiver.
    pub wake_rx: Option<mpsc::UnboundedReceiver<()>>,
    /// Hook dispatch channel — trigger producers and reaper send hook requests;
    /// the UI loop drains them.
    pub hook_rx: Option<mpsc::Receiver<HookDispatchRequest>>,
    /// Janet plugin event sender — installed into the plugin bridge at startup.
    /// Plugins call `(vigil/emit name data)` and the keeper routes events to
    /// the correct vigil's event queue.
    #[allow(dead_code)]
    pub vigil_plugin_tx: Option<mpsc::Sender<String>>,
}

impl VigilKeeper {
    /// Build a vigil-keeper from config entries. Creates per-vigil channels
    /// and spawns trigger tasks.
    pub fn from_entries(
        entries: Vec<VigilEntry>,
        paused_names: std::collections::HashSet<String>,
    ) -> Result<Self, String> {
        let (ctl_tx, ctl_rx) = mpsc::channel::<VigilCtl>(32);
        let (obs_tx, obs_rx) = mpsc::channel::<Observance>(64);
        let (wake_tx, wake_rx) = mpsc::unbounded_channel::<()>();
        let (hook_tx, hook_rx) = mpsc::channel::<HookDispatchRequest>(64);

        let mut vigils = Vec::new();
        let mut reap_inputs: Vec<VigilReapInput> = Vec::new();

        for entry in entries {
            let (tx, rx) = types::make_vigil_channel(256);
            let running = Arc::new(AtomicBool::new(false));

            let name = entry.name.clone();
            if entry.reap_interval_secs == 0 {
                return Err(format!(
                    "vigil {name}: reap_interval_secs must be greater than zero"
                ));
            }
            if let crate::config::VigilTrigger::Toll { interval_secs: 0 } = &entry.trigger {
                return Err(format!(
                    "vigil {name}: toll interval_secs must be greater than zero"
                ));
            }
            let interval = entry.reap_interval_secs;
            let prompt = entry.prompt.clone();
            let procession = entry.procession.clone();

            let trigger_kind = match &entry.trigger {
                crate::config::VigilTrigger::Toll { .. } => TriggerKind::Toll,
                crate::config::VigilTrigger::Watcher { .. } => TriggerKind::Watcher,
                crate::config::VigilTrigger::Harbinger { .. } => TriggerKind::Harbinger,
            };

            // Spawn trigger(s) based on type.
            match entry.trigger {
                crate::config::VigilTrigger::Toll { interval_secs } => {
                    toll::spawn_toll(name.clone(), interval_secs, tx.clone(), hook_tx.clone());
                }
                crate::config::VigilTrigger::Watcher { path, .. } => {
                    let watch_path = std::path::PathBuf::from(&path);
                    if let Err(e) = watcher::spawn_watcher(
                        name.clone(),
                        watch_path,
                        tx.clone(),
                        hook_tx.clone(),
                    ) {
                        warn!(%name, "failed to spawn watcher, skipping vigil: {e}");
                        continue;
                    }
                }
                crate::config::VigilTrigger::Harbinger {
                    address,
                    protocol,
                    socket_mode,
                    commands,
                } => {
                    let port: u16 = address
                        .strip_prefix("127.0.0.1:")
                        .or_else(|| address.strip_prefix("localhost:"))
                        .and_then(|p| p.parse().ok())
                        .unwrap_or(0);
                    if port == 0 {
                        return Err(format!(
                            "vigil {name}: invalid harbinger address '{address}'"
                        ));
                    }

                    if !protocol.is_empty() && protocol != "tcp" {
                        return Err(format!(
                            "vigil {name}: unsupported harbinger protocol '{protocol}' (only 'tcp' is supported)"
                        ));
                    }

                    let has_commands = matches!(socket_mode, crate::config::SocketMode::Commands);
                    if has_commands && commands.is_empty() {
                        return Err(format!(
                            "vigil {name}: commands mode requires non-empty commands map"
                        ));
                    }

                    if let Err(e) = harbinger::spawn_harbinger(
                        name.clone(),
                        port,
                        commands,
                        has_commands,
                        tx.clone(),
                        hook_tx.clone(),
                    ) {
                        warn!(%name, "failed to spawn harbinger, skipping vigil: {e}");
                        continue;
                    }
                }
            }

            let rite = entry.rite.clone();

            vigils.push(VigilInstance {
                name: name.clone(),
                reap_interval_secs: interval,
                prompt: prompt.clone(),
                procession: procession.clone(),
                tx: tx.clone(),
                running: running.clone(),
            });

            reap_inputs.push(VigilReapInput {
                name: name.clone(),
                trigger: trigger_kind,
                reap_interval_secs: interval,
                rx,
                running,
                rite,
                prompt,
                procession,
            });
        }

        // Build a map of vigil name → sender for procession chaining.
        let senders: std::collections::HashMap<String, mpsc::Sender<VigilEvent>> = vigils
            .iter()
            .map(|v| (v.name.clone(), v.tx.clone()))
            .collect();

        // Clone senders for the Janet plugin bridge router so plugins
        // calling (vigil/emit name data) can push events into any vigil's queue.
        let router_senders = senders.clone();
        let (vigil_plugin_tx, mut vigil_plugin_rx) = mpsc::channel::<String>(256);
        tokio::spawn(async move {
            while let Some(msg) = vigil_plugin_rx.recv().await {
                match msg.split_once('\t') {
                    Some((name, payload)) => {
                        if let Some(sender) = router_senders.get(name) {
                            let context: serde_json::Value = serde_json::from_str(payload)
                                .unwrap_or_else(|_| serde_json::json!({"data": payload}));
                            let event = VigilEvent {
                                vigil_name: name.to_string(),
                                trigger: crate::extras::vigil::types::TriggerKind::Toll,
                                context,
                                timestamp: chrono::Utc::now(),
                            };
                            if sender.try_send(event).is_err() {
                                warn!(%name, "vigil plugin event queue full, dropping");
                            }
                        } else {
                            warn!(%name, "vigil/emit for unknown vigil, dropping event");
                        }
                    }
                    None => {
                        warn!("vigil/emit received malformed message, dropping");
                    }
                }
            }
        });

        // Launch the reaper in a background task.
        let reaper_wake_tx = wake_tx;
        let reaper_hook_tx = hook_tx;
        tokio::spawn(async move {
            let paused = paused_names;
            reaper::run_reaper(
                reap_inputs,
                obs_tx,
                ctl_rx,
                Some(reaper_wake_tx),
                senders,
                reaper_hook_tx,
                paused,
            )
            .await;
        });

        Ok(Self {
            vigils,
            ctl_tx: Some(ctl_tx),
            observance_rx: Some(obs_rx),
            wake_rx: Some(wake_rx),
            hook_rx: Some(hook_rx),
            vigil_plugin_tx: Some(vigil_plugin_tx),
        })
    }

    /// Build a vigil-keeper from config entries + `.dirge/vigils/*.json` files.
    /// Filesystem entries are merged by name; config entries win on collision.
    pub fn from_config_and_filesystem(
        entries: Vec<VigilEntry>,
        paused_names: std::collections::HashSet<String>,
    ) -> Result<Self, String> {
        let mut merged = entries;

        // Scan .dirge/vigils/*.json for filesystem-defined vigils.
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let vigils_dir = crate::extras::dirge_paths::ProjectPaths::new(&cwd).vigils_dir();
        #[allow(clippy::collapsible_if)]
        if vigils_dir.is_dir() {
            if let Ok(readdir) = std::fs::read_dir(&vigils_dir) {
                for entry in readdir.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    match std::fs::read_to_string(&path) {
                        Ok(content) => match serde_json::from_str::<VigilEntry>(&content) {
                            Ok(file_entry) => {
                                // Config wins — only add if not already present.
                                let name = &file_entry.name;
                                if !merged.iter().any(|e| e.name == *name) {
                                    let file_path = path.display();
                                    info!(%name, file = %file_path, "imported vigil from filesystem");
                                    merged.push(file_entry);
                                } else {
                                    info!(%name, "vigil from filesystem skipped: config entry wins on name collision");
                                }
                            }
                            Err(e) => {
                                let file_path = path.display();
                                warn!(file = %file_path, "invalid vigil JSON, skipping: {e}");
                            }
                        },
                        Err(e) => {
                            let file_path = path.display();
                            warn!(file = %file_path, "cannot read vigil file, skipping: {e}");
                        }
                    }
                }
            }
        }

        // Import vigils added via `dirge vigil add` / `/vigil add`, which are
        // persisted to the SQLite store rather than config or the filesystem
        // dir. Without this the add paths are dead ends: the keeper would never
        // load them. Config/filesystem entries win on name collision, and
        // `list_non_resting` already excludes vigils the user put to rest.
        let db_path = crate::extras::dirge_paths::ProjectPaths::new(&cwd).session_db_path();
        if db_path.exists()
            && let Ok(store) = crate::extras::vigil_db::VigilStore::open_at(&db_path)
        {
            for row in store.list_non_resting().unwrap_or_default() {
                let name = row.name.clone();
                match serde_json::from_str::<VigilEntry>(&row.payload_json) {
                    Ok(db_entry) => {
                        if !merged.iter().any(|e| e.name == name) {
                            info!(%name, "imported vigil from store");
                            merged.push(db_entry);
                        } else {
                            info!(%name, "vigil from store skipped: config/filesystem entry wins on name collision");
                        }
                    }
                    Err(e) => {
                        warn!(%name, "invalid vigil payload in store, skipping: {e}");
                    }
                }
            }

            // DB state is authoritative even when the payload came from
            // config or a filesystem file (which win on name collision
            // above): a vigil laid to rest must not be reaped on the next
            // run. `list_non_resting` only covers store-only vigils, so
            // drop any merged entry whose DB row is resting.
            merged.retain(|e| {
                let resting = matches!(
                    store.get(&e.name),
                    Ok(Some(crate::extras::vigil_db::VigilRow {
                        status: crate::extras::vigil_db::VigilStatus::Resting,
                        ..
                    }))
                );
                if resting {
                    info!(name = %e.name, "vigil laid to rest - skipping on next run");
                }
                !resting
            });
        }

        Self::from_entries(merged, paused_names)
    }

    /// Signal the reaper to stop.
    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        if let Some(ref tx) = self.ctl_tx {
            let _ = tx.send(VigilCtl::Shutdown).await;
        }
        info!("vigil-keeper shutdown complete");
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod observance_context_tests {
    use super::observance_context;

    #[test]
    fn formats_simple_event() {
        assert_eq!(
            observance_context("jenkins-remediate", 2, "done"),
            "@{:vigil \"jenkins-remediate\" :count 2 :response \"done\" :exit :ok}"
        );
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(
            observance_context("a\"b", 1, "c\\d"),
            "@{:vigil \"a\\\"b\" :count 1 :response \"c\\\\d\" :exit :ok}"
        );
    }
}
