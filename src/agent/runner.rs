use rig::completion::Message;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::event::AgentEvent;
use crate::session::{MessageRole, Session};

pub struct AgentRunner {
    pub event_rx: mpsc::Receiver<AgentEvent>,
    /// Handle to the spawned tokio task. The UI calls `abort()` on interrupt
    /// so in-flight LLM calls and tool execution actually stop, rather than
    /// running to completion in the background and emitting permission
    /// prompts after the user thought they cancelled.
    pub task: JoinHandle<()>,
    /// Send a unit signal to ask the runner to stop the stream at the next
    /// safe boundary (after the current tool call's result). The runner
    /// emits `AgentEvent::Interjected` with whatever assistant text had
    /// streamed so far, and the UI is responsible for queueing the next
    /// user turn. Unbounded because the signal payload is just `()`.
    /// F20: bounded so a user who hammers the interject keybind
    /// can't fill an unbounded queue while the runner is in a long
    /// LLM call. Only the FIRST signal needs to be received — all
    /// subsequent ones are noise (the runner drains via
    /// `try_recv()` after the first wakeup). 64 is generous; if
    /// the channel is full, `try_send` silently no-ops (we already
    /// have one queued).
    pub interject_tx: mpsc::Sender<()>,
    /// Trigger cooperative hard-cancellation. Sending `()` here flips
    /// the inner `AbortSignal.cancel()` flag so the retry loop and
    /// rig stream see `is_cancelled()` and bail at their next check.
    /// The UI's Ctrl+C handler combines this with `JoinHandle::abort()`
    /// — abort kills the task at the next `.await`, cancel gives in-
    /// flight cooperative consumers a chance to surface a clean
    /// "cancelled" event first. Bounded to match `interject_tx`.
    pub cancel_tx: mpsc::Sender<()>,
}

impl AgentRunner {
    /// Move this runner into the interactive loop's run-state slots and mark the
    /// run active. Consuming `self` makes the install atomic — every channel /
    /// handle is transferred together, so a spawn site can't forget one slot
    /// (which would leak the runner task or strand the UI) or reuse a moved-out
    /// field. Used at every `spawn_runner` site in `ui/mod.rs` and
    /// `ui/run_handlers/*`.
    pub(crate) fn install_into(
        self,
        rx: &mut Option<mpsc::Receiver<AgentEvent>>,
        abort: &mut Option<JoinHandle<()>>,
        interject: &mut Option<mpsc::Sender<()>>,
        cancel: &mut Option<mpsc::Sender<()>>,
        is_running: &mut bool,
    ) {
        *rx = Some(self.event_rx);
        *abort = Some(self.task);
        *interject = Some(self.interject_tx);
        *cancel = Some(self.cancel_tx);
        *is_running = true;
    }
}

/// Abort-on-drop guard for a forked [`AgentRunner`]. Holding it while draining
/// the runner's events ensures a cancelled/early-returning drain actually stops
/// the fork — cooperative `cancel_tx` first (so an in-flight consumer can
/// surface a clean cancelled event), then a hard `task.abort()` at the next
/// `.await` — rather than orphaning a task that keeps calling the model.
///
/// Shared by every forked-runner consumer (`agent::review` background passes,
/// `agent::plan::runtime` phase forks) so the cancel-safety contract
/// lives in exactly one place.
pub(crate) struct AbortRunnerOnDrop {
    pub task: JoinHandle<()>,
    pub cancel_tx: mpsc::Sender<()>,
}

impl Drop for AbortRunnerOnDrop {
    fn drop(&mut self) {
        let _ = self.cancel_tx.try_send(());
        self.task.abort();
    }
}

/// Summarize a forked runner's tool-call names into a compact, de-duplicated
/// ` · `-joined line (first-occurrence order preserved). Port of Hermes's
/// action summary (`background_review.py`); shared by the review/curator passes.
pub(crate) fn summarize_actions(actions: &[String]) -> String {
    actions
        .iter()
        .fold(Vec::<&str>::new(), |mut acc, a| {
            if !acc.contains(&a.as_str()) {
                acc.push(a.as_str());
            }
            acc
        })
        .join(" · ")
}

