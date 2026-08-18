//! Cross-thread cancellation for a Janet worker thread.
//!
//! Janet evaluation is synchronous and owns its OS thread, so a runaway
//! cell (`(while true)`) cannot be stopped by dropping the reply channel —
//! [`Worker::eval_with_timeout`](super::worker::Worker::eval_with_timeout)
//! abandons the *reply*, but the `Cmd::Eval` keeps running and every later
//! hook, command and permission-gate dispatch queues behind it forever.
//!
//! Janet 1.37 exports a cross-thread interrupt for exactly this:
//!
//! ```text
//! janet_local_vm()                 -> *mut JanetVM   (this thread's VM)
//! janet_interpreter_interrupt(vm)                    (any thread)
//! janet_interpreter_interrupt_handled(vm)            (clears it)
//! ```
//!
//! `janet_interpreter_interrupt` atomically increments `vm->auto_suspend`.
//! The interpreter checks that counter at backward jumps and at call /
//! return boundaries (`vm_maybe_auto_suspend` in janet.c) and returns
//! `JANET_SIGNAL_INTERRUPT` from the running fiber. `janet_dobytes` treats
//! any signal other than OK/EVENT as a failure, so the interrupt surfaces
//! to Rust as a normal `client.run` error and **the root env survives** —
//! which is the whole point: a notebook that loses its state on every
//! runaway cell is not a notebook.
//!
//! # Two properties worth knowing
//!
//! `JANET_SIGNAL_INTERRUPT` is `JANET_SIGNAL_USER8`, and a fiber only traps
//! it when created with the `r`/`u` flags. Janet's `try` compiles to the
//! `e` (error-only) mask, so **plugin code cannot swallow the interrupt
//! with `(try …)`** — a runaway `(try (while true) ([e] nil))` still dies.
//! Even a deliberately-adversarial `(fiber/new f :a)` only buys one more
//! backward jump, because `auto_suspend` stays raised until the worker
//! clears it after the eval returns.
//!
//! The interrupt breaks *bytecode*, not a thread parked in a C syscall.
//! `(os/execute …)` on a hung subprocess, or a blocking read on a pipe
//! that never closes, is unaffected. That residual case is what running
//! the agent-facing notebook on its own VM covers (dirge-9xjg.2): the
//! wedged VM is respawned and plugin state is untouched.
//!
//! # Who clears the flag
//!
//! `auto_suspend` is a counter, not a latch: every `interrupt` must be
//! matched by exactly one `interrupt_handled` or the VM refuses to run
//! scheduled fibers ever again (janet.c's event loop breaks out while
//! `auto_suspend` is non-zero). The raiser cannot do it — it does not know
//! when the eval unwound. So [`InterruptHandle`] counts raises and the
//! worker thread drains that counter around each eval: once *before*
//! (discarding an interrupt raised while idle, which has nothing to
//! interrupt) and once *after* (clearing the one the eval just consumed).

// In a non-plugin build there is no VM to interrupt: `raise` is a
// no-op returning false and nothing drains. The type stays compiled so
// `Worker`'s field and signatures are feature-independent.
#![cfg_attr(not(feature = "plugin"), allow(dead_code))]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "plugin")]
use std::sync::atomic::AtomicPtr;

/// Shared interrupt channel between a Janet worker thread and its callers.
///
/// The worker installs its VM pointer once at startup ([`install`]);
/// any thread may then [`raise`] an interrupt, and only the worker thread
/// may [`drain`] the outstanding count.
///
/// [`install`]: InterruptHandle::install
/// [`raise`]: InterruptHandle::raise
/// [`drain`]: InterruptHandle::drain
#[derive(Debug)]
pub struct InterruptHandle {
    /// This worker's `JanetVM*`, published by the worker thread before it
    /// signals init-complete. Null until then, so a `raise` that races
    /// startup is a no-op rather than a wild pointer dereference.
    ///
    /// `AtomicPtr` rather than a `unsafe impl Send` newtype: the pointer is
    /// never dereferenced in Rust, only handed back to Janet's own
    /// documented cross-thread entry points.
    #[cfg(feature = "plugin")]
    vm: AtomicPtr<janetrs::lowlevel::JanetVM>,
    /// Raises not yet matched by an `interrupt_handled`.
    pending: AtomicUsize,
}

