//! Small concurrency conveniences.

use std::sync::{Mutex, MutexGuard};

/// Lock a [`Mutex`], recovering the guard if the lock was poisoned.
///
/// dirge never relies on lock poisoning for correctness — a panic while a lock
/// is held shouldn't cascade into every other locker panicking too — so the
/// codebase always recovers the inner guard. This replaces the ~120 repeated,
/// cryptic `.lock().unwrap_or_else(|e| e.into_inner())` call sites with one
/// named intent (`.lock_ignore_poison()`).
pub trait LockExt<T> {
    fn lock_ignore_poison(&self) -> MutexGuard<'_, T>;

    /// Non-blocking lock that ignores poisoning. Returns `None` only when
    /// the lock is currently held by another thread (`WouldBlock`); a
    /// poisoned-but-free mutex still yields its guard, matching
    /// [`LockExt::lock_ignore_poison`]'s never-cascade policy.
    ///
    /// Used on the UI keystroke path (dirge-w11c) so a plugin-bound key
    /// pressed while a plugin tool holds the manager mutex is dropped with
    /// a "busy" line instead of freezing the event loop, mirroring the
    /// loop-top `try_lock` drains (H-R1).
    // The sole production caller is plugin-gated; without `plugin` this is
    // exercised only by the unit tests (a separate cfg(test) build).
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    fn try_lock_ignore_poison(&self) -> Option<MutexGuard<'_, T>>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_ignore_poison(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    fn try_lock_ignore_poison(&self) -> Option<MutexGuard<'_, T>> {
        use std::sync::TryLockError;
        match self.try_lock() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn recovers_guard_after_poison() {
        let m = Arc::new(Mutex::new(7));
        let m2 = m.clone();
        // Poison the lock by panicking while it's held.
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        // A plain .lock() would now return Err; lock_ignore_poison recovers.
        assert_eq!(*m.lock_ignore_poison(), 7);
        *m.lock_ignore_poison() = 9;
        assert_eq!(*m.lock_ignore_poison(), 9);
    }

    #[test]
    fn try_lock_returns_none_when_contended() {
        let m = Mutex::new(1);
        let _held = m.lock_ignore_poison();
        // Same thread, lock already held → WouldBlock → None.
        assert!(m.try_lock_ignore_poison().is_none());
    }

    #[test]
    fn try_lock_recovers_guard_after_poison() {
        let m = Arc::new(Mutex::new(7));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        // Poisoned but uncontended → Some (never cascade the panic).
        let g = m.try_lock_ignore_poison();
        assert!(g.is_some());
        assert_eq!(*g.unwrap(), 7);
    }

    #[test]
    fn try_lock_succeeds_when_free() {
        let m = Mutex::new(3);
        assert_eq!(*m.try_lock_ignore_poison().unwrap(), 3);
    }
}
