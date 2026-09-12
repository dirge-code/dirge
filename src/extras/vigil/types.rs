//! Core types for the vigil heartbeat/wakeup runtime.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::config::{VigilCommand, VigilRite};

/// An event pushed into a vigil's queue by a trigger (toll, watcher, harbinger).
#[derive(Debug, Clone)]
pub struct VigilEvent {
    pub vigil_name: String,
    pub trigger: TriggerKind,
    /// Trigger-specific context data (file paths, socket payload, etc.).
    pub context: serde_json::Value,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    Toll,
    Watcher,
    Harbinger,
}

impl TriggerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TriggerKind::Toll => "toll",
            TriggerKind::Watcher => "watcher",
            TriggerKind::Harbinger => "harbinger",
        }
    }
}

/// Per-vigil channel pair. `tx` (sender) is `Clone` and shared with triggers.
/// `rx` (receiver) is consumed by the reaper for that vigil.
pub fn make_vigil_channel(bound: usize) -> (mpsc::Sender<VigilEvent>, mpsc::Receiver<VigilEvent>) {
    mpsc::channel(bound)
}

/// Runtime state for one active vigil. `tx` is shared with triggers;
/// `rx` is taken by the reaper at startup.
pub struct VigilInstance {
    pub name: String,
    pub reap_interval_secs: u64,
    pub prompt: String,
    pub procession: Option<String>,
    pub tx: mpsc::Sender<VigilEvent>,
    pub running: Arc<std::sync::atomic::AtomicBool>,
}

/// Bundle passed to the reaper: the input channel, rite config, prompt
/// template, and the "observance in flight" flag.
pub struct VigilReapInput {
    pub name: String,
    pub trigger: TriggerKind,
    pub reap_interval_secs: u64,
    pub rx: mpsc::Receiver<VigilEvent>,
    pub running: Arc<std::sync::atomic::AtomicBool>,
    pub rite: Option<VigilRite>,
    pub prompt: String,
    pub procession: Option<String>,
}

/// Snapshot of a single vigil's runtime state, returned by StatusReq queries.
#[derive(Debug, Clone)]
pub struct VigilStatusInfo {
    pub name: String,
    pub trigger: TriggerKind,
    pub reap_interval_secs: u64,
    pub running: bool,
    pub paused: bool,
    /// Number of events collected in the most recent reap window.
    pub last_event_count: usize,
    /// ISO 8601 timestamp of the most recent event reap.
    pub last_event_at: Option<String>,
}

/// Request to dispatch a plugin hook from a background task (trigger producers
/// or reaper). The UI loop drains the hook channel and dispatches via PluginManager.
#[derive(Debug, Clone)]
pub struct HookDispatchRequest {
    pub hook_name: String,
    pub context: String,
}

/// Control messages for the vigil-keeper / reaper.
#[derive(Debug)]
pub enum VigilCtl {
    Shutdown,
    Pause {
        name: String,
    },
    PauseAll,
    Resume {
        name: String,
    },
    ResumeAll,
    /// Query: respond with a snapshot of all vigil states.
    StatusReq {
        respond_to: tokio::sync::oneshot::Sender<Vec<VigilStatusInfo>>,
    },
}

/// Result of a rite gate check.
#[derive(Debug)]
pub enum RiteResult {
    Pass {
        output: Option<String>,
        /// Exit code of the rite command. `None` when the rite had no `cmd`
        /// (e.g. a `git_dirty`-only gate).
        exit_code: Option<i32>,
    },
    Fail {
        reason: String,
    },
}

/// Runtime representation of a vigil definition (deserialized from config
/// and/or filesystem).
#[derive(Debug, Clone)]
pub struct VigilConfig {
    pub name: String,
    pub trigger: TriggerKind,
    pub reap_interval_secs: u64,
    pub rite: Option<VigilRite>,
    pub prompt: String,
    pub procession: Option<String>,
}

/// Per-trigger payload variant carried in a VigilEvent's context.
#[derive(Debug, Clone)]
pub enum VigilPayload {
    Toll {
        interval_secs: u64,
    },
    Watcher {
        file: String,
        event: String,
    },
    Harbinger {
        data: String,
        commands: HashMap<String, VigilCommand>,
    },
}

/// Output of coalescing multiple VigilEvents into one batch.
#[derive(Debug, Clone)]
pub struct CoalescedBatch {
    pub vigil_name: String,
    pub trigger: TriggerKind,
    pub files: Vec<String>,
    pub events: Vec<serde_json::Value>,
    pub event_count: usize,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub harbinger_data: Option<String>,
    pub rite_output: Option<String>,
    pub rite_exit_code: Option<i32>,
}

impl CoalescedBatch {
    pub fn from_events(
        vigil_name: String,
        trigger: TriggerKind,
        events: &[VigilEvent],
        rite_output: Option<String>,
        rite_exit_code: Option<i32>,
    ) -> Self {
        let mut files: Vec<String> = Vec::new();
        let mut payloads: Vec<serde_json::Value> = Vec::new();
        let mut harbinger_data: Option<String> = None;

        for event in events {
            if let Some(fs) = event.context.get("files").and_then(|v| v.as_array()) {
                for f in fs {
                    if let Some(s) = f.as_str()
                        && !files.contains(&s.to_string())
                    {
                        files.push(s.to_string());
                    }
                }
            }
            if harbinger_data.is_none()
                && let Some(hd) = event.context.get("harbinger_data").and_then(|v| v.as_str())
            {
                harbinger_data = Some(hd.to_string());
            }
            payloads.push(event.context.clone());
        }

        Self {
            vigil_name,
            trigger,
            files,
            events: payloads,
            event_count: events.len(),
            timestamp: chrono::Utc::now(),
            harbinger_data,
            rite_output,
            rite_exit_code,
        }
    }
}

/// Runtime tracking for vigil mode — held by the keeper and surfaced to the
/// post-turn dispatch so `decide_post_done_action` knows whether a vigil
/// observance just completed.
#[derive(Debug, Clone)]
pub struct VigilRunState {
    pub active: bool,
    pub current_vigil: Option<String>,
    pub ctl_tx: Option<mpsc::Sender<VigilCtl>>,
}
