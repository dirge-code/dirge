//! Toll trigger — fires on a fixed timer interval.
#![allow(dead_code)]

use std::collections::VecDeque;
use tokio::sync::mpsc;
use tracing::warn;

use super::types::{HookDispatchRequest, TriggerKind, VigilEvent};

/// Number of events to buffer locally before dropping the oldest.
const LOCAL_RING_SIZE: usize = 256;

/// Spawn a toll (timer) trigger. Pushes a `VigilEvent` into the channel at
/// every `interval_secs` boundary. Ring-buffer backpressure: when the channel
/// is full, the oldest event in the local buffer is dropped and retried.
/// Dispatches `on-vigil-event` hook before pushing each event.
pub fn spawn_toll(
    vigil_name: String,
    interval_secs: u64,
    tx: mpsc::Sender<VigilEvent>,
    hook_tx: mpsc::Sender<HookDispatchRequest>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        let mut pending: VecDeque<VigilEvent> = VecDeque::with_capacity(LOCAL_RING_SIZE);
        // Skip the immediate first tick — first fire after interval_secs.
        interval.tick().await;
        loop {
            interval.tick().await;
            let event = VigilEvent {
                vigil_name: vigil_name.clone(),
                trigger: TriggerKind::Toll,
                context: serde_json::json!({"kind": "toll", "interval_secs": interval_secs}),
                timestamp: chrono::Utc::now(),
            };
            let hook_ctx = format!(
                "@{{:vigil \"{}\" :trigger :toll :interval_secs {}}}",
                vigil_name, interval_secs
            );
            let _ = hook_tx.try_send(HookDispatchRequest {
                hook_name: "on-vigil-event".into(),
                context: hook_ctx,
            });
            // Flush pending events before pushing the new one.
            while let Some(ev) = pending.pop_front() {
                if tx.try_send(ev.clone()).is_err() {
                    pending.push_front(ev);
                    break;
                }
            }
            // Push new event; pop oldest if ring is full.
            if pending.len() >= LOCAL_RING_SIZE {
                let _ = pending.pop_front();
                warn!(
                    vigil = %vigil_name,
                    "toll local ring full, dropping oldest event"
                );
            }
            if tx.try_send(event.clone()).is_err() {
                pending.push_back(event);
            }
        }
    })
}