impl InterruptHandle {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            #[cfg(feature = "plugin")]
            vm: AtomicPtr::new(std::ptr::null_mut()),
            pending: AtomicUsize::new(0),
        })
    }

    /// Publish this thread's Janet VM. **Must** be called from the worker
    /// thread itself — `janet_local_vm()` returns the caller's thread-local
    /// VM, so calling it anywhere else publishes the wrong one (or null).
    #[cfg(feature = "plugin")]
    pub fn install_local_vm(&self) {
        // SAFETY: `janet_local_vm` reads the calling thread's VM struct.
        // The caller contract above requires this run on the worker thread
        // after `JanetClient` init, so the VM is live.
        let vm = unsafe { janetrs::lowlevel::janet_local_vm() };
        self.vm.store(vm, Ordering::SeqCst);
    }

    /// Ask the worker's interpreter to stop at its next safe point.
    ///
    /// Returns `false` when there is no VM to interrupt (worker still
    /// starting, or a non-plugin build), so the caller can skip the grace
    /// wait and report the timeout immediately.
    ///
    /// Safe to call from any thread, and safe to call more than once — each
    /// raise is counted and cleared by [`drain`](Self::drain).
    pub fn raise(&self) -> bool {
        #[cfg(feature = "plugin")]
        {
            let vm = self.vm.load(Ordering::SeqCst);
            if vm.is_null() {
                return false;
            }
            // Count BEFORE raising. If the order were reversed the worker
            // could observe the signal, unwind, and drain a counter that
            // does not yet include this raise — leaving `auto_suspend`
            // permanently non-zero and the VM permanently unable to run.
            self.pending.fetch_add(1, Ordering::SeqCst);
            // SAFETY: `janet_interpreter_interrupt` is Janet's documented
            // cross-thread entry point; it does nothing but an atomic
            // increment on `vm->auto_suspend`. `vm` came from
            // `janet_local_vm` on the worker thread and stays valid for the
            // life of that thread, which outlives every `Worker` holding
            // this handle.
            unsafe { janetrs::lowlevel::janet_interpreter_interrupt(vm) };
            true
        }
        #[cfg(not(feature = "plugin"))]
        {
            false
        }
    }

    /// Clear every outstanding interrupt. **Worker thread only** — this is
    /// the only decrementer, which is what makes the CAS loop below
    /// underflow-free.
    ///
    /// Called around each eval; see the module docs for why both sides.
    pub fn drain(&self) {
        #[cfg(feature = "plugin")]
        {
            let vm = self.vm.load(Ordering::SeqCst);
            if vm.is_null() {
                return;
            }
            while let Some(n) = self.take_one() {
                let _ = n;
                // SAFETY: same contract as `raise`; decrements the same
                // atomic counter. Called exactly once per successful
                // `take_one`, so increments and decrements stay balanced.
                unsafe { janetrs::lowlevel::janet_interpreter_interrupt_handled(vm) };
            }
        }
    }

    /// Decrement `pending` if non-zero. Returns the pre-decrement value.
    fn take_one(&self) -> Option<usize> {
        let mut cur = self.pending.load(Ordering::SeqCst);
        loop {
            if cur == 0 {
                return None;
            }
            match self
                .pending
                .compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Some(cur),
                Err(actual) => cur = actual,
            }
        }
    }

    /// Outstanding (raised but not yet cleared) interrupts. Test/telemetry
    /// only — the count is inherently racy from any thread but the worker.
    #[cfg(test)]
    pub fn pending_count(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without an installed VM there is nothing to interrupt, and `raise`
    /// must say so rather than counting a raise nobody will ever clear.
    #[test]
    fn raise_without_a_vm_is_a_no_op() {
        let h = InterruptHandle::new();
        assert!(!h.raise());
        assert_eq!(h.pending_count(), 0);
    }

    /// `drain` with no VM must not spin or underflow.
    #[test]
    fn drain_without_a_vm_is_a_no_op() {
        let h = InterruptHandle::new();
        h.drain();
        assert_eq!(h.pending_count(), 0);
    }

    /// The counter is what keeps `interrupt` and `interrupt_handled`
    /// balanced, so it must never go below zero no matter how many drains
    /// race a single raise.
    #[test]
    fn take_one_stops_at_zero() {
        let h = InterruptHandle::new();
        h.pending.store(2, Ordering::SeqCst);
        assert_eq!(h.take_one(), Some(2));
        assert_eq!(h.take_one(), Some(1));
        assert_eq!(h.take_one(), None);
        assert_eq!(h.take_one(), None);
        assert_eq!(h.pending_count(), 0);
    }
}
