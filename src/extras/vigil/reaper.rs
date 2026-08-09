//! Reaper — drains per-vigil event channels on configurable cadences.
//! Uses `FuturesUnordered` so each vigil reaps independently; one vigil's
//! slow observance doesn't delay another's reap.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::dispatch::build_prompt;
use super::rite::evaluate_rite;
use super::types::{
    CoalescedBatch, RiteResult, TriggerKind, VigilEvent, VigilReapInput, VigilStatusInfo,
};

/// Context passed to the agent executor for an observance.
#[derive(Debug, Clone)]
pub struct Observance {
    pub vigil_name: String,
    pub prompt: String,
    pub context: serde_json::Value,
    pub event_count: usize,
    pub running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Run the reaper loop. Drains events from all active vigils, coalesces them
/// per reap window, runs rite gates, and produces `Observance`s.
/// Observances are sent to `observance_tx` for the vigil-keeper to dispatch.
pub async fn run_reaper(
    vigils: Vec<VigilReapInput>,
    observance_tx: mpsc::Sender<Observance>,
    mut ctl_rx: mpsc::Receiver<super::types::VigilCtl>,
    wake_tx: Option<mpsc::UnboundedSender<()>>,
    senders: HashMap<String, mpsc::Sender<VigilEvent>>,
    hook_tx: mpsc::Sender<super::types::HookDispatchRequest>,
    initial_paused: std::collections::HashSet<String>,
) {
    let mut reap_tasks: FuturesUnordered<tokio::task::JoinHandle<(String, Vec<VigilEvent>)>> =
        FuturesUnordered::new();

    // Lookup maps for metadata accessed in the reap-results arm.
    let running: HashMap<String, Arc<AtomicBool>> = vigils
        .iter()
        .map(|v| (v.name.clone(), v.running.clone()))
        .collect();
    let prompt_map: HashMap<String, String> = vigils
        .iter()
        .map(|v| (v.name.clone(), v.prompt.clone()))
        .collect();
    let rite_map: HashMap<String, Option<crate::config::VigilRite>> = vigils
        .iter()
        .map(|v| (v.name.clone(), v.rite.clone()))
        .collect();
    let procession_map: HashMap<String, Option<String>> = vigils
        .iter()
        .map(|v| (v.name.clone(), v.procession.clone()))
        .collect();
    // Infer trigger kind from the vigil config.
    let trigger_map: HashMap<String, TriggerKind> =
        vigils.iter().map(|v| (v.name.clone(), v.trigger)).collect();

    let mut paused: std::collections::HashSet<String> = initial_paused;

    let reap_interval_map: HashMap<String, u64> = vigils
        .iter()
        .map(|v| (v.name.clone(), v.reap_interval_secs))
        .collect();

    for mut input in vigils {
        let name = input.name.clone();
        let interval = input.reap_interval_secs;
        reap_tasks.push(tokio::spawn(async move {
            reap_interval(name, interval, &mut input.rx).await
        }));
    }

    // Per-vigil reap statistics, updated each reap window and exposed via StatusReq.
    type ReapStats = HashMap<String, (usize, chrono::DateTime<chrono::Utc>)>;
    let reap_stats: Arc<Mutex<ReapStats>> = Arc::new(Mutex::new(HashMap::new()));

    loop {
        tokio::select! {
            Some(ctl) = ctl_rx.recv() => {
                match ctl {
                    super::types::VigilCtl::Shutdown => {
                        info!("reaper shutting down");
                        break;
                    }
                    super::types::VigilCtl::Pause { name } => {
                        debug!(%name, "reaper pausing vigil");
                        paused.insert(name);
                    }
                    super::types::VigilCtl::PauseAll => {
                        debug!("reaper pausing all vigils");
                        for name in running.keys() {
                            paused.insert(name.clone());
                        }
                    }
                    super::types::VigilCtl::Resume { name } => {
                        debug!(%name, "reaper resuming vigil");
                        paused.remove(&name);
                    }
                    super::types::VigilCtl::ResumeAll => {
                        debug!("reaper resuming all vigils");
                        paused.clear();
                    }
                    super::types::VigilCtl::StatusReq { respond_to } => {
                        let mut statuses = Vec::new();
                        let stats = reap_stats.lock().unwrap();
                        for (name, run_flag) in &running {
                            let trigger = trigger_map.get(name).copied().unwrap_or(TriggerKind::Toll);
                            let interval = reap_interval_map.get(name).copied().unwrap_or(0);
                            let (count, ts) = stats.get(name).copied().unwrap_or((0, chrono::Utc::now()));
                            statuses.push(VigilStatusInfo {
                                name: name.clone(),
                                trigger,
                                reap_interval_secs: interval,
                                running: run_flag.load(std::sync::atomic::Ordering::Relaxed),
                                paused: paused.contains(name),
                                last_event_count: count,
                                last_event_at: Some(ts.to_rfc3339()),
                            });
                        }
                        let _ = respond_to.send(statuses);
                    }
                }
            }
            Some(result) = reap_tasks.next() => {
                match result {
                    Ok((vigil_name, events)) => {
                        if events.is_empty() {
                            continue;
                        }

                        // Track event count and timestamp for the panel indicator.
                        {
                            let mut stats = reap_stats.lock().unwrap();
                            stats.insert(vigil_name.clone(), (events.len(), chrono::Utc::now()));
                        }

                        if paused.contains(&vigil_name) {
                            warn!(%vigil_name, "skipping reap — vigil paused");
                            continue;
                        }

                        // Check if an observance is already running for this vigil.
                        let run_flag = running.get(&vigil_name).cloned();
                        if let Some(ref flag) = run_flag
                            && flag.load(Ordering::SeqCst)
                        {
                            warn!(%vigil_name, "skipping reap — observance in flight");
                            continue;
                        }

                        let trigger = trigger_map
                            .get(&vigil_name)
                            .copied()
                            .unwrap_or(TriggerKind::Toll);

                        // Dispatch on-vigil-reap hook pre-rite.
                        let reap_ctx = format!(
                            "@{{:vigil \"{}\" :event_count {} :trigger :{}}}",
                            vigil_name,
                            events.len(),
                            trigger.as_str()
                        );
                        let _ = hook_tx.try_send(
                            super::types::HookDispatchRequest {
                                hook_name: "on-vigil-reap".into(),
                                context: reap_ctx,
                            },
                        );

                        // Rite gate check — skip observance if the rite fails.
                        let (rite_output, rite_exit_code) =
                            if let Some(Some(rite)) = rite_map.get(&vigil_name) {
                                match evaluate_rite(rite).await {
                                    RiteResult::Pass { output } => (output, None),
                                    RiteResult::Fail { reason } => {
                                        warn!(%vigil_name, %reason, "rite gate failed, skipping observance");
                                        continue;
                                    }
                                }
                            } else {
                                (None, None)
                            };

                        let batch = CoalescedBatch::from_events(
                            vigil_name.clone(),
                            trigger,
                            &events,
                            rite_output,
                            rite_exit_code,
                        );

                        let prompt_template = prompt_map
                            .get(&vigil_name)
                            .map(|s| s.as_str())
                            .unwrap_or("");
                        let prompt = if prompt_template.is_empty() {
                            String::new()
                        } else {
                            build_prompt(prompt_template, &batch)
                        };

                        // Skip if the prompt still has unresolved {placeholders}
                        // — happens when the batch has only toll ticks and the
                        // template expects plugin-emitted context (job, etc.).
                        // Use a regex to match only template-variable patterns like
                        // {job} or {build_number}, not JSON object braces from
                        // substituted {harbinger_data} values.
                        if !prompt.is_empty() {
                            static RE: std::sync::LazyLock<regex::Regex> =
                                std::sync::LazyLock::new(|| {
                                    regex::Regex::new(r"\{[a-zA-Z_][a-zA-Z0-9_]*\}").unwrap()
                                });
                            if RE.is_match(&prompt) {
                                warn!(%vigil_name, "skipping observance — prompt has unresolved placeholders");
                                continue;
                            }
                        }

                        let context = coalesce_events(&events);

                        // Mark in-flight so overlapping reaps for this vigil are skipped.
                        if let Some(ref flag) = run_flag {
                            flag.store(true, Ordering::SeqCst);
                        }

                        let running_flag = run_flag.unwrap_or_else(|| {
                            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
                        });

                        let observance = Observance {
                            vigil_name: vigil_name.clone(),
                            prompt,
                            context,
                            event_count: batch.event_count,
                            running: running_flag,
                        };

                        if observance_tx.try_send(observance).is_err() {
                            warn!(%vigil_name, "observance queue full, dropping");
                        } else if let Some(ref wt) = wake_tx {
                            let _ = wt.send(());
                        }

                        // Procession: inject event into next vigil's queue.
                        if let Some(Some(next_name)) = procession_map.get(&vigil_name) {
                            if let Some(next_tx) = senders.get(next_name) {
                                let chain_event = VigilEvent {
                                    vigil_name: next_name.clone(),
                                    trigger: TriggerKind::Toll,
                                    context: serde_json::json!({
                                        "procession_from": vigil_name,
                                        "event_count": batch.event_count,
                                    }),
                                    timestamp: chrono::Utc::now(),
                                };
                                if next_tx.try_send(chain_event).is_err() {
                                    warn!(%next_name, from=%vigil_name,
                                        "procession queue full for next vigil");
                                } else {
                                    debug!(%next_name, from=%vigil_name,
                                        "procession: injected event into next vigil");
                                }
                            } else {
                                warn!(%next_name, from=%vigil_name,
                                    "procession target not found among active vigils");
                            }
                        }
                    }
                    Err(e) => {
                        warn!("reap task panicked: {e}");
                    }
                }
            }
        }
    }
}

async fn reap_interval(
    name: String,
    interval_secs: u64,
    rx: &mut mpsc::Receiver<VigilEvent>,
) -> (String, Vec<VigilEvent>) {
    let mut events: Vec<VigilEvent> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(interval_secs);

    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(event)) => {
                events.push(event);
                // Drain any additional events without blocking.
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }
            }
            Ok(None) => break, // Channel closed.
            Err(_) => break,   // Timeout — reap window elapsed.
        }
    }

    (name, events)
}

fn coalesce_events(events: &[VigilEvent]) -> serde_json::Value {
    if events.len() == 1 {
        return events[0].context.clone();
    }

    let mut files: Vec<String> = Vec::new();
    let mut payloads: Vec<serde_json::Value> = Vec::new();

    for event in events {
        if let Some(fs) = event.context.get("files").and_then(|v| v.as_array()) {
            for f in fs {
                if let Some(s) = f.as_str() {
                    files.push(s.to_string());
                }
            }
        }
        payloads.push(event.context.clone());
    }

    serde_json::json!({
        "events": payloads,
        "files": files,
        "event_count": events.len(),
    })
}
