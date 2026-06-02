#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use indexmap::IndexSet;

/// Monotonic version counter bumped on every `mark_modified` /
/// `clear_modified` call. Lets the info-panel build path skip the
/// O(N) clone-and-strip work when the underlying set hasn't changed
/// — `recent(256)` previously locked + cloned 256 PathBufs on every
/// keystroke during streaming (review #6).
static VERSION: AtomicU64 = AtomicU64::new(0);

/// Current version. Panel-side code remembers this and re-snapshots
/// only when the value changes.
pub fn version() -> u64 {
    VERSION.load(Ordering::Acquire)
}

/// Files the agent has written, edited, or patched in this session, in
/// insertion order (most-recently-modified appears last). The info panel
/// reads this to show a short tail of touched paths so the user has a
/// running record of what the agent has been doing.
///
/// `LazyLock` because `IndexSet::new()` is not `const`. The cost is one
/// extra atomic on first access.
pub static MODIFIED_FILES: LazyLock<Mutex<IndexSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(IndexSet::new()));

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
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut set = MODIFIED_FILES.lock_ignore_poison();
    // IndexSet preserves insertion order and dedups; we want the most-recent
    // touch to surface at the end, so re-insert moves the entry.
    set.shift_remove(&canonical);
    // Cap the set BEFORE inserting so we always have room for the
    // freshest entry. Oldest (front) gets evicted.
    while set.len() >= MAX_MODIFIED {
        set.shift_remove_index(0);
    }
    set.insert(canonical);
    VERSION.fetch_add(1, Ordering::Release);
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
    set.iter().skip(start).cloned().collect()
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
}
