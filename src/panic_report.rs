//! What a panic leaves behind for whoever survives it.
//!
//! A panic hook runs at the panic point, before any unwinding, and it
//! cannot know whether the process is about to die: `panic = "abort"`
//! is not set, `tokio` catches task panics, and several boundaries in
//! this codebase (`catch_unwind` around the plugin FFI, the injection
//! scanner, `spawn_blocking` join errors) survive one deliberately. A
//! hook that *acts* on a panic therefore has to guess, and under
//! `#[tokio::main(flavor = "current_thread")]` the guess dirge used to
//! make — "the panicking thread owns the terminal, so this is fatal" —
//! is wrong for every agent-task panic, because that task runs on the
//! UI thread.
//!
//! So the hook stops guessing and only writes down what happened. Two
//! parties read it afterwards, and they are exactly the parties that
//! know something the hook could not:
//!
//! - Whoever caught the panic and carried on ([`take`]) — the agent
//!   run's epitaph does this, and reports the crash in band as the
//!   run's error. Taking the record *claims* it.
//! - `TerminalGuard::drop`, which runs when the process really is
//!   tearing down. An unclaimed record means nobody survived it, so
//!   it prints the notice after the terminal has been restored.
//!
//! Only the most recent panic is kept. A run of them is a cascade, and
//! the first one is rarely the interesting one for the person reading
//! the screen — the full set is in the log either way, because the hook
//! still chains to the default hook and its backtrace.

use std::sync::Mutex;

use crate::sync_util::LockExt;

/// One panic, as much of it as a hook can see without a backtrace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanicRecord {
    /// The panic payload, when it was a string (the usual case:
    /// `panic!`, `unwrap`, `expect`, index-out-of-bounds).
    pub message: String,
    /// `file:line:col` of the panic site, when the runtime reported one.
    pub location: Option<String>,
}

impl PanicRecord {
    /// One line naming the panic and where it happened. Used both for
    /// the in-band error the UI shows and for the on-exit notice.
    pub fn describe(&self) -> String {
        match &self.location {
            Some(at) => format!("{} (at {at})", self.message),
            None => self.message.clone(),
        }
    }
}

/// Storage for the most recent unclaimed panic.
///
/// A type rather than a bare `static` so tests get their own slot —
/// a process-global one is shared with every other test in the binary
/// and with any panic they provoke on purpose.
pub struct PanicSlot(Mutex<Option<PanicRecord>>);

impl PanicSlot {
    pub const fn new() -> Self {
        PanicSlot(Mutex::new(None))
    }

    /// Write a panic down, replacing any unclaimed earlier one.
    pub fn record(&self, message: String, location: Option<String>) {
        *self.0.lock_ignore_poison() = Some(PanicRecord { message, location });
    }

    /// Claim the record. Returns `None` when there is nothing to claim
    /// or someone already took it — claiming is what stops the same
    /// panic being reported twice by two different survivors.
    pub fn take(&self) -> Option<PanicRecord> {
        self.0.lock_ignore_poison().take()
    }
}

impl Default for PanicSlot {
    fn default() -> Self {
        Self::new()
    }
}

static LAST_PANIC: PanicSlot = PanicSlot::new();

/// Set once the recording hook is chained on, so repeated calls (the
/// TUI guard, tests, embedded use) don't stack duplicates.
static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Chain a hook that records every panic and then delegates to
/// whatever hook was already installed (whose backtrace goes to the
/// log). Idempotent; called from `main` before anything can panic, so
/// headless runs get the same record the TUI does.
///
/// The hook deliberately does nothing else. It runs at the panic point,
/// before any unwinding, on every thread, for panics that kill the
/// process and panics that are about to be caught and survived — and it
/// has no way to tell those apart. Acting on that would be a guess; see
/// the module docs.
pub fn install() {
    if INSTALLED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_from(info);
        previous(info);
    }));
}

/// Extract the payload and location a hook was handed, and record them.
fn record_from(info: &std::panic::PanicHookInfo<'_>) {
    let message = payload_text(info.payload());
    let location = info.location().map(|l| l.to_string());
    LAST_PANIC.record(message, location);
}

/// Claim the most recent unreported panic. See [`PanicSlot::take`].
pub fn take() -> Option<PanicRecord> {
    LAST_PANIC.take()
}

/// The payload as a string when it is one. `panic!("…")`, `unwrap`,
/// `expect` and the built-in panics all produce `&str` or `String`;
/// anything else is a `Box<dyn Any>` we cannot read.
fn payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "Box<dyn Any>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_names_the_panic_and_where_it_happened() {
        let slot = PanicSlot::new();
        slot.record(
            "index out of bounds: the len is 0 but the index is 3".to_string(),
            Some("src/agent/tools/read.rs:120:9".to_string()),
        );
        let got = slot.take().expect("recorded");
        assert_eq!(
            got.describe(),
            "index out of bounds: the len is 0 but the index is 3 (at src/agent/tools/read.rs:120:9)"
        );
    }

    #[test]
    fn a_record_without_a_location_still_reads() {
        let slot = PanicSlot::new();
        slot.record("boom".to_string(), None);
        assert_eq!(slot.take().unwrap().describe(), "boom");
    }

    /// Claiming is the whole point: the run's epitaph and the terminal
    /// teardown both look here, and only one of them should report.
    #[test]
    fn only_the_first_claimant_gets_the_record() {
        let slot = PanicSlot::new();
        slot.record("boom".to_string(), None);
        assert!(slot.take().is_some());
        assert_eq!(
            slot.take(),
            None,
            "a claimed panic must not be reported twice"
        );
    }

    #[test]
    fn an_empty_slot_has_nothing_to_claim() {
        assert_eq!(PanicSlot::new().take(), None);
    }

    /// Only the most recent is kept — a cascade reports its last panic,
    /// and the full set is in the log via the chained default hook.
    #[test]
    fn the_newest_panic_replaces_an_unclaimed_older_one() {
        let slot = PanicSlot::new();
        slot.record("first".to_string(), None);
        slot.record("second".to_string(), None);
        assert_eq!(slot.take().unwrap().message, "second");
    }

    #[test]
    fn a_str_payload_and_a_string_payload_both_read() {
        let as_str: Box<dyn std::any::Any + Send> = Box::new("static message");
        assert_eq!(payload_text(as_str.as_ref()), "static message");
        let as_string: Box<dyn std::any::Any + Send> = Box::new(String::from("owned message"));
        assert_eq!(payload_text(as_string.as_ref()), "owned message");
    }

    #[test]
    fn an_unreadable_payload_does_not_lose_the_panic() {
        let odd: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(payload_text(odd.as_ref()), "Box<dyn Any>");
    }

    /// The installed hook writes to the process slot. Asserted as
    /// "something is there" rather than on the text: this is the one
    /// test that touches the global, and a parallel test panicking on
    /// purpose can legitimately be the record we find.
    #[test]
    fn the_installed_hook_records_a_panic_the_process_survives() {
        install();
        let caught = std::panic::catch_unwind(|| panic!("survivable"));
        assert!(caught.is_err());
        assert!(
            take().is_some(),
            "a caught panic must still leave a record for its survivor to claim"
        );
    }
}
