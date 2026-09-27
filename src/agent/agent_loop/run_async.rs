//! Generic async tool-call support through the `run_async` dispatcher tool.
//!
//! `bash(background=true)` covers detached SHELLS and `task(background=true)`
//! covers detached SUBAGENTS; neither lets the model run an arbitrary long
//! tool call without blocking its own turn. `run_async` closes that gap for
//! tools that opt in via [`LoopTool::supports_async`]. The call is prepared
//! through the foreground pipeline, then its execute+finalize future is
//! detached into the session's [`BackgroundStore`]. Only tools that do not
//! require interactive approval during execution may opt in. Completion lands
//! as the standard `<system-reminder>` follow-up, and `task_status` can query
//! it — the same push machinery background subagents use.
//!
//! Foreground calls still obey the dispatch budget watchdog, which stops
//! over-budget calls rather than detaching them.

use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::agent::tools::background::BackgroundStore;

use super::result::LoopToolResult;
use super::tool::{AbortSignal, LoopTool, LoopToolUpdate};
use super::types::{Context, LoopConfig};

/// Sentinel marking a deferred call's synthetic tool result. The dispatcher
/// checks for this instead of `is_error` because a successful defer must not
/// read as a failure to the failure trackers.
pub const ASYNC_DEFERRED_MARKER: &str = "⏳ async:";

/// Deferred calls share the subagent in-flight cap — both are detached work
/// tracked by the same store, and the cap exists to stop a runaway model
/// burning unbounded machine/API resources in the background.
pub const ASYNC_CAP_ERROR: &str = "background task cap reached";

/// True when any of dirge's interactive ask channels has a pending prompt.
/// Detached work must not initiate an interactive approval; eligibility is
/// restricted to tools that do not prompt during execution.
pub(crate) fn interactive_prompt_pending() -> bool {
    crate::human_wait::anyone_waiting()
}

/// Build the synthetic result a deferred call returns to the transcript.
/// Worded to tell the model the work is detached, how to check on it, and
/// NOT to block on it.
fn deferred_result(task_id: &str, detail: &str) -> LoopToolResult {
    LoopToolResult {
        content: vec![serde_json::json!({
            "type": "text",
            "text": format!(
                "{ASYNC_DEFERRED_MARKER} {detail} — background task id: {task_id}. \
                 It runs in the background; its result arrives automatically as a \
                 <system-reminder> when it finishes (query it mid-turn with \
                 task_status). Do not wait for it — continue with other work."
            ),
        })],
        details: serde_json::json!({"async_task_id": task_id}),
        terminate: None,
    }
}

/// Spawn a prepared call as a detached background task.
///
/// `execute_future` must already be boxed `'static` (its captures cloned
/// ahead of time by the caller). The spawned task races it against a FRESH
/// session-lifetime `AbortSignal` — deliberately not the caller's signal:
/// detaching means the turn-level cancel/budget window no longer applies.
/// Terminal outcome goes through `finalize_future` and lands in the store.
///
/// Returns the synthetic deferred result, or an error when the cap
/// refuses the spawn.
#[allow(clippy::too_many_arguments)]
pub(super) fn defer_prepared_call(
    store: &BackgroundStore,
    execute_future: Pin<
        Box<dyn std::future::Future<Output = Result<LoopToolResult, String>> + Send>,
    >,
    finalize_future: impl FnOnce(
        super::tools::ExecutedOutcome,
    ) -> Pin<
        Box<dyn std::future::Future<Output = super::tools::FinalizedOutcome> + Send>,
    > + Send
    + 'static,
    detail: &str,
) -> Result<LoopToolResult, String> {
    if store.running_count() >= BackgroundStore::max_concurrent() {
        return Err(format!(
            "{ASYNC_CAP_ERROR} ({} running). Wait for one to finish or query \
             task_status; the call was NOT started.",
            BackgroundStore::max_concurrent()
        ));
    }
    let task_id = format!("async-{}", uuid::Uuid::new_v4());
    store.insert(task_id.clone());
    store.notify_started(&task_id);

    let store_for_task = store.clone();
    let task_id_for_task = task_id.clone();
    let handle = tokio::spawn(async move {
        let signal = AbortSignal::new();
        // Race the tool against the fresh signal. The session-swap path
        // aborts this whole task via the handle, so the select is mostly
        // belt-and-braces for the tool's own cooperative cancellation.
        let outcome = tokio::select! {
            biased;
            _ = signal.cancelled() => super::tools::ExecutedOutcome {
                result: super::tools::create_error_tool_result(
                    super::side_effect::ABORTED_SENTINEL,
                ),
                is_error: true,
            },
            res = execute_future => match res {
                Ok(result) => super::tools::ExecutedOutcome {
                    result,
                    is_error: false,
                },
                Err(err) => super::tools::ExecutedOutcome {
                    result: super::tools::create_error_tool_result(&err),
                    is_error: true,
                },
            },
        };
        let finalized = finalize_future(outcome).await;
        let state = if finalized.is_error {
            crate::agent::tools::background::TaskState::Failed(super::tools::executed_result_text(
                &finalized.result,
            ))
        } else {
            crate::agent::tools::background::TaskState::Completed(
                super::tools::executed_result_text(&finalized.result),
            )
        };
        store_for_task.notify(&task_id_for_task, state);
    });
    store.attach_handle(&task_id, handle);
    Ok(deferred_result(&task_id, detail))
}

