//! Shared scheduler state + run gates for the background curators
//! (dirge-rwrg).
//!
//! The skills curator and memory curator each carried a copy-pasted
//! state struct, load/save, and interval gate — and the copies had
//! diverged: skills kept the old "seed the clock and defer one full
//! interval" first-run protocol while the memory curator had moved to
//! a session-count gate (dirge-jyks). One clock now owns the policy
//! for both:
//!
//! - FIRST run: gated on accumulated sessions
//!   ([`DEFAULT_MIN_SESSIONS_FIRST_RUN`]), not the calendar. Memory
//!   and skill churn is highest on a young project; deferring a full
//!   interval after install delayed the first consolidation for no
//!   reason. No deadlock (dirge-6js7's failure mode): this branch
//!   returns `true` as soon as sessions accumulate, and the caller's
//!   pass seeds `last_run` via [`CuratorClock::mark_ran`].
//! - AFTER: a fixed per-curator interval between runs.
//!
//! On-disk format is unchanged: the same `{last_run, first_check}`
//! JSON the legacy structs wrote. Older files carrying a dropped
//! `last_scanned_watermark` field (from the removed cross-session
//! extractor) still load — serde ignores unknown fields.

use std::path::{Path, PathBuf};

use crate::extras::dirge_paths::ProjectPaths;

/// Sessions a young project must accumulate before a curator's first
/// run fires (dirge-jyks).
pub(crate) const DEFAULT_MIN_SESSIONS_FIRST_RUN: i64 = 10;

/// Persistent scheduler state. Field names match the three legacy
/// structs so existing state files load unchanged.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ClockState {
    /// Unix timestamp (seconds) of the last run. `None` = never run;
    /// different from epoch-0 which is a valid timestamp.
    pub last_run: Option<u64>,
    /// Timestamp when the state was first seeded.
    pub first_check: u64,
}

/// One curator's scheduler clock: state file + run gates.
pub(crate) struct CuratorClock {
    state_path: PathBuf,
    state: ClockState,
    interval_secs: u64,
    min_sessions_first_run: i64,
    session_db_path: PathBuf,
}

impl CuratorClock {
    /// Load (or initialize) the clock backed by `state_path`.
    pub fn new(
        paths: &ProjectPaths,
        state_path: PathBuf,
        interval_hours: u64,
        min_sessions_first_run: i64,
    ) -> Result<Self, String> {
        let state = Self::load_state(&state_path)?;
        Ok(Self {
            state_path,
            state,
            interval_secs: interval_hours * 3600,
            min_sessions_first_run,
            session_db_path: paths.session_db_path(),
        })
    }

    fn load_state(path: &Path) -> Result<ClockState, String> {
        if !path.exists() {
            return Ok(ClockState {
                last_run: None,
                first_check: now_secs(),
            });
        }
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("read curator state: {e}"))?;
        serde_json::from_str(&content).map_err(|e| format!("parse curator state: {e}"))
    }

    /// Persist the current state atomically.
    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create state dir: {e}"))?;
        }
        let content = serde_json::to_string_pretty(&self.state)
            .map_err(|e| format!("serialize state: {e}"))?;
        crate::fs_atomic::atomic_write_sync(&self.state_path, content.as_bytes())
            .map_err(|e| format!("write state: {e}"))
    }

    /// Should this curator run now? Before the first run the gate is
    /// SESSION COUNT; afterwards it's the interval.
    pub fn should_run_now(&self) -> bool {
        match self.state.last_run {
            None => self.session_count() >= self.min_sessions_first_run,
            Some(last) => now_secs().saturating_sub(last) >= self.interval_secs,
        }
    }

    /// Record that a run happened now and persist.
    pub fn mark_ran(&mut self) -> Result<(), String> {
        self.state.last_run = Some(now_secs());
        self.save()
    }

    /// Last recorded run, if any (tests + diagnostics).
    #[allow(dead_code)]
    pub fn last_run(&self) -> Option<u64> {
        self.state.last_run
    }

    /// Best-effort session count from the project DB. 0 when the DB
    /// can't be opened (no sessions yet → first run stays deferred).
    fn session_count(&self) -> i64 {
        let Ok(db) = crate::extras::session_db::SessionDb::open(&self.session_db_path) else {
            return 0;
        };
        db.conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap_or(0)
    }
}

fn now_secs() -> u64 {
    crate::time_util::now_unix_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project() -> (ProjectPaths, std::path::PathBuf) {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("dirge-clock-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = ProjectPaths::at(&dir);
        (paths, dir)
    }

    fn seed_sessions(paths: &ProjectPaths, n: usize) {
        std::fs::create_dir_all(paths.sessions_dir()).unwrap();
        let db = crate::extras::session_db::SessionDb::open(&paths.session_db_path()).unwrap();
        for i in 0..n {
            db.insert_session(
                &format!("sess-{i}"),
                "cli",
                "gpt-5",
                "openai",
                "2026-05-01T10:00:00Z",
            )
            .unwrap();
        }
    }

    fn clock(paths: &ProjectPaths) -> CuratorClock {
        CuratorClock::new(
            paths,
            paths.dirge_dir().join(".test_clock_state"),
            168,
            DEFAULT_MIN_SESSIONS_FIRST_RUN,
        )
        .unwrap()
    }

    /// First run gates on session count, not the calendar.
    #[test]
    fn first_run_gates_on_session_count() {
        let (paths, _dir) = temp_project();
        seed_sessions(&paths, 2);
        let c = clock(&paths);
        assert!(!c.should_run_now(), "2 sessions — deferred");

        seed_sessions(&paths, 10); // 12 total
        let c = clock(&paths);
        assert!(c.should_run_now(), "enough sessions — fires immediately");
    }

    /// After the first run, the interval gate applies.
    #[test]
    fn interval_gate_after_first_run() {
        let (paths, _dir) = temp_project();
        seed_sessions(&paths, 12);
        let mut c = clock(&paths);
        assert!(c.should_run_now());
        c.mark_ran().unwrap();
        assert!(!c.should_run_now(), "just ran — interval gate holds");

        // Backdate last_run past the interval; reload from disk.
        c.state.last_run = Some(now_secs() - 169 * 3600);
        c.save().unwrap();
        let c = clock(&paths);
        assert!(c.should_run_now(), "interval elapsed — runs again");
    }

    /// State files written by the legacy structs (`{last_run,
    /// first_check}`) load unchanged. Old files carrying the dropped
    /// `last_scanned_watermark` field still deserialize (serde ignores
    /// unknown fields).
    #[test]
    fn legacy_state_files_load() {
        let (paths, _dir) = temp_project();
        let path = paths.dirge_dir().join(".test_clock_state");
        std::fs::create_dir_all(paths.dirge_dir()).unwrap();
        std::fs::write(
            &path,
            r#"{"last_run": 1234567890, "first_check": 1234567800, "last_scanned_watermark": "2026-05-03T10:00:00Z"}"#,
        )
        .unwrap();

        let c = CuratorClock::new(&paths, path.clone(), 168, 10).unwrap();
        assert_eq!(c.last_run(), Some(1234567890));

        // Re-saving drops the obsolete field.
        c.save().unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("last_scanned_watermark"),
            "obsolete field must not be re-written: {raw}"
        );
    }
}