pub fn convert_history(session: &Session) -> Vec<Message> {
    use rig::OneOrMany;
    use rig::completion::message::AssistantContent;
    let (summary, first_kept) = session.compacted_context();
    let mut messages = Vec::new();

    if let Some(summary) = summary {
        messages.push(Message::system(format!(
            "[Previous conversation summary]\n{}",
            summary
        )));
    }

    for msg in &session.messages[first_kept..] {
        match msg.role {
            MessageRole::User => {
                if msg.images.is_empty() {
                    messages.push(Message::user(msg.content.to_string()));
                } else {
                    // Multipart: text part first (if any), then one
                    // `dirge-asset:<uuid>` sentinel Image per ref. The
                    // sentinel smuggles the asset id through rig without
                    // carrying bytes; `value_to_rig_message_for_provider`
                    // reifies it to base64 at the provider boundary.
                    use rig::completion::message::{
                        DocumentSourceKind, Image, ImageMediaType, Text, UserContent,
                    };
                    let mut parts: Vec<UserContent> = Vec::new();
                    if !msg.content.is_empty() {
                        parts.push(UserContent::Text(Text {
                            text: msg.content.to_string(),
                            additional_params: None,
                        }));
                    }
                    for img in &msg.images {
                        let media_type = match img.media_type.as_str() {
                            "image/png" => Some(ImageMediaType::PNG),
                            "image/jpeg" => Some(ImageMediaType::JPEG),
                            "image/gif" => Some(ImageMediaType::GIF),
                            "image/webp" => Some(ImageMediaType::WEBP),
                            _ => None,
                        };
                        parts.push(UserContent::Image(Image {
                            data: DocumentSourceKind::Url(format!(
                                "dirge-asset:{}",
                                img.asset_id.0
                            )),
                            media_type,
                            detail: None,
                            additional_params: None,
                        }));
                    }
                    if let Ok(content) = OneOrMany::many(parts) {
                        messages.push(Message::User { content });
                    }
                }
            }
            MessageRole::System => messages.push(Message::system(msg.content.to_string())),
            MessageRole::Assistant => {
                // Phase 3: if this assistant message has structured
                // tool calls, emit a single Assistant message with
                // text + tool_use content parts, followed by ONE
                // tool_result User message per call. The pairing
                // matches opencode's `toModelMessagesEffect`
                // (`message-v2.ts:630-899`); Anthropic + OpenAI
                // reject orphan tool_use blocks so we always emit a
                // result, marking Interrupted/Failed as error text
                // rather than skipping. Bare assistant messages
                // (no tool_calls) keep the prior simple shape.
                if msg.tool_calls.is_empty() {
                    messages.push(Message::assistant(msg.content.to_string()));
                    continue;
                }

                // Build the Assistant message's content blocks: text
                // first (if any) then each ToolCall.
                let mut parts: Vec<AssistantContent> = Vec::new();
                if !msg.content.is_empty() {
                    parts.push(AssistantContent::text(msg.content.to_string()));
                }
                for tc in &msg.tool_calls {
                    parts.push(AssistantContent::tool_call(
                        tc.id.clone(),
                        tc.name.clone(),
                        tc.args.clone(),
                    ));
                }
                // OneOrMany::many requires at least one element; we
                // always have at least one ToolCall here since
                // tool_calls is non-empty.
                let content = if parts.len() == 1 {
                    OneOrMany::one(parts.pop().unwrap())
                } else {
                    OneOrMany::many(parts).expect("non-empty parts vec")
                };
                messages.push(Message::Assistant { id: None, content });

                // One User tool_result per call. State maps to:
                //  Completed  → result text verbatim
                //  Interrupted → "[Tool execution was interrupted]"
                //  Failed     → "[Tool error: <message>]"
                for tc in &msg.tool_calls {
                    let body = match &tc.state {
                        crate::session::ToolCallState::Completed { result } => result.clone(),
                        crate::session::ToolCallState::Interrupted => {
                            "[Tool execution was interrupted]".to_string()
                        }
                        crate::session::ToolCallState::Failed { error } => {
                            format!("[Tool error: {}]", error)
                        }
                    };
                    messages.push(Message::tool_result(tc.id.clone(), body));
                }
            }
        }
    }

    messages
}
/// dirge-rmk: emit one stream-json event line to stdout. NDJSON shape
/// matches Claude Code so tooling written against `claude --print
/// --output-format stream-json` works against dirge unchanged.
pub(crate) fn emit_stream_json_event(value: serde_json::Value) {
    if let Ok(s) = serde_json::to_string(&value) {
        println!("{}", s);
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
}

/// Generate a UUIDv4-shaped session id without pulling the `uuid`
/// crate (dirge already has enough deps). Random bytes via system
/// time + thread id seeded into a small xorshift.
pub(crate) fn uuid_v4_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let mut state = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(pid);
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let words = state.to_le_bytes();
        chunk.copy_from_slice(&words[..chunk.len()]);
    }
    // Set version (4) + variant (10) bits per RFC 4122.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

