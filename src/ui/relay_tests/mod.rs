//! PTY relay integration tests.
//!
//! Shared helpers live in `common.rs`; individual test files
//! each cover a focused scenario (2–3 tests per file).
//!
//! All tests require `sandbox-microvm` feature + unix.

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod common;

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod basic_typing;

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod sustained_overlap;

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod slow_consumer_boundary;

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod drain_roundtrip;

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod channel_rate;

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod channel_burst;

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod crossterm_suite;

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod e2e_attach;

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod ssh;

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod reader_shutdown;

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod sentinel_restore;

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod poll_latency;
