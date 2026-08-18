//! The agent-facing notebook kernel (dirge-9xjg).
//!
//! A persistent Janet VM the model evaluates cells against. State — defs,
//! loaded data, open handles — accumulates across tool calls and across
//! in-process subagents, so the agent builds context up incrementally
//! instead of re-deriving it on every call or writing one-off scripts.
//!
//! # Why this is a process-global rather than a `PluginManager` field
//!
//! The bridge cfn (`harness/__notebook`) runs **on the plugin worker
//! thread**, which is reached from `PluginManager::invoke_plugin_tool`
//! while the caller holds the `PluginManager` mutex. If the notebook
//! worker lived inside `PluginManager`, the cfn would have to re-acquire
//! that same mutex and deadlock every single call. A separate lock has no
//! such cycle: the plugin lock is never taken from inside this one.
//!
//! # Lazily spawned
//!
//! A second Janet VM costs a thread and an env, and most sessions never
//! evaluate a cell. Nothing is spawned until the first call, so processes
//! that don't use the notebook pay nothing.
//!
//! # Known bound: head-of-line blocking is reduced, not removed
//!
//! The tool is delivered as a plugin, so a cell reaches this module through
//! the plugin worker, which is parked for the cell's duration. Concurrent
//! subagents' hook dispatch queues behind it, bounded by
//! [`CELL_TIMEOUT`]. What the VM split *does* buy is the failure case: a
//! cell that wedges in a C syscall no longer takes plugins, hooks and the
//! permission gate down with it for the session — only the notebook needs
//! respawning, and plugin state is untouched. Eliminating the queueing
//! entirely means making the notebook a core tool that bypasses the plugin
//! worker; that is a deliberate follow-up, not an oversight.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use super::worker::{CellOutput, Worker};

/// Wall-clock bound on one notebook cell.
///
/// Deliberately under `INTERACTIVE_EVAL_TIMEOUT` (30 s), the budget the
/// plugin worker gives the tool handler that calls us. If a cell could
/// outlast that, the OUTER eval would time out first and interrupt the
/// *plugin* VM — recoverable, but it would report the wrong thing as
/// wedged and leave the cell running unobserved.
pub const CELL_TIMEOUT: Duration = Duration::from_secs(20);

/// Cap on captured output returned to the model, per stream.
pub const CELL_OUTPUT_LIMIT: usize = 8192;

/// The notebook VM. `None` before the first cell, and again after a
/// [`respawn`] fails, so a failed respawn cannot leave a dead worker
/// behind that every later call would time out against.
fn slot() -> &'static Mutex<Option<Worker>> {
    static SLOT: OnceLock<Mutex<Option<Worker>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Run `f` against the notebook worker, spawning it if needed.
fn with_worker<T>(f: impl FnOnce(&mut Worker) -> T) -> Result<T, String> {
    let mut guard = slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        *guard = Some(Worker::try_spawn_notebook()?);
    }
    let worker = guard.as_mut().expect("just spawned");
    Ok(f(worker))
}

/// Evaluate one cell in `session`'s env.
///
/// `Err` means the notebook itself is unreachable. A cell that *raises* is
/// an ordinary result and comes back as `Ok` with `ok: false` — the model
/// needs the error text and whatever printed before it, which is data, not
/// a host failure.
pub fn eval_cell(session: &str, code: &str) -> Result<CellOutput, String> {
    with_worker(|w| w.eval_cell(code, Some(session), CELL_OUTPUT_LIMIT, CELL_TIMEOUT))?
}

/// Drop one session's bindings, leaving `notebook/shared` and every other
/// session alone. This is the agent's own recovery path for a poisoned
/// scratch env — without it, a bad `def` needs an operator.
pub fn reset_session(session: &str) -> Result<String, String> {
    with_worker(|w| {
        w.eval(&format!(
            r#"(notebook/-reset "{}")"#,
            super::escape_janet_string(session)
        ))
    })?
}

/// Sessions with live state, for the agent's own orientation.
pub fn sessions() -> Result<String, String> {
    with_worker(|w| w.eval("(string/join (map string (notebook/-sessions)) \" \")"))?
}

/// Tear the notebook VM down and start a fresh one.
///
/// The backstop for the one case the interrupt cannot reach: a cell parked
/// in a C syscall. This loses **all** notebook state, which is why it is
/// last-resort and why it exists at all — before the VM split, the only
/// recovery was respawning the shared worker, which would also have taken
/// every plugin's state with it.
pub fn respawn() -> Result<(), String> {
    let mut guard = slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Drop first: `Worker::drop` bounds its own join and leaks the thread
    // if it is wedged, so this cannot hang even when the reason we are
    // respawning is that the old VM will never return.
    *guard = None;
    *guard = Some(Worker::try_spawn_notebook()?);
    Ok(())
}

#[cfg(all(test, feature = "plugin"))]
mod tests {
    use super::*;

    /// Every test here drives ONE process-global VM, and `respawn`
    /// deliberately destroys it. Under `cargo test` (threads, unlike
    /// nextest's process-per-test) they would otherwise stomp each other,
    /// and the failure mode is a test passing for the wrong reason rather
    /// than an obvious crash.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static GATE: Mutex<()> = Mutex::new(());
        GATE.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The headline property: state accumulates across separate calls, so
    /// the agent can build context up instead of re-deriving it.
    #[test]
    fn state_accumulates_across_cells() {
        let _guard = serial();
        let s = "test-accumulate";
        let _ = reset_session(s);
        eval_cell(s, "(def base 40)").unwrap();
        eval_cell(s, "(def more 2)").unwrap();
        let out = eval_cell(s, "(+ base more)").unwrap();
        assert!(out.ok, "{out:?}");
        assert_eq!(out.value, "42");
    }

    /// A raising cell is data, not a host error, and must keep both its
    /// message and whatever it printed first.
    #[test]
    fn a_raising_cell_is_reported_as_data() {
        let _guard = serial();
        let s = "test-raise";
        let _ = reset_session(s);
        let out = eval_cell(s, r#"(print "got here") (error "nope")"#).unwrap();
        assert!(!out.ok, "expected the cell to report failure: {out:?}");
        assert!(out.value.contains("nope"), "{out:?}");
        assert!(out.output.contains("got here"), "{out:?}");
    }

    /// Reset has to actually clear the session, or the agent's only
    /// self-recovery path is a no-op.
    #[test]
    fn reset_clears_only_its_own_session() {
        let _guard = serial();
        let (a, b) = ("test-reset-a", "test-reset-b");
        let _ = reset_session(a);
        let _ = reset_session(b);
        eval_cell(a, "(def keep-me :a)").unwrap();
        eval_cell(b, "(def keep-me :b)").unwrap();

        reset_session(a).unwrap();
        assert!(
            !eval_cell(a, "keep-me").unwrap().ok,
            "session a survived reset"
        );
        assert_eq!(eval_cell(b, "keep-me").unwrap().value, ":b");
    }

    /// Respawn is the backstop for a syscall-parked cell. It loses state by
    /// design — assert that, so nobody "fixes" reset into a respawn.
    #[test]
    fn respawn_replaces_the_vm_and_loses_state() {
        let _guard = serial();
        let s = "test-respawn";
        eval_cell(s, "(def before :here)").unwrap();
        assert_eq!(eval_cell(s, "before").unwrap().value, ":here");
        respawn().unwrap();
        assert!(
            !eval_cell(s, "before").unwrap().ok,
            "state survived a respawn"
        );
        // And the fresh VM works.
        assert_eq!(eval_cell(s, "(+ 1 1)").unwrap().value, "2");
    }
}