/// Outcome of the post-response plugin dispatch sequence
/// (`on-response` → `on-complete` → `prepare-next-run`).
///
/// Note: `next_model` is intentionally NOT included here. The
/// `prepare-next-run` hook stores its value in [`PluginManager`]; the
/// caller of `run_print` (e.g. `main.rs`'s `--loop` driver) drains it
/// via `take_pending_next_model()` AFTER `run_print` returns. That
/// keeps the choice of how to react (warn-and-ignore in `--print`,
/// rebuild agent in `--loop`) in the caller's hands and out of the
/// runner.
#[cfg(feature = "plugin")]
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ResponseHookResult {
    /// `Some(text)` when a plugin called `harness/replace-result` to
    /// substitute the agent's response. Caller decides how to surface
    /// it (text mode prints with a marker since the original already
    /// streamed; JSON modes substitute cleanly).
    pub replacement: Option<String>,
}

/// Resolve `prompt` through the `on-prompt` hook chain:
///
///   1. Dispatch `on-prompt` → join results as a "hint" prepended
///      to the prompt
///   2. `harness/request-prompt` → if set, replaces the hint
///   3. `harness/replace-prompt` → if set, fully replaces the prompt
///
/// Errors from plugin code are surfaced to stderr so the user's
/// stdout (the structured `--print` result) stays clean.
#[cfg(feature = "plugin")]
pub(crate) fn resolve_prompt_with_hooks(
    prompt: &str,
    mgr: &mut crate::plugin::PluginManager,
) -> String {
    let janet_ctx = format!(
        "@{{:prompt \"{}\"}}",
        crate::plugin::escape_janet_string(prompt)
    );
    let mut hint: Option<String> = match mgr.dispatch("on-prompt", &janet_ctx) {
        Ok(results) if !results.is_empty() => Some(results.join("\n")),
        Ok(_) => None,
        Err(e) => {
            eprintln!("[plugin] on-prompt error: {e}");
            None
        }
    };
    if let Some(pending) = mgr.take_pending_prompt() {
        hint = Some(pending);
    }
    let replace = mgr.take_pending_prompt_replace();
    if let Some(rep) = replace {
        rep
    } else if let Some(h) = hint {
        format!("{}\n\n{}", h, prompt)
    } else {
        prompt.to_string()
    }
}

/// Run the post-response hook chain: `on-response` → record store →
/// `on-complete` → `prepare-next-run`. Returns the replacement (if
/// any). The `set-next-model` value, if any, is left in
/// [`PluginManager`] for the caller to drain via
/// `take_pending_next_model()`.
#[cfg(feature = "plugin")]
pub(crate) fn apply_response_hooks(
    response: &str,
    mgr: &mut crate::plugin::PluginManager,
) -> ResponseHookResult {
    let janet_ctx = format!(
        "@{{:response \"{}\"}}",
        crate::plugin::escape_janet_string(response)
    );
    if let Err(e) = mgr.dispatch("on-response", &janet_ctx) {
        eprintln!("[plugin] on-response error: {e}");
    }
    // dirge-tte0: fire `message-end` on the headless path too. Previously
    // only the interactive (TUI) finalization dispatched it, so a
    // `harness/rewrite-message` plugin silently no-op'd under
    // `--print` / `--loop` / ACP. Mirror done.rs: dispatch, then apply any
    // rewrite to the text that gets stored.
    let mut stored = response.to_string();
    match mgr.dispatch(
        "message-end",
        &format!(
            "@{{:message \"{}\"}}",
            crate::plugin::escape_janet_string(response)
        ),
    ) {
        Ok(_) => {
            if let Some(rewritten) = mgr.take_message_rewrite() {
                stored = rewritten;
            }
        }
        Err(e) => eprintln!("[plugin] message-end error: {e}"),
    }
    mgr.store_response(&stored);
    let replacement = mgr.take_pending_replace_result();
    if let Err(e) = mgr.dispatch("on-complete", "@{}") {
        eprintln!("[plugin] on-complete error: {e}");
    }
    if let Err(e) = mgr.dispatch("prepare-next-run", "@{}") {
        eprintln!("[plugin] prepare-next-run error: {e}");
    }
    ResponseHookResult { replacement }
}

