//! System CPU / MEM stats for the [SYSTEM LOAD] sub-panel in the
//! right-side UI panel (ui-redesign branch).
//!
//! Polled periodically by a background task; results land in a
//! shared `SysLoadSnapshot` that the renderer reads at paint time.
//! Cheap — `sysinfo::System::refresh_*` calls take <1ms on Apple
//! Silicon / a modern Linux box; we poll once every 2 seconds.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

/// Cached snapshot of system load. Updated in place by
/// [`SysLoadPoller`]; read by the renderer's panel paint code.
#[derive(Debug, Clone, Default)]
pub struct SysLoadSnapshot {
    /// CPU utilization across all cores, 0.0..=100.0.
    pub cpu_pct: f32,
    /// Memory usage as a percent of total, 0.0..=100.0.
    pub mem_pct: f32,
}

/// Shared, lock-protected snapshot. Cheap to clone (an Arc bump).
#[derive(Debug, Clone, Default)]
pub struct SharedSysLoad(Arc<Mutex<SysLoadSnapshot>>);

impl SharedSysLoad {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the current snapshot. Returns a copy so the lock isn't
    /// held across the paint path.
    pub fn snapshot(&self) -> SysLoadSnapshot {
        self.0.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn store(&self, snap: SysLoadSnapshot) {
        if let Ok(mut g) = self.0.lock() {
            *g = snap;
        }
    }
}

/// Spawn a background polling task on the current tokio runtime.
/// Returns the shared snapshot handle; the task runs forever and
/// dies when its tokio runtime shuts down. Polling cadence is
/// `interval`; values <500ms get clamped to 500ms so we don't burn
/// CPU on the panel paint.
pub fn spawn_poller(interval: Duration) -> SharedSysLoad {
    let shared = SharedSysLoad::new();
    let shared_for_task = shared.clone();
    let cadence = interval.max(Duration::from_millis(500));

    tokio::spawn(async move {
        // `sysinfo` requires a back-to-back refresh-then-read for
        // CPU — the first sample is always 0 because cpu_usage is
        // computed across the window between two refresh_cpu_all
        // calls. Prime with one refresh + sleep before the loop.
        let mut sys = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
                .with_memory(MemoryRefreshKind::nothing().with_ram()),
        );
        sys.refresh_cpu_all();
        sys.refresh_memory();
        tokio::time::sleep(Duration::from_millis(200)).await;

        loop {
            sys.refresh_cpu_all();
            sys.refresh_memory();

            // CPU: average of all logical cores' usage. `cpu_usage()`
            // is a percentage per core; mean approximates overall
            // utilization for the panel readout. Could swap for
            // global_cpu_info().cpu_usage() but the per-core mean
            // matches what `top` shows.
            let cpus = sys.cpus();
            let cpu_pct = if cpus.is_empty() {
                0.0
            } else {
                let sum: f32 = cpus.iter().map(|c| c.cpu_usage()).sum();
                sum / cpus.len() as f32
            };

            let total_mem = sys.total_memory().max(1);
            let used_mem = sys.used_memory();
            let mem_pct = (used_mem as f32 / total_mem as f32) * 100.0;

            shared_for_task.store(SysLoadSnapshot { cpu_pct, mem_pct });
            tokio::time::sleep(cadence).await;
        }
    });
    shared
}
