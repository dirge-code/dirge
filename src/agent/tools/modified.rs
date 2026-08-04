#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use indexmap::IndexMap;

/// Monotonic version counter bumped on every `mark_modified` /
/// `clear_modified` call. Lets the info-panel build path skip the
/// O(N) clone-and-strip work when the underlying set hasn't changed
/// — `recent(256)` previously locked + cloned 256 PathBufs on every
/// keystroke during streaming (review #6). Doubles as the epoch
/// source for [`epoch`] / [`since`]: every mark stores the counter
/// value it was made at, so "what changed since I started" is a
/// version comparison, not a set diff.
static VERSION: AtomicU64 = AtomicU64::new(0);

/// Current version. Panel-side code remembers this and re-snapshots
/// only when the value changes.
pub fn version() -> u64 {
    VERSION.load(Ordering::Acquire)
}

/// Snapshot the counter for a later [`since`] query. Capture BEFORE the
/// unit of work whose delta you want: entries marked after the capture
/// carry a strictly greater version (see [`since`]).
pub fn epoch() -> u64 {
    version()
}

/// How many DISTINCT files have been mutated. The progress monitor
/// (dirge-uw2l.3) reads this at turn boundaries: an increase means new
/// ground was broken, while a flat count across turns means the run is
/// re-editing what it already touched. Cheap — a length, not a clone.
pub fn count() -> usize {
    MODIFIED_FILES.lock().map(|s| s.len()).unwrap_or(0)
}

/// Files the agent has written, edited, or patched in this session, in
/// insertion order (most-recently-modified appears last). The info panel
/// reads this to show a short tail of touched paths so the user has a
/// running record of what the agent has been doing.
///
/// `LazyLock` because `IndexMap::new()` is not `const`. The cost is one
/// extra atomic on first access.
///
/// The value is the [`VERSION`] the path was marked at. `IndexMap`
/// preserves insertion order like the old `IndexSet` (re-insert moves
/// the entry to the end) while letting [`since`] answer "marked after
/// this epoch" per entry.
pub static MODIFIED_FILES: LazyLock<Mutex<IndexMap<PathBuf, u64>>> =
    LazyLock::new(|| Mutex::new(IndexMap::new()));

/// Record that `path` was modified by a write/edit/apply_patch tool call.
/// Maximum entries retained in the modified-files set. Older entries
/// fall off when the cap is reached so a long session editing many
/// files doesn't grow this set unboundedly. The panel only renders
/// the last few entries anyway, so trimming older ones is invisible
/// to the user.
const MAX_MODIFIED: usize = 256;

/// Best-effort canonicalize; falls back to the path as given when the file
/// doesn't exist yet or canonicalize fails.
pub fn mark_modified(path: &Path) {
    let canonical = crate::permission::path::canonical_or_self(path);
    let mut set = MODIFIED_FILES.lock_ignore_poison();
    // IndexMap preserves insertion order and dedups; we want the most-recent
    // touch to surface at the end, so re-insert moves the entry. The stored
    // version is the counter AFTER this mark, so it is strictly greater than
    // any epoch captured before the mark — a re-touched path reappears in a
    // `since(epoch)` delta that already reported it once.
    set.shift_remove(&canonical);
    // Cap the set BEFORE inserting so we always have room for the
    // freshest entry. Oldest (front) gets evicted.
    while set.len() >= MAX_MODIFIED {
        set.shift_remove_index(0);
    }
    let version = VERSION.fetch_add(1, Ordering::Release) + 1;
    set.insert(canonical, version);
}

/// Clear the tracked list. Hooked into /clear so the panel resets along
/// with the conversation.
pub fn clear_modified() {
    MODIFIED_FILES.lock_ignore_poison().clear();
    VERSION.fetch_add(1, Ordering::Release);
}

/// Snapshot of the most-recent `n` modified files (newest last). Returns
/// path strings ready for display; entries already canonicalized when
/// possible so the caller can shorten them relative to a working dir.
pub fn recent(n: usize) -> Vec<PathBuf> {
    let set = MODIFIED_FILES.lock_ignore_poison();
    let len = set.len();
    let start = len.saturating_sub(n);
    set.iter().skip(start).map(|(p, _)| p.clone()).collect()
}

