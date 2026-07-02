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

impl FileLock {
    /// Acquire the exclusive lock guarding `target`, blocking until it is
    /// available. Fail-open: returns a no-op guard if the lock can't be taken.
    pub(crate) fn acquire_for(target: &Path) -> Self {
        let file = open_lock_file(&lock_path(target)).filter(|file| flock_exclusive(file, true));
        Self { _file: file }
    }

    /// Non-blocking acquire. `Some` if the lock was taken (or degraded to a
    /// no-op because the lock file couldn't be opened), `None` if another
    /// holder currently owns it. Test-only — production always blocks.
    #[cfg(test)]
    pub(crate) fn try_acquire_for(target: &Path) -> Option<Self> {
        match open_lock_file(&lock_path(target)) {
            Some(file) => flock_exclusive(&file, false).then_some(Self { _file: Some(file) }),
            None => Some(Self { _file: None }),
        }
    }
}

/// `true` if the lock was acquired, `false` if it is contended (non-blocking)
/// or the syscall failed / is unsupported.
#[cfg(unix)]
fn flock_exclusive(file: &File, block: bool) -> bool {
    use std::os::unix::io::AsRawFd;
    let mut op = libc::LOCK_EX;
    if !block {
        op |= libc::LOCK_NB;
    }
    // SAFETY: `file` outlives the call; flock only reads the fd.
    unsafe { libc::flock(file.as_raw_fd(), op) == 0 }
}

#[cfg(not(unix))]
fn flock_exclusive(_file: &File, _block: bool) -> bool {
    // No portable advisory lock; degrade to the prior unsynchronized behavior.
    false
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