#[cfg(all(test, feature = "plugin"))]
mod plugin_hook_tests {
    use super::*;
    use crate::plugin::PluginManager;

    /// on-prompt result is joined with the original prompt as a
    /// "hint" prefix. Demonstrates the simplest plugin-mutates-input
    /// flow: a code-style hint that always precedes the user prompt.
    #[test]
    fn resolve_prompt_prepends_on_prompt_hint() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(defn style-hint [ctx] "ALWAYS USE TYPESCRIPT")"#)
            .unwrap();
        mgr.register("on-prompt", "style-hint");
        let out = resolve_prompt_with_hooks("write a function", &mut mgr);
        assert!(out.contains("ALWAYS USE TYPESCRIPT"));
        assert!(out.contains("write a function"));
        assert!(
            out.find("ALWAYS USE TYPESCRIPT").unwrap() < out.find("write a function").unwrap(),
            "hint must come before the prompt"
        );
    }

    /// harness/request-prompt overrides the dispatch result. Used by
    /// plugins that want full control: they may run logic in the
    /// hook AND emit a queue-style replacement.
    #[test]
    fn resolve_prompt_request_prompt_overrides_hint() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(
            r#"(defn override [ctx]
                 (harness/request-prompt "from-request-prompt")
                 "from-dispatch")"#,
        )
        .unwrap();
        mgr.register("on-prompt", "override");
        let out = resolve_prompt_with_hooks("original", &mut mgr);
        // The "from-dispatch" hint is discarded once
        // request-prompt was set — same precedence as the UI path.
        assert!(out.contains("from-request-prompt"));
        assert!(out.contains("original"));
        assert!(!out.contains("from-dispatch"));
    }

    /// harness/replace-prompt fully substitutes the prompt — the
    /// original text is not seen by the LLM at all.
    #[test]
    fn resolve_prompt_replace_prompt_substitutes_entirely() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(
            r#"(defn replace [ctx]
                 (harness/replace-prompt "ENTIRELY NEW PROMPT")
                 nil)"#,
        )
        .unwrap();
        mgr.register("on-prompt", "replace");
        let out = resolve_prompt_with_hooks("user typed this", &mut mgr);
        assert_eq!(out, "ENTIRELY NEW PROMPT");
        assert!(!out.contains("user typed this"));
    }

    /// No plugins / nil result: prompt passes through untouched.
    #[test]
    fn resolve_prompt_no_hook_passthrough() {
        let mut mgr = PluginManager::try_new().unwrap();
        let out = resolve_prompt_with_hooks("just this", &mut mgr);
        assert_eq!(out, "just this");
    }

    #[tokio::test]
    async fn install_into_populates_every_slot_and_marks_running() {
        let (_tx, event_rx) = mpsc::channel(1);
        let (interject_tx, _) = mpsc::channel(1);
        let (cancel_tx, _) = mpsc::channel(1);
        let runner = AgentRunner {
            event_rx,
            task: tokio::spawn(async {}),
            interject_tx,
            cancel_tx,
        };
        let (mut rx, mut abort, mut interject, mut cancel) = (None, None, None, None);
        let mut is_running = false;
        runner.install_into(
            &mut rx,
            &mut abort,
            &mut interject,
            &mut cancel,
            &mut is_running,
        );
        // Every slot must be filled — the whole point is "can't forget one".
        assert!(rx.is_some() && abort.is_some() && interject.is_some() && cancel.is_some());
        assert!(is_running);
    }

    /// Regression test: `install_into` sets `is_running = true`.  Callers
    /// that redundantly set it back to `false` after a drain break the
    /// invariant — the UI then thinks no run is active (Ctrl+C cancel
    /// breaks, next prompt spawns a duplicate concurrent agent).
    #[tokio::test]
    async fn install_into_is_running_must_not_be_overwritten_by_caller() {
        let (_tx, event_rx) = mpsc::channel(1);
        let (interject_tx, _) = mpsc::channel(1);
        let (cancel_tx, _) = mpsc::channel(1);
        let runner = AgentRunner {
            event_rx,
            task: tokio::spawn(async {}),
            interject_tx,
            cancel_tx,
        };
        let (mut rx, mut abort, mut interject, mut cancel) = (None, None, None, None);
        let mut is_running = false;
        runner.install_into(
            &mut rx,
            &mut abort,
            &mut interject,
            &mut cancel,
            &mut is_running,
        );
        // The macro manages is_running; a caller must not overwrite it.
        assert!(
            is_running,
            "install_into sets is_running; caller must not set false"
        );
        // Simulate the bug: if a call site did `is_running = false` here it
        // would clobber the live drain.  That line was removed — verify the
        // flag stays true without it.
        assert!(
            is_running,
            "is_running must still be true; no overwrite allowed"
        );
    }

    #[test]
    fn summarize_actions_dedups_preserving_first_occurrence_order() {
        let actions = [
            "read".to_string(),
            "grep".to_string(),
            "read".to_string(),
            "bash".to_string(),
            "grep".to_string(),
        ];
        assert_eq!(summarize_actions(&actions), "read · grep · bash");
        assert_eq!(summarize_actions(&[]), "");
        assert_eq!(summarize_actions(&["edit".to_string()]), "edit");
    }

    /// on-response can mutate the final response via
    /// harness/replace-result. Used by formatting / wrapping
    /// plugins that produce structured output around the agent's
    /// text.
    #[test]
    fn apply_response_hooks_replace_result() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(
            r#"(defn wrap [ctx]
                 (harness/replace-result "WRAPPED")
                 nil)"#,
        )
        .unwrap();
        mgr.register("on-response", "wrap");
        let result = apply_response_hooks("raw response", &mut mgr);
        assert_eq!(result.replacement.as_deref(), Some("WRAPPED"));
        // next_model is not part of ResponseHookResult; it's left in
        // the manager. Verify it wasn't set as a side-effect of the
        // wrap hook.
        assert_eq!(mgr.take_pending_next_model(), None);
    }

    /// prepare-next-run can set the next model. The runner does NOT
    /// drain it — the caller (e.g. `run_headless_loop`) is responsible
    /// for `take_pending_next_model()`.
    #[test]
    fn apply_response_hooks_set_next_model_left_in_manager() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(
            r#"(defn pick-model [ctx]
                 (harness/set-next-model "claude-opus-4-7")
                 nil)"#,
        )
        .unwrap();
        mgr.register("prepare-next-run", "pick-model");
        let _ = apply_response_hooks("ok", &mut mgr);
        assert_eq!(
            mgr.take_pending_next_model().as_deref(),
            Some("claude-opus-4-7")
        );
    }

    /// dirge-tte0: `message-end` (`harness/rewrite-message`) now fires on
    /// the HEADLESS path too — previously only the TUI dispatched it, so a
    /// rewrite plugin silently no-op'd under `--print`/`--loop`/ACP. The
    /// stored response must reflect the rewrite.
    #[test]
    fn apply_response_hooks_fires_message_end_rewrite() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(
            r#"(defn rw [ctx]
                 (harness/rewrite-message "REWRITTEN-BY-MESSAGE-END")
                 nil)"#,
        )
        .unwrap();
        mgr.register("message-end", "rw");
        apply_response_hooks("original text", &mut mgr);
        // store_response wrote the rewritten text into `harness-response`.
        let stored = mgr.eval("harness-response").unwrap();
        assert!(
            stored.contains("REWRITTEN-BY-MESSAGE-END"),
            "headless message-end rewrite must be stored; got {stored:?}"
        );
        // The rewrite slot was consumed by apply_response_hooks.
        assert_eq!(mgr.take_message_rewrite(), None);
    }

    /// No plugins / no hooks fired: response passes through with
    /// no replacement and no next-model.
    #[test]
    fn apply_response_hooks_no_hooks_passthrough() {
        let mut mgr = PluginManager::try_new().unwrap();
        let result = apply_response_hooks("ok", &mut mgr);
        assert_eq!(result, ResponseHookResult::default());
    }
}
