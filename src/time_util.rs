//! Current-time-since-epoch helpers.
//!
//! Many sites need "seconds / millis / nanos since the Unix epoch" for
//! timestamps, cache keys, and unique-id seeds, and hand-rolled
//! `SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_*()).unwrap_or(0)`
//! ~30 times with subtle variation. These centralize it; the `.unwrap_or(0)`
//! saturates the only failure case (a system clock set before 1970), which
//! never happens in practice.

use std::time::{SystemTime, UNIX_EPOCH};

/// Whole seconds since the Unix epoch (0 if the clock predates the epoch).
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Nanoseconds since the Unix epoch — a monotonic-ish unique seed for
/// ids / temp filenames.
pub fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
