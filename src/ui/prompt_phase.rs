//! Off-loop `on-prompt` hook phase (dirge-qhfk).
//!
//! Firing `on-prompt` inline in the submit arm blocked the single
//! `current_thread` runtime inside the Janet worker, so a hook opening a
//! dialog deadlocked the loop that services `dialog_rx`. This runs the
//! dispatch on a spawned task (via `spawn_blocking`); the `prompt_phase`
//! `select!` arm then prints the hook's `[plugin]` output and runs the shared
//! submit tail ([`crate::ui::run_handlers::submit::submit_resolved_prompt`])
//! with the resolved hint/replace.
//!
//! Types are unconditional (the `select!` arm can't be `#[cfg]`-gated); only
//! [`spawn`] — which touches the plugin manager — is plugin-gated, so the
//! field stays `None` and the arm never fires without the feature.

use crate::ui::phase::PhaseHandle;

/// The resolved outputs of the `on-prompt` hook chain, collected off-loop.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub(crate) struct PromptResult {
    /// Prepended to the prompt (`harness/request-prompt` or hook return value).
    pub hint: Option<String>,
    /// Full prompt rewrite (`harness/replace-prompt`); wins over `hint`.
    pub replace: Option<String>,
    /// Sanitized `[plugin]` output lines to print on-loop.
    pub lines: Vec<String>,
    /// Hook/dispatch error lines to print on-loop.
    pub errors: Vec<String>,
}

/// Terminal event from the spawned `on-prompt` task.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub(crate) enum PromptPhaseEvent {
    Ready(PromptResult),
}

/// Handle to the spawned `on-prompt` task plus the original user text (needed
/// on-loop to build the final prompt after the hook resolves).
pub(crate) struct PromptPhaseHandle {
    pub core: PhaseHandle<PromptPhaseEvent>,
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub text: String,
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub images: Vec<crate::agent::agent_loop::message::ImageRef>,
}

/// Spawn the `on-prompt` dispatch off-loop. The blocking Janet call runs on
/// `spawn_blocking` so the loop keeps draining `dialog_rx` while a hook waits
/// on a dialog.
#[cfg(feature = "plugin")]
pub(crate) fn spawn(
    pm: std::sync::Arc<std::sync::Mutex<crate::plugin::PluginManager>>,
    text: String,
    images: Vec<crate::agent::agent_loop::message::ImageRef>,
) -> PromptPhaseHandle {
    let task_text = text.clone();
    let core = PhaseHandle::spawn(1, move |tx| async move {
        let result = tokio::task::spawn_blocking(move || {
            use crate::sync_util::LockExt;
            let mut mgr = pm.lock_ignore_poison();
            dispatch_on_prompt(&mut mgr, &task_text)
        })
        .await
        .unwrap_or_else(|_| PromptResult {
            hint: None,
            replace: None,
            lines: Vec::new(),
            errors: vec!["on-prompt hook task panicked".to_string()],
        });
        let _ = tx.send(PromptPhaseEvent::Ready(result)).await;
    });
    PromptPhaseHandle { core, text, images }
}

/// Run the `on-prompt` hook and collect its outputs. Mirrors the original
/// inline dispatch: hook return value (or `harness/request-prompt`) becomes
/// `hint`; `harness/replace-prompt` becomes `replace`; `[plugin]` lines are
/// sanitized for on-loop display.
#[cfg(feature = "plugin")]
fn dispatch_on_prompt(mgr: &mut crate::plugin::PluginManager, text: &str) -> PromptResult {
    use crate::ui::events::sanitize_output;
    let mut hint = None;
    let mut lines = Vec::new();
    let mut errors = Vec::new();
    match mgr.dispatch(
        "on-prompt",
        &format!(
            "@{{:prompt \"{}\"}}",
            crate::plugin::escape_janet_string(text)
        ),
    ) {
        Ok(results) if !results.is_empty() => {
            for line in &results {
                lines.push(sanitize_output(line).to_string());
            }
            hint = Some(results.join("\n"));
        }
        Ok(_) => {}
        Err(e) => errors.push(format!("on-prompt error: {e}")),
    }
    // A hook may queue a follow-up prompt via harness/request-prompt.
    if let Some(pending) = mgr.take_pending_prompt() {
        hint = Some(pending);
    }
    // harness/replace-prompt rewrites the current turn entirely (wins over hint).
    let replace = mgr.take_pending_prompt_replace();
    PromptResult {
        hint,
        replace,
        lines,
        errors,
    }
}

#[cfg(all(test, feature = "plugin"))]
mod tests {
    use super::{PromptPhaseEvent, spawn};
    use crate::plugin::PluginManager;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // dirge-qhfk: the on-prompt phase runs the hook OFF the loop and reports
    // its hint + [plugin] lines back through the terminal event. (Off-loop
    // dispatch is what keeps a dialog-opening hook from deadlocking the loop;
    // that the dispatch runs on spawn_blocking is covered by the phase module.)
    #[tokio::test]
    async fn on_prompt_phase_collects_hint() {
        let mut manager = PluginManager::try_new().unwrap();
        manager.eval("(defn on-prompt [ctx] \"HINT\")").unwrap();
        manager.register("on-prompt", "on-prompt");
        let pm = Arc::new(Mutex::new(manager));

        let mut handle = spawn(pm, "hello".to_string(), Vec::new());
        assert_eq!(handle.text, "hello");
        let ev = tokio::time::timeout(Duration::from_secs(5), handle.core.rx.recv())
            .await
            .expect("on-prompt phase should complete promptly");
        match ev {
            Some(PromptPhaseEvent::Ready(result)) => {
                assert_eq!(result.hint.as_deref(), Some("HINT"));
                assert_eq!(result.lines, vec!["HINT".to_string()]);
                assert!(result.replace.is_none());
                assert!(result.errors.is_empty(), "{:?}", result.errors);
            }
            None => panic!("on-prompt phase produced no event"),
        }
    }
}
