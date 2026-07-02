//! Off-loop Done plugin-chain phase (dirge-qhfk).
//!
//! `AgentEvent::Done` fires the `on-response` → `message-end` → `on-complete`
//! → `prepare-next-run` hook chain. Running it inline blocked the single
//! `current_thread` runtime inside the Janet worker, so a hook opening a
//! dialog deadlocked the loop that services `dialog_rx`. This runs the whole
//! chain on a spawned task (via `spawn_blocking`); the `done_phase` `select!`
//! arm then prints the collected `[plugin]` output, applies any model swap,
//! and runs the shared `finish_done` tail with the (possibly rewritten)
//! response.
//!
//! Types are unconditional (the `select!` arm can't be `#[cfg]`-gated); only
//! [`spawn`] — which touches the plugin manager — is plugin-gated, so the
//! field stays `None` and the arm never fires without the feature.

use compact_str::CompactString;

use crate::ui::phase::PhaseHandle;

/// Outputs of the Done hook chain, collected off-loop.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub(crate) struct DoneChainResult {
    /// FINAL assistant text — the `message-end` rewrite, if any, is applied.
    pub response: CompactString,
    /// Follow-up prompt from `on-response` (return value or request-prompt).
    pub followup: Option<String>,
    /// Sanitized `[plugin]` output lines from `on-response`, to print on-loop.
    pub lines: Vec<String>,
    /// Hook error lines, to print on-loop.
    pub errors: Vec<String>,
    /// New model from `prepare-next-run` (`harness-next-model`), if set.
    pub next_model: Option<String>,
}

/// Terminal event from the spawned Done-chain task.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub(crate) enum DonePhaseEvent {
    Ready(DoneChainResult),
}

/// Handle to the spawned Done-chain task plus the turn's token/cost totals
/// (captured at Done receipt, applied on-loop by the completion arm).
pub(crate) struct DonePhaseHandle {
    pub core: PhaseHandle<DonePhaseEvent>,
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub tokens: u64,
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub cost: f64,
}

/// Spawn the Done hook chain off-loop. The blocking Janet calls run on
/// `spawn_blocking` so the loop keeps draining `dialog_rx` while a hook waits
/// on a dialog.
#[cfg(feature = "plugin")]
pub(crate) fn spawn(
    pm: std::sync::Arc<std::sync::Mutex<crate::plugin::PluginManager>>,
    response: CompactString,
    tokens: u64,
    cost: f64,
) -> DonePhaseHandle {
    let core = PhaseHandle::spawn(1, move |tx| async move {
        let fallback = response.clone();
        let result = tokio::task::spawn_blocking(move || {
            use crate::sync_util::LockExt;
            let mut mgr = pm.lock_ignore_poison();
            run_done_chain(&mut mgr, response)
        })
        .await
        .unwrap_or_else(|_| DoneChainResult {
            response: fallback,
            followup: None,
            lines: Vec::new(),
            errors: vec!["done-chain hook task panicked".to_string()],
            next_model: None,
        });
        let _ = tx.send(DonePhaseEvent::Ready(result)).await;
    });
    DonePhaseHandle { core, tokens, cost }
}

/// Run the Done hook chain in order (the worker serializes evals, so the
/// on-response → message-end → store_response → on-complete → prepare-next-run
/// ordering holds), collecting the outputs the completion arm applies on-loop.
/// Mirrors the original inline chain exactly.
#[cfg(feature = "plugin")]
fn run_done_chain(
    mgr: &mut crate::plugin::PluginManager,
    mut response: CompactString,
) -> DoneChainResult {
    use crate::plugin::escape_janet_string;
    use crate::ui::events::sanitize_output;

    let mut lines = Vec::new();
    let mut errors = Vec::new();
    let mut followup = None;

    match mgr.dispatch(
        "on-response",
        &format!("@{{:response \"{}\"}}", escape_janet_string(&response)),
    ) {
        Ok(results) if !results.is_empty() => {
            for line in &results {
                lines.push(sanitize_output(line).to_string());
            }
            followup = Some(results.join("\n"));
        }
        Ok(_) => {}
        Err(e) => errors.push(format!("on-response error: {e}")),
    }
    // A follow-up prompt queued via harness/request-prompt wins over the return.
    if let Some(pending) = mgr.take_pending_prompt() {
        followup = Some(pending);
    }

    // message-end: a plugin may rewrite the STORED/persisted text (already
    // streamed to the screen) via harness/rewrite-message.
    match mgr.dispatch(
        "message-end",
        &format!("@{{:message \"{}\"}}", escape_janet_string(&response)),
    ) {
        Ok(_) => {
            if let Some(rewritten) = mgr.take_message_rewrite() {
                response = CompactString::new(&rewritten);
            }
        }
        Err(e) => errors.push(format!("message-end error: {e}")),
    }
    // Store the FINAL response so the next hook sees it (before it's cleared).
    mgr.store_response(&response);

    if let Err(e) = mgr.dispatch("on-complete", "@{}") {
        errors.push(format!("on-complete error: {e}"));
    }
    if let Err(e) = mgr.dispatch("prepare-next-run", "@{}") {
        errors.push(format!("prepare-next-run error: {e}"));
    }
    let next_model = mgr.take_pending_next_model();

    // Clear harness-response so the next turn's hook doesn't see stale text.
    let _ = mgr.eval("(set harness-response nil)");

    DoneChainResult {
        response,
        followup,
        lines,
        errors,
        next_model,
    }
}

#[cfg(all(test, feature = "plugin"))]
mod tests {
    use super::{DonePhaseEvent, spawn};
    use crate::plugin::PluginManager;
    use compact_str::CompactString;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // dirge-qhfk: the Done chain runs OFF the loop and reports its outputs
    // (on-response follow-up + lines, message-end rewrite, next-model) back
    // through the terminal event — with ordering preserved (message-end
    // rewrite reflected in the FINAL response the tail persists).
    #[tokio::test]
    async fn done_chain_runs_off_loop_and_collects_outputs() {
        let mut manager = PluginManager::try_new().unwrap();
        manager
            .eval("(defn on-response [ctx] \"followup-text\")")
            .unwrap();
        manager.register("on-response", "on-response");
        manager
            .eval("(defn message-end [ctx] (harness/rewrite-message \"REWRITTEN\"))")
            .unwrap();
        manager.register("message-end", "message-end");
        manager
            .eval("(defn prepare-next-run [ctx] (harness/set-next-model \"model-x\"))")
            .unwrap();
        manager.register("prepare-next-run", "prepare-next-run");
        let pm = Arc::new(Mutex::new(manager));

        let mut handle = spawn(pm, CompactString::new("original"), 100, 0.5);
        assert_eq!(handle.tokens, 100);
        assert_eq!(handle.cost, 0.5);
        let ev = tokio::time::timeout(Duration::from_secs(5), handle.core.rx.recv())
            .await
            .expect("done chain should complete promptly");
        match ev {
            Some(DonePhaseEvent::Ready(result)) => {
                // message-end rewrite must be reflected in the FINAL response
                // (what the tail persists) — not the pre-rewrite text.
                assert_eq!(result.response.as_str(), "REWRITTEN");
                assert_eq!(result.followup.as_deref(), Some("followup-text"));
                assert_eq!(result.lines, vec!["followup-text".to_string()]);
                assert_eq!(result.next_model.as_deref(), Some("model-x"));
                assert!(result.errors.is_empty(), "{:?}", result.errors);
            }
            None => panic!("done chain produced no event"),
        }
    }
}
