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
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_ignore_poison(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
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
}
