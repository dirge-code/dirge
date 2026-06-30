//! Central output chokepoint for asynchronous messages that need to
//! reach the user but DON'T originate from the agent's own event
//! stream.
//!
//! Previously, off-stream messages (MCP server stderr, plugin
//! warnings, background-task lifecycle pings, etc.) reached the
//! user via inconsistent paths: some went through `tracing::warn!`
//! (which writes to plain stderr and paints over the alt-screen
//! UI), some called `renderer.write_line` directly from inside
//! deeply-nested task spawns (requiring `&mut Renderer` access in
//! places it shouldn't be), and some leaked control bytes through
//! sanitizers built for one specific source.
//!
//! This module owns ONE `tokio::sync::mpsc::UnboundedSender<Notification>`
//! as a process-global; producers send a typed `Notification` and
//! the UI event loop drains the channel with the same
//! `tokio::select!` arm shape as `ask_rx` / `question_rx` /
//! `lifecycle_rx`. The receiver path runs through the standard
//! `Renderer::write_line` pipeline — same wrapping, same theming,
//! same scroll behaviour — so a message from an MCP server reads
//! the same way as an agent error or a permission denial.

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::sync::{Mutex, RwLock};

use tokio::sync::mpsc::{Receiver, Sender, channel};

/// One off-stream message destined for the chat area. Variants pick
/// the visual treatment (color + prefix); content is plain text
/// that has ALREADY been sanitized of escape sequences by the
/// producer.
#[derive(Debug, Clone)]
pub enum Notification {
    /// Output from an MCP child server's stderr. Renders dim with
    /// a `[mcp:<server>]` prefix.
    McpLog { server: String, line: String },
    /// Generic informational note from a non-agent source (plugins,
    /// background tasks). Renders in the agent color.
    #[allow(dead_code)] // reserved for future producers
    Info(String),
    /// Warning from a non-agent source. Renders in the warn color.
    #[allow(dead_code)]
    Warn(String),
    /// Error from a non-agent source. Renders in the error color.
    #[allow(dead_code)]
    Error(String),
}

/// Bounded channel capacity. A sustained MCP-server stderr flood
/// (buggy panic loop, hostile / compromised child) could otherwise
/// queue unboundedly and OOM dirge — review #4. 1024 is enough
/// headroom for legitimate burst-y log emissions, and `try_send`
/// drops on overflow so a runaway producer can't outpace the UI.
const NOTIF_CAP: usize = 1024;

/// Global sender. Installed at startup; replaceable so a fresh
/// `install()` (test harness, future UI restart) swaps in a live
/// sender — review #2. RwLock instead of OnceLock so producers see
/// the LIVE sender, not an orphan bound to a dropped receiver.
///
/// Read path is hot (every MCP log line takes a read lock); write
/// path fires once at install. `RwLock` gives the right contention
/// shape.
static TX: RwLock<Option<Sender<Notification>>> = RwLock::new(None);

/// Holding pen for the receiver between `install()` (called in
/// `main()` BEFORE any producer can fire) and `take_receiver()`
/// (called by `run_interactive` to own the rx for its select
/// loop). Mutex<Option<_>> because there's no atomic
/// take-Option-by-move primitive on stable.
static RX_HOLDER: Mutex<Option<Receiver<Notification>>> = Mutex::new(None);

/// Create the channel and stash both sender + receiver. Call this
/// EARLY in `main()` (review #1), before any producer (MCP
/// stderr forwarder, plugin worker) can fire. The UI loop calls
/// `take_receiver()` to claim the receiver when it spins up.
///
/// Re-installing replaces the previous channel. A producer
/// holding a clone of the OLD sender will see `try_send` fail
/// (closed channel) on its next call and the slot is cleared
/// (review #2). New producers get the live sender via `sender()`.
pub fn install() {
    let (tx, rx) = channel(NOTIF_CAP);
    {
        let mut slot = TX.write().unwrap_or_else(|e| e.into_inner());
        *slot = Some(tx);
    }
    {
        let mut holder = RX_HOLDER.lock_ignore_poison();
        *holder = Some(rx);
    }
}