/// The `run_async` dispatcher tool. Looks the target up in the registry by
/// name, checks [`LoopTool::supports_async`], and defers through
/// [`defer_prepared_call`].
pub struct RunAsyncTool {
    tools: Vec<Arc<dyn LoopTool>>,
    store: BackgroundStore,
    context: Context,
    config: LoopConfig,
}

impl std::fmt::Debug for RunAsyncTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunAsyncTool")
            .field(
                "tools",
                &self.tools.iter().map(|t| t.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl RunAsyncTool {
    pub fn new(
        tools: Vec<Arc<dyn LoopTool>>,
        store: BackgroundStore,
        context: Context,
        config: LoopConfig,
    ) -> Self {
        Self {
            tools,
            store,
            context,
            config,
        }
    }
}

impl LoopTool for RunAsyncTool {
    fn name(&self) -> &str {
        "run_async"
    }

    fn description(&self) -> &str {
        "Run a tool call in the background without blocking your turn. Only tools \
         that support async execution can be targets; interactive approval is not \
         available in detached calls. The result arrives automatically as a \
         <system-reminder> when it finishes (query mid-turn with task_status). \
         For long-running shell commands use bash with background=true instead."
    }

    fn label(&self) -> &str {
        "run_async"
    }

    fn parameters(&self) -> &Value {
        static PARAMS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        PARAMS.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "tool": {
                        "type": "string",
                        "description": "Name of a tool that supports background execution (e.g. \"bash_output\").",
                    },
                    "args": {
                        "type": "object",
                        "description": "Arguments for the tool, exactly as you would pass them to it directly.",
                    },
                },
                "required": ["tool", "args"]
            })
        })
    }

    fn supports_async(&self, _args: &Value) -> bool {
        false
    }

    fn execute<'a>(
        &'a self,
        tool_call_id: &'a str,
        args: Value,
        _signal: AbortSignal,
        _on_update: LoopToolUpdate,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<LoopToolResult, String>> + Send + 'a>>
    {
        Box::pin(async move {
            let tool_name = args
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let target_args = args.get("args").cloned().unwrap_or(Value::Null);

            let target = match self.tools.iter().find(|t| t.name() == tool_name) {
                Some(t) => t.clone(),
                None => {
                    let names: Vec<&str> = self.tools.iter().map(|t| t.name()).collect();
                    let mut msg = format!("run_async: tool {tool_name:?} not found");
                    if let Some(sugg) = super::suggest::closest(&tool_name, &names) {
                        msg.push_str(&format!(". Did you mean `{sugg}`?"));
                    }
                    return Err(msg);
                }
            };

            if !target.supports_async(&target_args) {
                return Err(format!(
                    "run_async: {tool_name} does not support background execution for these arguments. \
                     Run it in the foreground instead, or (for a long shell command) use \
                     bash with background=true / timeout."
                ));
            }

            if interactive_prompt_pending() {
                return Err("run_async: an interactive prompt is in flight; run the call in the foreground first".to_string());
            }

            // Prepare the target through the foreground pipeline before detaching.
            let target_call = super::tools::ToolCall {
                id: format!("{tool_call_id}/async"),
                name: tool_name.clone(),
                arguments: target_args,
            };
            let prepared = super::tools::prepare_tool_call(
                &self.context,
                &super::message::AssistantMessage::new(vec![], super::message::StopReason::ToolUse),
                &target_call,
                &self.config,
                &_signal,
            )
            .await;

            match prepared {
                super::tools::PrepareOutcome::Immediate { result, is_error } => {
                    // Preparation failed (blocked, invalid args, aborted) —
                    // surface that error directly; nothing was detached.
                    if is_error {
                        Err(super::tools::executed_result_text(&result))
                    } else {
                        Ok(result)
                    }
                }
                super::tools::PrepareOutcome::Prepared { tool, args, notes } => {
                    let call_id = target_call.id.clone();
                    let tool_for_exec = tool.clone();
                    let args_for_exec = args.clone();
                    let context_clone = self.context.clone();
                    let config_clone = self.config.clone();
                    let target_call_clone = target_call.clone();
                    let exec = Box::pin(async move {
                        let no_update: LoopToolUpdate = Arc::new(|_| {});
                        tool_for_exec
                            .execute(&call_id, args_for_exec, AbortSignal::new(), no_update)
                            .await
                    });
                    let finalize = move |outcome: super::tools::ExecutedOutcome| -> Pin<
                        Box<
                            dyn std::future::Future<Output = super::tools::FinalizedOutcome> + Send,
                        >,
                    > {
                        Box::pin(async move {
                            let mut finalized = super::tools::finalize_executed_tool_call(
                                &context_clone,
                                &super::message::AssistantMessage::new(
                                    vec![],
                                    super::message::StopReason::ToolUse,
                                ),
                                &target_call_clone,
                                &args,
                                outcome,
                                &config_clone,
                            )
                            .await;
                            super::tools::prepend_notes_to_result(&mut finalized.result, &notes);
                            finalized
                        })
                    };
                    defer_prepared_call(
                        &self.store,
                        exec,
                        finalize,
                        &format!("{tool_name} started in the background"),
                    )
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::message::{AssistantMessage, ContentBlock, StopReason};
    use crate::agent::agent_loop::tools::{
        ExecutedOutcome, FinalizedOutcome, ToolCall, create_error_tool_result,
        execute_tool_calls_sequential,
    };
    use crate::agent::agent_loop::types::{Context, LoopConfig, ToolExecutionMode};
    use crate::agent::agent_loop::{LoopEvent, inflight::InflightSet};
    use crate::agent::tools::background::TaskState;
    use tokio::sync::mpsc;

    /// Minimal async-capable tool for these tests: returns quickly with a
    /// canned payload, records the args it saw.
    struct SlowEchoTool {
        delay_ms: u64,
        seen: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl std::fmt::Debug for SlowEchoTool {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SlowEchoTool").finish()
        }
    }

    impl LoopTool for SlowEchoTool {
        fn name(&self) -> &str {
            "slow_echo"
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn label(&self) -> &str {
            "slow_echo"
        }
        fn parameters(&self) -> &Value {
            static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            P.get_or_init(|| serde_json::json!({"type": "object"}))
        }
        fn supports_async(&self, _args: &Value) -> bool {
            true
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            args: Value,
            _signal: AbortSignal,
            _on_update: LoopToolUpdate,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<LoopToolResult, String>> + Send + 'a>>
        {
            let seen = self.seen.clone();
            let delay = self.delay_ms;
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                seen.lock().unwrap().push(args.clone());
                Ok(LoopToolResult {
                    content: vec![serde_json::json!({"type":"text","text":"slow_echo done"})],
                    details: Value::Null,
                    terminate: None,
                })
            })
        }
    }

    fn test_context(tools: Vec<Arc<dyn LoopTool>>) -> Context {
        Context {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools,
        }
    }

    fn test_config() -> LoopConfig {
        LoopConfig::for_tests(Arc::new(|msgs: &[Value]| msgs.to_vec()))
    }

    #[tokio::test]
    async fn cancelled_store_aborts_handle_attached_after_session_switch() {
        let store = BackgroundStore::new();
        let id = "cancelled-before-attach";
        store.insert(id.into());
        store.cancel_all();
        let handle = tokio::spawn(std::future::pending::<()>());
        let abort = handle.abort_handle();
        store.attach_handle(id, handle);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !abort.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("late handle must be aborted");
    }

    #[tokio::test]
    async fn run_async_reports_capacity_failure_as_error() {
        let store = BackgroundStore::new();
        for i in 0..BackgroundStore::max_concurrent() {
            let id = format!("occupied-{i}");
            store.insert(id.clone());
            store.attach_handle(&id, tokio::spawn(std::future::pending()));
        }
        let target: Arc<dyn LoopTool> = Arc::new(SlowEchoTool {
            delay_ms: 0,
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let run_async: Arc<dyn LoopTool> = Arc::new(RunAsyncTool::new(
            vec![target.clone()],
            store.clone(),
            test_context(vec![target.clone()]),
            test_config(),
        ));
        let context = test_context(vec![target, run_async]);
        let assistant_msg = AssistantMessage::new(
            vec![ContentBlock::ToolCall {
                id: "call-cap".into(),
                name: "run_async".into(),
                arguments: serde_json::json!({"tool":"slow_echo","args":{}}),
                signature: None,
                signature_model: None,
            }],
            StopReason::ToolUse,
        );
        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
        let batch = execute_tool_calls_sequential(
            &context,
            &assistant_msg,
            &crate::agent::agent_loop::tools::extract_tool_calls(&assistant_msg),
            &test_config(),
            &AbortSignal::new(),
            &tx,
            &InflightSet::new(),
        )
        .await;
        assert!(batch.messages[0].is_error);
        assert_eq!(store.running_count(), BackgroundStore::max_concurrent());
        store.cancel_all();
    }

    #[tokio::test]
    async fn failed_deferred_call_notifies_failure() {
        let store = BackgroundStore::new();
        let result = defer_prepared_call(
            &store,
            Box::pin(async { Err("tool failed".to_string()) }),
            |outcome| {
                Box::pin(async move {
                    super::super::tools::finalize_executed_tool_call(
                        &test_context(vec![]),
                        &AssistantMessage::new(vec![], StopReason::ToolUse),
                        &ToolCall {
                            id: "failed".into(),
                            name: "test".into(),
                            arguments: Value::Null,
                        },
                        &Value::Null,
                        outcome,
                        &test_config(),
                    )
                    .await
                })
            },
            "failing tool",
        );
        assert!(
            super::super::tools::executed_result_text(&result.unwrap())
                .contains(ASYNC_DEFERRED_MARKER)
        );
        let notification = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(n) = store.drain_notifications().into_iter().next() {
                    break n;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            matches!(notification.state, TaskState::Failed(ref error) if error.contains("tool failed"))
        );
    }

    #[tokio::test]
    async fn run_async_defers_and_result_arrives_via_store() {
        let store = BackgroundStore::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let target: Arc<dyn LoopTool> = Arc::new(SlowEchoTool {
            delay_ms: 50,
            seen: seen.clone(),
        });
        let run_async: Arc<dyn LoopTool> = Arc::new(RunAsyncTool::new(
            vec![target.clone()],
            store.clone(),
            test_context(vec![target.clone()]),
            test_config(),
        ));
        let context = test_context(vec![target, run_async]);
        let config = test_config();
        let signal = AbortSignal::new();
        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);

        let assistant_msg = AssistantMessage::new(
            vec![ContentBlock::ToolCall {
                id: "call-1".to_string(),
                name: "run_async".to_string(),
                arguments: serde_json::json!({"tool": "slow_echo", "args": {"x": 1}}),
                signature: None,
                signature_model: None,
            }],
            StopReason::ToolUse,
        );
        let tool_calls = crate::agent::agent_loop::tools::extract_tool_calls(&assistant_msg);

        let batch = execute_tool_calls_sequential(
            &context,
            &assistant_msg,
            &tool_calls,
            &config,
            &signal,
            &tx,
            &InflightSet::new(),
        )
        .await;

        // The dispatch returns IMMEDIATELY with the deferred marker; the
        // target tool has NOT necessarily finished.
        assert_eq!(batch.messages.len(), 1);
        let text = batch.messages[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(
            text.contains(ASYNC_DEFERRED_MARKER),
            "expected deferred marker, got: {text}"
        );
        assert!(
            !batch.messages[0].is_error,
            "a successful defer must not be an error result"
        );
        assert_eq!(store.running_count(), 1, "one detached task in flight");

        // Wait for the detached task to finish and land in the store.
        let mut notified = None;
        for _ in 0..100 {
            let drained = store.drain_notifications();
            if let Some(n) = drained.into_iter().next() {
                notified = Some(n);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let n = notified.expect("detached task must notify the store");
        assert!(
            matches!(n.state, TaskState::Completed(_)),
            "got {:?}",
            n.state
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "target tool must have executed exactly once"
        );
    }

    #[tokio::test]
    async fn run_async_rejects_non_async_tool() {
        struct PlainTool;
        impl std::fmt::Debug for PlainTool {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("PlainTool").finish()
            }
        }
        impl LoopTool for PlainTool {
            fn name(&self) -> &str {
                "plain"
            }
            fn description(&self) -> &str {
                "t"
            }
            fn label(&self) -> &str {
                "plain"
            }
            fn parameters(&self) -> &Value {
                static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
                P.get_or_init(|| serde_json::json!({"type": "object"}))
            }
            // default supports_async → false
            fn execute<'a>(
                &'a self,
                _id: &'a str,
                _args: Value,
                _signal: AbortSignal,
                _on_update: LoopToolUpdate,
            ) -> Pin<
                Box<dyn std::future::Future<Output = Result<LoopToolResult, String>> + Send + 'a>,
            > {
                Box::pin(async { Ok(create_error_tool_result("unreachable")) })
            }
        }

        let store = BackgroundStore::new();
        let target: Arc<dyn LoopTool> = Arc::new(PlainTool);
        let run_async: Arc<dyn LoopTool> = Arc::new(RunAsyncTool::new(
            vec![target.clone()],
            store.clone(),
            test_context(vec![target.clone()]),
            test_config(),
        ));
        let context = test_context(vec![target, run_async]);
        let config = test_config();
        let signal = AbortSignal::new();
        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);

        let assistant_msg = AssistantMessage::new(
            vec![ContentBlock::ToolCall {
                id: "call-1".to_string(),
                name: "run_async".to_string(),
                arguments: serde_json::json!({"tool": "plain", "args": {}}),
                signature: None,
                signature_model: None,
            }],
            StopReason::ToolUse,
        );
        let tool_calls = crate::agent::agent_loop::tools::extract_tool_calls(&assistant_msg);
        let batch = execute_tool_calls_sequential(
            &context,
            &assistant_msg,
            &tool_calls,
            &config,
            &signal,
            &tx,
            &InflightSet::new(),
        )
        .await;

        assert!(batch.messages[0].is_error);
        let text = batch.messages[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(
            text.contains("does not support background execution"),
            "got: {text}"
        );
        assert_eq!(store.running_count(), 0, "nothing must be detached");
    }

    /// A tool future dropped by the watchdog on a non-async-capable tool
    /// still produces the watchdog error.
    #[tokio::test(start_paused = true)]
    async fn watchdog_still_kills_non_async_tool() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct NonAsyncSlow {
            dropped: std::sync::Arc<AtomicBool>,
        }
        impl std::fmt::Debug for NonAsyncSlow {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("NonAsyncSlow").finish()
            }
        }
        impl LoopTool for NonAsyncSlow {
            fn name(&self) -> &str {
                "non_async_slow"
            }
            fn description(&self) -> &str {
                "t"
            }
            fn label(&self) -> &str {
                "non_async_slow"
            }
            fn parameters(&self) -> &Value {
                static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
                P.get_or_init(|| serde_json::json!({"type": "object"}))
            }
            fn execute<'a>(
                &'a self,
                _id: &'a str,
                _args: Value,
                _signal: AbortSignal,
                _on_update: LoopToolUpdate,
            ) -> Pin<
                Box<dyn std::future::Future<Output = Result<LoopToolResult, String>> + Send + 'a>,
            > {
                let dropped = self.dropped.clone();
                Box::pin(async move {
                    struct G(std::sync::Arc<AtomicBool>);
                    impl Drop for G {
                        fn drop(&mut self) {
                            self.0.store(true, Ordering::SeqCst);
                        }
                    }
                    let _g = G(dropped);
                    std::future::pending::<()>().await;
                    #[allow(unreachable_code)]
                    Ok(create_error_tool_result("unreachable"))
                })
            }
        }

        let dropped = std::sync::Arc::new(AtomicBool::new(false));
        let target: Arc<dyn LoopTool> = Arc::new(NonAsyncSlow {
            dropped: dropped.clone(),
        });
        let context = test_context(vec![target]);
        let config = test_config();
        let signal = AbortSignal::new();
        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
        let assistant_msg = AssistantMessage::new(
            vec![ContentBlock::ToolCall {
                id: "call-1".to_string(),
                name: "non_async_slow".to_string(),
                arguments: serde_json::json!({}),
                signature: None,
                signature_model: None,
            }],
            StopReason::ToolUse,
        );
        let tool_calls = crate::agent::agent_loop::tools::extract_tool_calls(&assistant_msg);
        let batch = tokio::time::timeout(
            crate::timeout::Timeouts::get().tool_call + std::time::Duration::from_secs(5),
            execute_tool_calls_sequential(
                &context,
                &assistant_msg,
                &tool_calls,
                &config,
                &signal,
                &tx,
                &InflightSet::new(),
            ),
        )
        .await
        .expect("watchdog must fire within its budget");
        assert!(batch.messages[0].is_error);
        assert!(
            dropped.load(Ordering::SeqCst),
            "non-async future must be dropped"
        );
    }

    // Unused-import silencers for the test module's shared helpers that some
    // test subsets don't touch.
    #[allow(unused)]
    fn _silence(_a: ToolCall, _b: ExecutedOutcome, _c: FinalizedOutcome, _d: ToolExecutionMode) {}
}
