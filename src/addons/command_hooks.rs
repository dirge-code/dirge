//! `type: "addon"` command hooks: the entry's addon answers through the
//! handler it registered under `:dirge/command-hooks`, in the host running
//! now. The answer is read as the process it stands in for
//! ([`policy::addon_answer`]), so `policy::interpret` decodes both kinds of
//! entry alike. Every way of getting no answer fails open: no verdict, the
//! action allowed.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

use crate::agent::command_hooks::boundary::HookRunner;
use crate::agent::command_hooks::domain::{Exited, HookCommand, HookError};
use crate::agent::command_hooks::policy;

/// Adapter: answers addon entries from the process-wide addon host.
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveAddonHookRunner;

impl HookRunner for LiveAddonHookRunner {
    fn run(
        &self,
        cmd: &HookCommand,
        payload: &str,
        _project_dir: &Path,
    ) -> Result<Exited, HookError> {
        let (addon, handler) = cmd
            .addon_target()
            .ok_or_else(|| HookError::SpawnFailed("not an addon hook entry".to_string()))?;
        let host = super::global()
            .ok_or_else(|| HookError::SpawnFailed("the addon host is not running".to_string()))?;
        let payload: Value = serde_json::from_str(payload)
            .map_err(|e| HookError::SpawnFailed(format!("hook payload is not JSON: {e}")))?;
        let (addon, handler) = (addon.to_string(), handler.to_string());
        let call = move || host.run_hook_handler(&addon, &handler, &payload);
        let secs = cmd.timeout_secs();
        let answer = if super::cljrs::isolate::on_event_loop_thread() {
            // The isolate bounds this caller itself and refuses it anything
            // that waits on the loop (an MCP call), so it cannot hang it.
            call()
        } else {
            within(Duration::from_secs(secs), call).ok_or(HookError::TimedOut(secs))?
        };
        answer
            .map(|value| policy::addon_answer(&value))
            .map_err(|stderr| HookError::NonZeroExit { code: None, stderr })
    }
}

/// `work`'s answer, or `None` when it has none within `budget`. The work
/// runs on its own thread, outside any async runtime, and is left to finish
/// on its own when it overruns.
fn within<T: Send + 'static>(
    budget: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("dirge-addon-hook".into())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .ok()?;
    rx.recv_timeout(budget).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_answers_in_time_and_gives_up_after() {
        assert_eq!(within(Duration::from_secs(2), || 7), Some(7));
        let slow = within(Duration::from_millis(20), || {
            std::thread::sleep(Duration::from_millis(500));
            7
        });
        assert_eq!(slow, None);
    }

    #[test]
    fn without_a_host_an_addon_entry_fails_open() {
        let cmd: HookCommand =
            serde_json::from_str(r#"{"type": "addon", "addon": "a", "handler": "h"}"#).unwrap();
        if super::super::global().is_none() {
            assert!(matches!(
                LiveAddonHookRunner.run(&cmd, "{}", Path::new(".")),
                Err(HookError::SpawnFailed(_))
            ));
        }
    }
}