/// Claim the receiver. Called once by the UI loop at startup.
/// Returns `None` if `install()` was never called OR a previous
/// caller already took the receiver — both edge cases mean "no UI
/// notification path available", and the caller should
/// `std::future::pending()`-await as a no-op arm.
pub fn take_receiver() -> Option<Receiver<Notification>> {
    let mut holder = RX_HOLDER.lock_ignore_poison();
    holder.take()
}

/// Get a clone of the live sender. Returns `None` if `install()`
/// hasn't been called yet (very early startup, CLI-only paths,
/// tests). Producers should `.ok()`-style the failure.
pub fn sender() -> Option<Sender<Notification>> {
    let slot = TX.read().unwrap_or_else(|e| e.into_inner());
    slot.clone()
}

pub fn notify_mcp_log(server: &str, line: &str) {
    notify_send(Notification::McpLog {
        server: server.to_string(),
        line: line.to_string(),
    });
}

/// Generic send helper — used by future Info/Warn/Error producers
/// so they share the orphan-detection + bounded-drop semantics.
#[allow(dead_code)]
pub fn notify_send(notif: Notification) {
    let Some(tx) = sender() else {
        return;
    };
    if tx.try_send(notif).is_err() {
        // Either the channel is full (drop the line, can't keep up
        // with a runaway producer — better than OOM) or the
        // receiver was dropped (UI exited / restarted). Clear the
        // slot in the latter case so subsequent producers don't
        // keep retrying against a dead channel. Capacity-full
        // would just transiently shed; we accept the redundant
        // slot-clear there.
        if tx.is_closed() {
            let mut slot = TX.write().unwrap_or_else(|e| e.into_inner());
            // Only clear if the slot still points at THIS dead
            // sender. A concurrent `install()` may have already
            // swapped in a fresh one — don't trample it.
            if let Some(current) = slot.as_ref()
                && current.same_channel(&tx)
            {
                *slot = None;
            }
        }
    }
}

/// Send an MCP log line through the notification channel.
/// Convenience wrapper for the stderr forwarder. Uses `try_send`
/// — drops the line if the queue is full (review #4), and detects
/// orphaned senders to clear the slot (review #2) so producers
/// don't keep pumping into a dead channel after a UI restart.
///
/// Receiver-side sanitization (review #7) happens in the UI event
/// loop so EVERY notification gets stripped of control bytes
/// regardless of how careful the producer was.
#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise tests that share global TX / RX_HOLDER state.
    /// cargo runs unit tests in parallel by default, but
    /// `install()` mutates singleton state — two concurrent tests
    /// would race for the receiver.
    static TEST_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Review #2: re-installing replaces the previous channel.
    /// The old sender continues to exist (held by producers) but
    /// `try_send` returns Err once the receiver is dropped.
    #[test]
    fn install_replaces_previous_channel() {
        let _g = TEST_GATE.lock_ignore_poison();
        // Two installs in sequence — second wins.
        install();
        let rx1 = take_receiver().expect("first take");
        install();
        let mut rx2 = take_receiver().expect("second take after replace");
        drop(rx1);
        // Producer obtains the LIVE sender (post second install).
        let s = sender().expect("sender after replace");
        s.try_send(Notification::Info("ping".to_string())).unwrap();
        // Receiver gets the message via rx2, not rx1.
        let n = rx2.try_recv().expect("message delivered");
        match n {
            Notification::Info(s) => assert_eq!(s, "ping"),
            _ => panic!("wrong variant"),
        }
    }

    /// Review #4: bounded channel — `try_send` returns Err when
    /// full instead of growing unboundedly.
    #[test]
    fn bounded_channel_drops_on_full() {
        let _g = TEST_GATE.lock_ignore_poison();
        install();
        // Don't take_receiver — leave the rx in the holder so the
        // channel stays open but unread. Producers can fill it
        // up to NOTIF_CAP and then start failing.
        let s = sender().expect("sender installed");
        let mut accepted = 0;
        let mut dropped = 0;
        for i in 0..(NOTIF_CAP + 10) {
            match s.try_send(Notification::Info(format!("msg {i}"))) {
                Ok(()) => accepted += 1,
                Err(_) => dropped += 1,
            }
        }
        assert_eq!(accepted, NOTIF_CAP);
        assert_eq!(dropped, 10);
    }
}