/// Files marked AFTER `epoch` was captured — the delta for the unit of
/// work that captured it at its start. A path RE-touched after the
/// capture carries its newest mark's version, so it appears here too:
/// a naive snapshot-and-diff of the set would silently drop it
/// (`IndexSet`/`IndexMap` keep a re-inserted entry at its original
/// position), the false negative dirge-d0e5.1 guards against.
pub fn since(epoch: u64) -> Vec<PathBuf> {
    let set = MODIFIED_FILES.lock_ignore_poison();
    set.iter()
        .filter(|(_, v)| **v > epoch)
        .map(|(p, _)| p.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize tests that share the global `MODIFIED_FILES` set so they
    /// don't observe each other's writes when cargo runs tests in parallel.
    /// The production code path only holds the inner lock for a single
    /// mark/clear, so real-world contention is a non-issue.
    static TEST_GATE: Mutex<()> = Mutex::new(());

    fn with_isolated<R>(f: impl FnOnce() -> R) -> R {
        let _guard = TEST_GATE.lock_ignore_poison();
        clear_modified();
        let r = f();
        clear_modified();
        r
    }

    /// Review #6: the version counter bumps on every mark + clear,
    /// so panel-side caches can detect when their snapshot is stale.
    #[test]
    fn version_bumps_on_mark_and_clear() {
        with_isolated(|| {
            let v0 = version();
            let dir = std::env::temp_dir().join("dirge-modified-test-version");
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("a.txt");
            std::fs::write(&p, "x").unwrap();

            mark_modified(&p);
            let v1 = version();
            assert!(v1 > v0, "mark must bump version: {v0} -> {v1}");

            mark_modified(&p);
            let v2 = version();
            assert!(v2 > v1, "re-mark (re-insert) bumps too: {v1} -> {v2}");

            clear_modified();
            let v3 = version();
            assert!(v3 > v2, "clear must bump version: {v2} -> {v3}");
        });
    }

    #[test]
    fn mark_modified_dedups_by_path() {
        with_isolated(|| {
            // Use unique paths under /tmp so canonicalize succeeds and tests
            // don't collide.
            let dir = std::env::temp_dir().join("dirge-modified-test-dedup");
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("a.txt");
            std::fs::write(&p, "x").unwrap();

            mark_modified(&p);
            mark_modified(&p);
            mark_modified(&p);
            assert_eq!(recent(10).len(), 1);
        });
    }

    #[test]
    fn mark_modified_preserves_recency_order() {
        with_isolated(|| {
            let dir = std::env::temp_dir().join("dirge-modified-test-order");
            std::fs::create_dir_all(&dir).unwrap();
            let a = dir.join("a.txt");
            let b = dir.join("b.txt");
            std::fs::write(&a, "x").unwrap();
            std::fs::write(&b, "x").unwrap();

            mark_modified(&a);
            mark_modified(&b);
            mark_modified(&a); // re-touch a → moves it to the end

            let recent = recent(10);
            assert_eq!(recent.len(), 2);
            // Last entry is the most-recently-touched file.
            assert!(recent.last().unwrap().ends_with("a.txt"));
            assert!(recent.first().unwrap().ends_with("b.txt"));
        });
    }

    #[test]
    fn recent_caps_at_requested_length() {
        with_isolated(|| {
            let dir = std::env::temp_dir().join("dirge-modified-test-cap");
            std::fs::create_dir_all(&dir).unwrap();
            for i in 0..5 {
                let p = dir.join(format!("f{}.txt", i));
                std::fs::write(&p, "x").unwrap();
                mark_modified(&p);
            }
            assert_eq!(recent(3).len(), 3);
            assert_eq!(recent(10).len(), 5);
            assert_eq!(recent(0).len(), 0);
        });
    }

    #[test]
    fn clear_modified_empties_the_set() {
        with_isolated(|| {
            let dir = std::env::temp_dir().join("dirge-modified-test-clear");
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("a.txt");
            std::fs::write(&p, "x").unwrap();
            mark_modified(&p);
            assert_eq!(recent(10).len(), 1);
            clear_modified();
            assert_eq!(recent(10).len(), 0);
        });
    }

    /// dirge-d0e5.1: `since(epoch)` is the per-unit-of-work delta. A
    /// delegation's child process rehydrates the session's CUMULATIVE
    /// registry (session/rehydrate.rs replays `mark_modified`) before its
    /// run starts; capturing the epoch at run start must exclude every
    /// replayed file — a delegation that changes nothing reports nothing,
    /// even when earlier delegations in the same session changed files.
    #[test]
    fn since_scopes_delta_to_marks_after_epoch() {
        with_isolated(|| {
            let dir = std::env::temp_dir().join("dirge-modified-test-since");
            std::fs::create_dir_all(&dir).unwrap();
            let a = dir.join("a.txt");
            let b = dir.join("b.txt");
            std::fs::write(&a, "x").unwrap();
            std::fs::write(&b, "x").unwrap();

            // Prior delegations' files replayed into the registry.
            mark_modified(&a);
            mark_modified(&b);
            let epoch = epoch();

            // This delegation changes NOTHING → empty delta.
            assert!(
                since(epoch).is_empty(),
                "a delegation that changes nothing must report no files, got {:?}",
                since(epoch)
            );

            // A file touched after the capture appears in the delta.
            mark_modified(&a);
            let delta = since(epoch);
            assert!(
                delta.iter().any(|p| p.ends_with("a.txt")),
                "touched-after-capture file must appear: {delta:?}"
            );
            assert!(
                !delta.iter().any(|p| p.ends_with("b.txt")),
                "untouched-after-capture file must not appear: {delta:?}"
            );
        });
    }

    /// dirge-d0e5.1: a file touched in delegation 1 and touched AGAIN in
    /// delegation 2 appears in BOTH deltas. The naive fix — snapshot the
    /// set, diff afterwards — fails this: a re-inserted path keeps its
    /// original position, so it silently vanishes from the second diff.
    /// The per-entry version makes the second touch a new, higher version.
    #[test]
    fn since_reports_a_path_touched_again_in_a_later_delta() {
        with_isolated(|| {
            let dir = std::env::temp_dir().join("dirge-modified-test-since-remark");
            std::fs::create_dir_all(&dir).unwrap();
            let a = dir.join("a.txt");
            std::fs::write(&a, "x").unwrap();

            // Delegation 1 touches a.
            let e1 = epoch();
            mark_modified(&a);
            assert!(
                since(e1).iter().any(|p| p.ends_with("a.txt")),
                "delegation 1 must report the file it touched"
            );

            // Delegation 2 touches the SAME file again.
            let e2 = epoch();
            mark_modified(&a);
            assert!(
                since(e2).iter().any(|p| p.ends_with("a.txt")),
                "delegation 2 must report the file it re-touched, got {:?}",
                since(e2)
            );
        });
    }
}
