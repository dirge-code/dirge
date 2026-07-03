//! Advisory cross-process file lock used to serialize an OAuth
//! load → refresh → save so two Dirge processes don't both spend the same
//! (single-use, rotated-on-refresh) refresh token and clobber each other's
//! result. See dirge-m1o5.
//!
//! Best-effort by design: locking is a correctness optimization, not a
//! safety invariant, so acquisition never hard-fails. If the lock file can't
//! be opened, or the platform lacks advisory locking, callers get a no-op
//! guard and fall back to the previous unsynchronized behavior rather than
//! breaking auth.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a contended acquire waits before proceeding without the lock.
/// Bounds the cross-process wedge when the current holder is stuck (e.g. a
/// hung refresh POST): losing serialization is recoverable, a deadlocked
/// process is not.
const ACQUIRE_DEADLINE: Duration = Duration::from_secs(60);

/// Poll interval for the non-blocking retry loop in `acquire_for`.
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Exclusive advisory lock held for the lifetime of the guard. Released when
/// dropped (implicitly, when the underlying file is closed).
pub(crate) struct FileLock {
    // `None` == degraded no-op guard (open failed / unsupported platform).
    _file: Option<File>,
}

/// The lock file guarding `target` — e.g. `auth.json` → `auth.json.lock`. A
/// sidecar (rather than locking the credential file itself) keeps the lock
/// independent of the atomic-rename the save performs.
fn lock_path(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

fn open_lock_file(path: &Path) -> Option<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .ok()
}

/// Outcome of a single non-blocking lock attempt. On platforms without
/// advisory locking (`cfg(not(unix))`) `try_flock_exclusive` only ever
/// returns `Unsupported`, so the other variants go unconstructed there.
#[cfg_attr(not(unix), allow(dead_code))]
enum TryLock {
    Acquired,
    /// Another holder owns the lock; retrying may succeed.
    Contended,
    /// Syscall failed or the platform lacks advisory locking; retrying is
    /// pointless — fail open immediately.
    Unsupported,
}

impl FileLock {
    /// Acquire the exclusive lock guarding `target`, waiting up to
    /// [`ACQUIRE_DEADLINE`] for the current holder to release it. Fail-open:
    /// returns a no-op guard if the lock can't be taken (open failure,
    /// unsupported platform, or a holder stuck past the deadline).
    pub(crate) fn acquire_for(target: &Path) -> Self {
        Self::acquire_for_with_deadline(target, ACQUIRE_DEADLINE)
    }

    fn acquire_for_with_deadline(target: &Path, deadline: Duration) -> Self {
        let Some(file) = open_lock_file(&lock_path(target)) else {
            return Self { _file: None };
        };
        let start = Instant::now();
        loop {
            match try_flock_exclusive(&file) {
                TryLock::Acquired => return Self { _file: Some(file) },
                TryLock::Unsupported => return Self { _file: None },
                TryLock::Contended => {}
            }
            let elapsed = start.elapsed();
            if elapsed >= deadline {
                return Self { _file: None };
            }
            std::thread::sleep(RETRY_INTERVAL.min(deadline - elapsed));
        }
    }

    /// Non-blocking acquire. `Some` if the lock was taken (or degraded to a
    /// no-op because the lock file couldn't be opened / locking is
    /// unsupported), `None` if another holder currently owns it. Test-only —
    /// production waits via `acquire_for`.
    #[cfg(test)]
    pub(crate) fn try_acquire_for(target: &Path) -> Option<Self> {
        match open_lock_file(&lock_path(target)) {
            Some(file) => match try_flock_exclusive(&file) {
                TryLock::Acquired => Some(Self { _file: Some(file) }),
                TryLock::Contended => None,
                TryLock::Unsupported => Some(Self { _file: None }),
            },
            None => Some(Self { _file: None }),
        }
    }
}

#[cfg(unix)]
fn try_flock_exclusive(file: &File) -> TryLock {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `file` outlives the call; flock only reads the fd.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return TryLock::Acquired;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EINTR => TryLock::Contended,
        _ => TryLock::Unsupported,
    }
}

#[cfg(not(unix))]
fn try_flock_exclusive(_file: &File) -> TryLock {
    // No portable advisory lock; degrade to the prior unsynchronized behavior.
    TryLock::Unsupported
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct TempTarget(PathBuf);

    impl TempTarget {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "dirge_file_lock_{tag}_{}_{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir.join("auth.json"))
        }

        fn target(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTarget {
        fn drop(&mut self) {
            if let Some(parent) = self.0.parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        }
    }

    #[test]
    fn second_acquire_is_contended_while_first_is_held() {
        let t = TempTarget::new("contended");
        let held = FileLock::try_acquire_for(t.target()).expect("first acquire should succeed");
        assert!(
            FileLock::try_acquire_for(t.target()).is_none(),
            "a second acquire must observe the lock as held"
        );
        drop(held);
        assert!(
            FileLock::try_acquire_for(t.target()).is_some(),
            "the lock must be re-acquirable after the holder is dropped"
        );
    }

    #[test]
    fn bounded_acquire_returns_degraded_guard_when_deadline_expires() {
        use std::time::{Duration, Instant};

        let t = TempTarget::new("deadline");
        let held = FileLock::try_acquire_for(t.target()).expect("first acquire should succeed");

        let start = Instant::now();
        let guard = FileLock::acquire_for_with_deadline(t.target(), Duration::from_millis(200));
        let elapsed = start.elapsed();

        assert!(
            guard._file.is_none(),
            "a contended acquire past the deadline must degrade to a no-op guard"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "bounded acquire must return promptly after the deadline, took {elapsed:?}"
        );
        drop(held);
    }

    #[test]
    fn blocking_acquire_serializes_a_read_modify_write() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let t = TempTarget::new("serialize");
        let target = t.target().to_path_buf();
        // Shared counter standing in for the on-disk credential. Each thread
        // does read → (yield) → write+1 under the lock; without mutual
        // exclusion the delayed reads interleave and an update is lost.
        let counter = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let target = target.clone();
                let counter = counter.clone();
                std::thread::spawn(move || {
                    let _lock = FileLock::acquire_for(&target);
                    let seen = counter.load(Ordering::SeqCst);
                    std::thread::yield_now();
                    counter.store(seen + 1, Ordering::SeqCst);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            counter.load(Ordering::SeqCst),
            8,
            "every locked increment must land"
        );
    }
}
