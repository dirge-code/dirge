//! Watcher trigger — fires on filesystem change events via the `notify` crate.
//! Debounces rapid-fire events at 500ms.
#![allow(dead_code)]

use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::VecDeque;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::warn;

use super::types::{HookDispatchRequest, TriggerKind, VigilEvent};

/// Spawn a watcher trigger. Pushes coalesced events into the channel.
/// Ring-buffer backpressure via a local `VecDeque`: oldest event is dropped
/// when the ring is full, and pending events are flushed before new ones.
/// Dispatches `on-vigil-event` hook before pushing each batch.
pub fn spawn_watcher(
    vigil_name: String,
    path: PathBuf,
    tx: mpsc::Sender<VigilEvent>,
    hook_tx: mpsc::Sender<HookDispatchRequest>,
) -> Result<tokio::task::JoinHandle<()>, String> {
    let (event_tx, mut event_rx) = mpsc::channel::<Vec<PathBuf>>(64);

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            let kind = event.kind;
            if matches!(
                kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            ) {
                let paths: Vec<PathBuf> = event.paths;
                let _ = event_tx.try_send(paths);
            }
        }
    })
    .map_err(|e| format!("create watcher for {vigil_name}: {e}"))?;

    let watch_path = path.clone();
    watcher
        .watch(&watch_path, RecursiveMode::Recursive)
        .map_err(|e| format!("watch {:?} for {vigil_name}: {e}", watch_path))?;

    Ok(tokio::spawn(async move {
        // Hold `_watcher` alive until this task ends.
        let _watcher = watcher;

        let mut event_paths: Vec<PathBuf> = Vec::new();
        let debounce = std::time::Duration::from_millis(500);
        const LOCAL_RING_SIZE: usize = 256;
        let mut pending: VecDeque<VigilEvent> = VecDeque::with_capacity(LOCAL_RING_SIZE);

        loop {
            match tokio::time::timeout(debounce, event_rx.recv()).await {
                Ok(Some(paths)) => {
                    event_paths.extend(paths);
                    // Drain any additional events that arrived during the debounce window.
                    while let Ok(paths) = event_rx.try_recv() {
                        event_paths.extend(paths);
                    }

                    let event_count = event_paths.len();
                    let event = VigilEvent {
                        vigil_name: vigil_name.clone(),
                        trigger: TriggerKind::Watcher,
                        context: serde_json::json!({
                            "files": std::mem::take(&mut event_paths),
                            "event_count": event_count,
                        }),
                        timestamp: chrono::Utc::now(),
                    };
                    let hook_ctx = format!(
                        "@{{:vigil \"{}\" :trigger :watcher :event_count {}}}",
                        vigil_name, event_count
                    );
                    let _ = hook_tx.try_send(HookDispatchRequest {
                        hook_name: "on-vigil-event".into(),
                        context: hook_ctx,
                    });
                    // Flush pending events before pushing the new batch.
                    while let Some(ev) = pending.pop_front() {
                        if tx.try_send(ev.clone()).is_err() {
                            pending.push_front(ev);
                            break;
                        }
                    }
                    if pending.len() >= LOCAL_RING_SIZE {
                        let _ = pending.pop_front();
                        warn!(
                            vigil = %vigil_name,
                            "watcher local ring full, dropping oldest event"
                        );
                    }
                    if tx.try_send(event.clone()).is_err() {
                        pending.push_back(event);
                    }
                }
                Ok(None) => break, // Channel closed.
                Err(_) => {
                    // Timeout — no events in the debounce window, flush if any.
                    if !event_paths.is_empty() {
                        let event = VigilEvent {
                            vigil_name: vigil_name.clone(),
                            trigger: TriggerKind::Watcher,
                            context: serde_json::json!({
                                "files": std::mem::take(&mut event_paths),
                            }),
                            timestamp: chrono::Utc::now(),
                        };
                        let hook_ctx = format!(
                            "@{{:vigil \"{}\" :trigger :watcher :flush true}}",
                            vigil_name
                        );
                        let _ = hook_tx.try_send(HookDispatchRequest {
                            hook_name: "on-vigil-event".into(),
                            context: hook_ctx,
                        });
                        // Flush pending before timeout-flush event.
                        while let Some(ev) = pending.pop_front() {
                            if tx.try_send(ev.clone()).is_err() {
                                pending.push_front(ev);
                                break;
                            }
                        }
                        if pending.len() >= LOCAL_RING_SIZE {
                            let _ = pending.pop_front();
                            warn!(
                                vigil = %vigil_name,
                                "watcher local ring full (flush), dropping oldest event"
                            );
                        }
                        if tx.try_send(event.clone()).is_err() {
                            pending.push_back(event);
                        }
                    }
                }
            }
        }
    }))
}
