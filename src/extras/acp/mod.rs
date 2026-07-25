pub mod config;

use std::sync::Arc;

use agent_client_protocol::schema::*;
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Responder, Stdio};
use agent_client_protocol::{on_receive_notification, on_receive_request};

use crate::cli::Cli;
use crate::config::{Config, ProviderEntry};
use crate::context::ContextFiles;
use crate::event::AgentEvent;
use crate::permission::ask::AskSender;
use crate::permission::checker::{PermCheck, PermissionChecker};
use crate::permission::{PermissionConfig, SecurityMode};
use crate::provider::{ModelSwitch, resolve_model_switch};
use crate::sandbox::Sandbox;
use crate::session::{MessageRole, Session, ToolCallEntry, ToolCallState};
use compact_str::CompactString;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Per-`SessionId` conversation state (dirge-5wqc). Before this the ACP bridge
/// was stateless: every prompt built a fresh agent with an empty history, so a
/// multi-turn editor conversation lost all context each turn, and there was no
/// handle to cancel an in-flight run.
struct AcpSession {
    /// Accumulated conversation. `convert_history` turns it into the rig
    /// history fed to `spawn_runner` on the next prompt.
    session: Session,
    /// The client-declared working directory for this session. `/cd` updates
    /// it; `run_prompt` chdirs here before each turn.
    cwd: PathBuf,
    /// Session-scoped `/model` override (ACP slash command). `run_prompt`
    /// prefers these over the CLI/config default so a switch persists across
    /// turns in this session. Both stay `None` until the client runs
    /// `/model <id>`; `provider_override` is set only on a cross-provider
    /// switch (a same-provider swap keeps the current provider).
    model_override: Option<CompactString>,
    provider_override: Option<CompactString>,
    /// Session-scoped `/mode` override (ACP slash command). `None` falls back
    /// to the CLI/config-resolved security mode.
    mode_override: Option<SecurityMode>,
    /// Abort/cancel handles for the currently-running prompt, if any.
    run: Option<AcpRun>,
}

/// Monotonic source for [`AcpRun::generation`] stamps.
static NEXT_RUN_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Cancellation handles for one in-flight prompt run.
struct AcpRun {
    /// Which registration this run belongs to, stamped by [`register_run`].
    /// `finish_turn` only clears the session's run slot when the finishing
    /// turn's generation matches, so an aborted run (replaced by an
    /// overlapping prompt on the same session) can't null out its
    /// replacement's cancel handles on the way down.
    generation: u64,
    /// The runner task — `abort()` stops it at the next await point.
    task: tokio::task::JoinHandle<()>,
    /// Cooperative cancel: flips the runner's `AbortSignal` so in-flight LLM
    /// calls / tools bail at their next check.
    cancel_tx: tokio::sync::mpsc::Sender<()>,
    /// Set by [`cancel_run`] so the streaming loop reports `Cancelled` rather
    /// than `EndTurn` once the aborted runner's event channel closes.
    cancelled: Arc<AtomicBool>,
}

type SessionMap = tokio::sync::Mutex<HashMap<String, AcpSession>>;

/// The rig history for a session's prior turns (empty if the session is unknown
/// — a prompt without a preceding `new_session`), plus the session's cwd.
async fn history_and_cwd(
    sessions: &SessionMap,
    id: &str,
) -> (Vec<rig::completion::Message>, Option<PathBuf>) {
    let map = sessions.lock().await;
    match map.get(id) {
        Some(s) => (
            crate::agent::runner::convert_history(&s.session),
            Some(s.cwd.clone()),
        ),
        None => (Vec::new(), None),
    }
}

/// Register the in-flight run so `session/cancel` can reach it. Creates a
/// default session entry if the client skipped `new_session`. Aborts any run
/// it replaces (editors serialize turns, so this is belt-and-suspenders).
/// Returns the generation stamped onto the run; the caller passes it back to
/// [`finish_turn`] so a replaced run can't clear its replacement's slot.
async fn register_run(
    sessions: &SessionMap,
    id: &str,
    provider: &str,
    model: &str,
    mut run: AcpRun,
) -> u64 {
    let generation = NEXT_RUN_GENERATION.fetch_add(1, Ordering::Relaxed);
    run.generation = generation;
    let mut map = sessions.lock().await;
    let entry = map.entry(id.to_string()).or_insert_with(|| AcpSession {
        session: Session::new(provider, model, 0),
        cwd: std::env::current_dir().unwrap_or_default(),
        model_override: None,
        provider_override: None,
        mode_override: None,
        run: None,
    });
    if let Some(prev) = entry.run.replace(run) {
        prev.task.abort();
    }
    generation
}

/// Append a completed turn — the user prompt plus the assistant's full-fidelity
/// reply (text + tool calls) — to the session so the next prompt resumes with
/// context. Mirrors the headless `--session` persistence in `main.rs`. Clears
/// the run handle — but only if the stored run still belongs to this turn
/// (`generation` matches). An overlapping prompt on the same session replaces
/// the run via `register_run` and aborts the old task; the aborted turn's loop
/// still exits through here, and without the guard it would null out the
/// replacement's cancel handles, making the new run uncancellable
/// (dirge-5wqc regression, overlap case).
async fn finish_turn(
    sessions: &SessionMap,
    id: &str,
    generation: u64,
    prompt: &str,
    response: &str,
    tool_calls: Vec<ToolCallEntry>,
) {
    let mut map = sessions.lock().await;
    if let Some(s) = map.get_mut(id) {
        s.session.add_message(MessageRole::User, prompt);
        s.session
            .add_message_with_tool_calls(MessageRole::Assistant, response, tool_calls);
        if s.run.as_ref().is_some_and(|r| r.generation == generation) {
            s.run = None;
        }
    }
}

/// Abort the in-flight run for `id`, if any (the ACP `session/cancel` handler).
/// Flags cancellation BEFORE aborting so the streaming loop observes it, sends
/// the cooperative cancel, then hard-aborts the task. Returns whether a run was
/// actually cancelled.
async fn cancel_run(sessions: &SessionMap, id: &str) -> bool {
    let mut map = sessions.lock().await;
    let Some(s) = map.get_mut(id) else {
        return false;
    };
    if let Some(run) = s.run.take() {
        run.cancelled.store(true, Ordering::SeqCst);
        let _ = run.cancel_tx.try_send(());
        run.task.abort();
        true
    } else {
        false
    }
}

struct AcpState {
    cli: Cli,
    cfg: Config,
    context: ContextFiles,
    sessions: SessionMap,
}

pub async fn serve(cli: Cli, cfg: Config, context: ContextFiles) -> anyhow::Result<()> {
    let state = Arc::new(AcpState {
        cli,
        cfg,
        context,
        sessions: tokio::sync::Mutex::new(HashMap::new()),
    });

    Agent
        .builder()
        .name("dirge")
        .on_receive_request(
            {
                let state = state.clone();
                move |req: InitializeRequest, responder, _cx| {
                    let state = state.clone();
                    async move { handle_initialize(req, responder, &state).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                move |req: NewSessionRequest, responder, cx| {
                    let state = state.clone();
                    async move { handle_new_session(req, responder, cx, &state).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                move |req: PromptRequest, responder, cx| {
                    let state = state.clone();
                    async move { handle_prompt(req, responder, cx, state).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            {
                let state = state.clone();
                move |notif: CancelNotification, _cx| {
                    let state = state.clone();
                    async move {
                        // dirge-5wqc: the editor's cancel button sends
                        // `session/cancel`. Previously the catch-all answered
                        // "Unhandled" while the runner kept executing. Abort the
                        // in-flight run for this session instead.
                        let id = notif.session_id.to_string();
                        let cancelled = cancel_run(&state.sessions, &id).await;
                        tracing::info!("ACP session/cancel for {} (cancelled={})", id, cancelled);
                        Ok(())
                    }
                }
            },
            on_receive_notification!(),
        )
        .on_receive_dispatch(
            |dispatch: Dispatch<AgentRequest, AgentNotification>, cx: ConnectionTo<Client>| {
                async move {
                    dispatch.respond_with_error(
                        agent_client_protocol::util::internal_error("Unhandled ACP message"),
                        cx,
                    )
                }
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new())
        .await
        .map_err(|e| anyhow::anyhow!("ACP server error: {}", e))?;

    Ok(())
}

async fn handle_initialize(
    req: InitializeRequest,
    responder: Responder<InitializeResponse>,
    state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let _ = state;

    let caps = AgentCapabilities::new();

    let resp = InitializeResponse::new(req.protocol_version)
        .agent_capabilities(caps)
        .agent_info(Implementation::new("dirge", "1.0.4"));

    responder.respond(resp)
}

async fn handle_new_session(
    req: NewSessionRequest,
    responder: Responder<NewSessionResponse>,
    cx: ConnectionTo<Client>,
    state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());

    tracing::info!(
        "ACP new session: {} (cwd: {})",
        session_id,
        req.cwd.display()
    );

    // dirge-5wqc: record the session so its conversation history accumulates
    // across prompts (and store the client's cwd instead of dropping it).
    let provider = state.cli.resolve_provider(&state.cfg);
    let model = state.cli.resolve_model(&state.cfg).to_string();
    state.sessions.lock().await.insert(
        session_id.to_string(),
        AcpSession {
            session: Session::new(&provider, &model, 0),
            cwd: req.cwd.clone(),
            model_override: None,
            provider_override: None,
            mode_override: None,
            run: None,
        },
    );

    let resp = NewSessionResponse::new(session_id.clone());
    responder.respond(resp)?;

    // dirge-32k9 (gh#714): announce the slash commands so ACP clients (Zed, etc.) can
    // offer them in their command palette. The spec expects this after
    // session creation; each invoked command comes back as ordinary prompt
    // text, which `run_prompt` intercepts. Fire-and-forget: a failed
    // notification just means no palette entries, not a broken session.
    let notif = SessionNotification::new(
        session_id,
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
            acp_available_commands(),
        )),
    );
    let _ = cx.send_notification(notif);
    Ok(())
}

async fn handle_prompt(
    req: PromptRequest,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
    state: Arc<AcpState>,
) -> Result<(), agent_client_protocol::Error> {
    let session_id = req.session_id.clone();

    tracing::info!("ACP prompt for session {}", session_id);

    let prompt_text = req
        .prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    cx.spawn({
        let cx = cx.clone();
        async move { run_prompt(&state, &prompt_text, session_id, responder, cx).await }
    })
}

async fn run_prompt(
    state: &AcpState,
    prompt_text: &str,
    session_id: SessionId,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), agent_client_protocol::Error> {
    let id_key = session_id.to_string();

    // dirge-32k9 (gh#714): session-scoped `/model` and `/mode` overrides take precedence
    // over the CLI/config resolution so a slash-command switch persists across
    // turns in this ACP session.
    let (model_override, provider_override, mode_override) =
        session_overrides(&state.sessions, &id_key).await;

    let provider_str = provider_override
        .clone()
        .unwrap_or_else(|| state.cli.resolve_provider(&state.cfg));
    let (model_str, model_explicit) = if let Some(m) = model_override.clone() {
        // A `/model` switch is an explicit choice — skip the alias-default and
        // Codex-default substitutions the default path applies below.
        (m, true)
    } else {
        let config_model = state
            .cfg
            .resolve_role(crate::config::ConfigRole::Default)
            .and_then(|(_, e)| e.model);
        let model_explicit = state.cli.model.is_some() || config_model.is_some();
        let model_str = if !model_explicit {
            // dirge-j3jd: resolve the alias's provider TYPE so a custom alias
            // doesn't fall back to the OpenRouter default model id.
            CompactString::new(crate::provider::default_model_for_alias(
                &provider_str,
                &state.cfg.providers_map(),
            ))
        } else {
            state.cli.resolve_model(&state.cfg)
        };
        (model_str, model_explicit)
    };

    let current_mode = resolve_acp_mode(&state.cli, &state.cfg, mode_override);

    // dirge-32k9 (gh#714): intercept the curated ACP slash commands. They run locally and
    // never reach the LLM — mirroring the interactive UI, where slash commands
    // don't enter the conversation. Unrecognized `/...` prompts fall through to
    // the model unchanged.
    if let Some((cmd, args)) = parse_acp_slash(prompt_text)
        && let Some(reply) = handle_acp_slash(
            &state.sessions,
            &state.cfg,
            &id_key,
            cmd,
            args,
            AcpCurrent {
                provider: &provider_str,
                model: &model_str,
                mode: current_mode,
            },
        )
        .await
    {
        let _ = cx.send_notification(agent_text_chunk(&session_id, reply));
        let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
        return Ok(());
    }

    let client = create_acp_client(&provider_str, &state.cfg)
        .map_err(|e| agent_client_protocol::Error::new(-32603, e.to_string()))?;

    // dirge-ovjk: apply the Codex-default substitution only for a defaulted
    // model, so an explicit choice is honored here the same as the main path.
    let model_str = crate::provider::resolve_model_name(&client, &model_str, model_explicit);
    let model = client.completion_model(model_str.clone());

    let (permission, ask_tx) = build_acp_permission(state, current_mode);
    // Adversarial-review finding #2: ACP used to build its checker
    // and never install the active prompt's `deny_tools` list. Plan
    // mode (or any frontmatter deny) was a no-op for editor-side
    // clients — the LLM could edit/write/bash unrestricted even
    // though the prompt forbade it. Mirror the
    // `main.rs::build_channels`-followed-by-`apply_prompt_deny`
    // sequence here so ACP sessions get the same permission model
    // as the interactive UI.
    crate::permission::apply_prompt_deny(&permission, &state.context.current_prompt_deny_tools);
    let sandbox = Sandbox::new(state.cli.resolve_sandbox(&state.cfg));

    // Audit H16: ACP path used to pass `None` for `bg_store`, so the
    // `task` tool's `background=true` path silently degraded — the
    // store insert was skipped, the spawned subagent ran but had
    // nowhere to deposit its result for the next turn. Provide a
    // real store. No UI sink (ACP renders via its own protocol),
    // so the lifecycle events drop on the floor; the LLM-side
    // pending-notification mechanism still works.
    let bg_store = crate::agent::tools::background::BackgroundStore::new();
    let agent = crate::provider::build_agent(
        model,
        &state.cli,
        &state.cfg,
        &state.context,
        permission,
        ask_tx,
        None,
        None,
        Some(bg_store),
        #[cfg(feature = "lsp")]
        None,
        sandbox,
        #[cfg(feature = "mcp")]
        None::<&crate::extras::mcp::McpClientManager>,
        #[cfg(feature = "semantic")]
        None::<&crate::semantic::SemanticManager>,
        // dirge-502b: ACP sessions identify themselves via the ACP
        // protocol's own SessionId. Pass it through so the
        // SessionSearchTool excludes the live session from its
        // own search results.
        Some(session_id.to_string()),
    )
    .await;

    // dirge-5wqc: resume the session's prior turns so the conversation keeps
    // context across prompts, and honor the client-declared cwd. The cwd set is
    // best-effort: the process working directory is global, so two sessions in
    // different directories would race it — but editors drive one turn at a
    // time, so in practice a session always runs in its own directory.
    let (history, cwd) = history_and_cwd(&state.sessions, &id_key).await;
    if let Some(cwd) = cwd
        && cwd.is_dir()
    {
        let _ = std::env::set_current_dir(&cwd);
    }

    let runner = agent.spawn_runner(
        crate::provider::Prompt::text(prompt_text.to_string()),
        history,
        None,
        None,
    );
    let mut rx = runner.event_rx;

    // dirge-5wqc: register the in-flight run so `session/cancel` can abort it.
    let cancelled = Arc::new(AtomicBool::new(false));
    let generation = register_run(
        &state.sessions,
        &id_key,
        &provider_str,
        &model_str,
        AcpRun {
            generation: 0, // stamped by register_run
            task: runner.task,
            cancel_tx: runner.cancel_tx,
            cancelled: cancelled.clone(),
        },
    )
    .await;

    // Accumulate the assistant turn (text + tool calls) so `finish_turn`
    // persists a full-fidelity reply into the session history — mirrors the
    // headless loop in `provider/run.rs`.
    let mut full_response = String::new();
    let mut turn_tool_calls: Vec<ToolCallEntry> = Vec::new();

    // F5: correlate rig tool-call ids with ACP ids so parallel
    // calls pair with their results correctly. See
    // `ToolCallCorrelator` doc for the dual-mode logic.
    let mut correlator = ToolCallCorrelator::default();
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Token(text) => {
                full_response.push_str(&text);
                let chunk =
                    ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())));
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentMessageChunk(chunk),
                );
                let _ = cx.send_notification(notif);
            }
            AgentEvent::Reasoning(text) => {
                let chunk =
                    ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())));
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentThoughtChunk(chunk),
                );
                let _ = cx.send_notification(notif);
            }
            AgentEvent::ToolCall { id, name, args } => {
                // Record for history; the matching ToolResult flips it to
                // Completed. An unanswered call stays Interrupted so
                // convert_history re-emits it and the model sees no orphan.
                turn_tool_calls.push(ToolCallEntry {
                    id: id.to_string(),
                    name: name.to_string(),
                    args: args.clone(),
                    state: ToolCallState::Interrupted,
                });
                let args_str = args.to_string();
                let acp_id = ToolCallId::new(uuid::Uuid::new_v4().to_string());
                correlator.record(id.as_str(), acp_id.clone());
                let tool_call = ToolCall::new(acp_id, name.to_string())
                    .raw_input(serde_json::from_str(&args_str).ok());
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::ToolCall(tool_call),
                );
                let _ = cx.send_notification(notif);
            }
            AgentEvent::ToolStarted { id } => {
                // Surface the pending → in_progress transition the
                // ACP protocol expects between the initial ToolCall
                // and the eventual completion. Previously dirge
                // skipped this transition; consumers had no way to
                // distinguish "queued" from "running".
                if let Some(acp_id) = correlator.resolve(id.as_str()) {
                    let fields = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
                    let update = ToolCallUpdate::new(acp_id, fields);
                    let notif = SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::ToolCallUpdate(update),
                    );
                    let _ = cx.send_notification(notif);
                }
            }
            AgentEvent::ToolResult { id, output, kind } => {
                // Flip the accumulated call to Completed for history. Match by
                // rig id, or fall back to the most recent Interrupted call for
                // providers that don't emit ids (same as provider/run.rs).
                let rig_id = id.as_str();
                let target = if !rig_id.is_empty() {
                    turn_tool_calls.iter_mut().rev().find(|e| e.id == rig_id)
                } else {
                    turn_tool_calls
                        .iter_mut()
                        .rev()
                        .find(|e| matches!(e.state, ToolCallState::Interrupted))
                };
                if let Some(entry) = target {
                    entry.state = ToolCallState::Completed {
                        result: output.to_string(),
                    };
                }
                let id = correlator
                    .resolve(id.as_str())
                    .unwrap_or_else(|| ToolCallId::new(String::new()));
                // `kind == File` could become a `ResourceLink`
                // ContentBlock in a follow-up; for now both
                // variants surface as TextContent so the LLM /
                // ACP client behavior is unchanged. The
                // classification just flows through the event for
                // future consumers.
                let _ = kind;
                let fields = ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(output.to_string()),
                    ))]);
                let update = ToolCallUpdate::new(id, fields);
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::ToolCallUpdate(update),
                );
                let _ = cx.send_notification(notif);
            }
            AgentEvent::Done { response, .. } => {
                // `Done.response` is the authoritative full text.
                full_response = response.to_string();
                break;
            }
            AgentEvent::Error(error) => {
                // dirge-6po9: don't swallow the error and report a clean
                // EndTurn — the editor client would see a truncated/empty
                // turn with no diagnostic. Surface the text as a chunk
                // before breaking, same shape as ContextOverflow below, so
                // a provider auth failure / network error / tool crash is
                // visible in the client.
                let _ =
                    cx.send_notification(diagnostic_chunk(&session_id, format!("error: {error}")));
                break;
            }
            AgentEvent::ContextOverflow { error, .. } => {
                // ACP has no auto-compact-and-respawn flow — the
                // client (editor) owns submission and would re-issue
                // on its own. Surface the friendly error and end
                // the stream, same shape as `Error`. Same warning:
                // the original prompt isn't lost (ACP keeps its own
                // history), but auto-recovery is interactive-only.
                let _ = cx.send_notification(diagnostic_chunk(
                    &session_id,
                    format!("context overflow: {error}"),
                ));
                break;
            }
            // Observability markers added for the interactive UI's
            // turn tracker + interjection queue. ACP doesn't have a
            // mid-stream interjection concept (the client owns
            // submission), so we treat these as no-ops.
            AgentEvent::CustomMessage { .. } => {
                // Plugin-emitted custom message. ACP doesn't have a
                // first-class concept for these; the interactive UI
                // is the renderer. Drop on the ACP path so a Custom
                // message from a plugin doesn't break the structured
                // stream.
            }
            AgentEvent::TurnStart { .. }
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::Usage { .. }
            | AgentEvent::CompactionStarted { .. }
            | AgentEvent::ContextCompacted { .. }
            | AgentEvent::CheckpointRefresh { .. }
            | AgentEvent::RetryNotice { .. }
            | AgentEvent::SystemNotice { .. }
            | AgentEvent::RepairStats { .. }
            | AgentEvent::EscalationActivated { .. } => {
                // Observability markers — no ACP-protocol
                // equivalent. The interactive UI is the only
                // renderer for these; drop on the ACP path so
                // they don't perturb the structured stream.
            }
            AgentEvent::UserMessage { .. } => {
                // Steering-injected user message mid-run — ACP
                // doesn't support mid-stream interjection; drop.
            }
            AgentEvent::Interjected { .. } => {
                // An interjected turn shouldn't reach the ACP bridge —
                // ACP runs aren't interactive — but bail cleanly if
                // one does rather than panic on partial state.
                break;
            } // (ContextOverflow is handled higher up with the
              // `{ error, .. }` binding that formats the friendly
              // error into a SessionUpdate before breaking. PR #127
              // briefly added a second `{ .. }` catch-all here that
              // was unreachable — removed.)
        }
    }

    // dirge-5wqc: persist the turn (user prompt + assistant reply w/ tool
    // calls) so the NEXT prompt in this session resumes with full context.
    // Runs even on cancel/error so a partial turn stays in history rather than
    // vanishing — matching the interactive UI's interrupt behavior.
    finish_turn(
        &state.sessions,
        &id_key,
        generation,
        prompt_text,
        &full_response,
        turn_tool_calls,
    )
    .await;

    // The ACP spec requires `Cancelled` when the client sent `session/cancel`,
    // even though the aborted runner just closed its channel without a `Done`.
    let reason = if cancelled.load(Ordering::SeqCst) {
        StopReason::Cancelled
    } else {
        StopReason::EndTurn
    };
    let _ = responder.respond(PromptResponse::new(reason));
    Ok(())
}

/// Wrap plain text as an `AgentMessageChunk` session notification. The base
/// for surfacing any agent-authored text over ACP — diagnostics and the
/// locally-handled slash-command replies both use it.
fn agent_text_chunk(session_id: &SessionId, text: String) -> SessionNotification {
    SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        )))),
    )
}

/// dirge-6po9: build the agent-message chunk that surfaces a run-ending
/// diagnostic (a transient/auth/tool error, or a context overflow) to the
/// ACP client. Without it the `Error` arm broke silently and the turn
/// reported a clean `EndTurn` with truncated content and no reason. Shared
/// by the `Error` and `ContextOverflow` arms so they stay the same shape.
fn diagnostic_chunk(session_id: &SessionId, text: String) -> SessionNotification {
    agent_text_chunk(session_id, text)
}

/// dirge-32k9 (gh#714): the curated slash commands dirge exposes over ACP. Unlike the
/// full interactive set, these are the ones with a meaningful non-interactive
/// behavior an editor client can drive; TUI-only commands (`/panel`, `/tree`,
/// `/fork`, …) are deliberately omitted. Tuple is `(name, description, input
/// hint)`; names carry NO leading `/` — the ACP client adds that affordance.
/// The single source for both the advertisement and the dispatcher below, so
/// they can't drift.
const ACP_COMMANDS: &[(&str, &str, Option<&str>)] = &[
    ("help", "list dirge's ACP slash commands", None),
    ("clear", "clear this session's conversation history", None),
    (
        "cd",
        "change the session working directory",
        Some("directory path"),
    ),
    (
        "model",
        "show the current model, or switch to one",
        Some("model id (optional)"),
    ),
    (
        "mode",
        "view or set the permission mode",
        Some("standard | restrictive | accept | yolo"),
    ),
];

/// Build the ACP `AvailableCommand` list from [`ACP_COMMANDS`].
fn acp_available_commands() -> Vec<AvailableCommand> {
    ACP_COMMANDS
        .iter()
        .map(|(name, desc, hint)| {
            let cmd = AvailableCommand::new(*name, *desc);
            match hint {
                Some(h) => cmd.input(AvailableCommandInput::Unstructured(
                    UnstructuredCommandInput::new(*h),
                )),
                None => cmd,
            }
        })
        .collect()
}

/// Render the `/help` reply — the same curated list, as plain text.
fn acp_help_text() -> String {
    let mut out = String::from("dirge slash commands:\n");
    for (name, desc, _) in ACP_COMMANDS {
        out.push_str(&format!("  /{name}  —  {desc}\n"));
    }
    out.push_str("\nother `/...` input is sent to the model as-is.");
    out
}

/// Split a leading-slash prompt into `(command, args)` with the `/` stripped
/// from the command. Returns `None` when the prompt isn't a slash command (no
/// leading `/`, a bare `/`, or whitespace right after the slash). The command
/// is the first whitespace-delimited token; args is the trimmed remainder.
/// WHICH commands are actually handled is decided by [`handle_acp_slash`].
fn parse_acp_slash(prompt: &str) -> Option<(&str, &str)> {
    let rest = prompt.trim().strip_prefix('/')?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(match rest.split_once(char::is_whitespace) {
        Some((cmd, args)) => (cmd, args.trim()),
        None => (rest, ""),
    })
}

/// The provider/model/mode a slash command should treat as "current" — the
/// values `run_prompt` resolved for this turn (session overrides already
/// applied). Bundled so [`handle_acp_slash`] stays under the arg-count limit.
struct AcpCurrent<'a> {
    provider: &'a str,
    model: &'a str,
    mode: SecurityMode,
}

/// Dispatch a parsed slash command against the ACP session state. Returns
/// `Some(reply)` for a handled command (the caller streams `reply` back as an
/// agent message and ends the turn), or `None` for a command outside the
/// curated set (the caller forwards the original prompt to the model).
async fn handle_acp_slash(
    sessions: &SessionMap,
    cfg: &Config,
    id: &str,
    cmd: &str,
    args: &str,
    current: AcpCurrent<'_>,
) -> Option<String> {
    match cmd {
        "help" => Some(acp_help_text()),
        "clear" => {
            let mut map = sessions.lock().await;
            if let Some(s) = map.get_mut(id) {
                // Mirror the interactive `/clear`, minus the TUI/board bits:
                // wipe the conversation state so the next prompt starts fresh.
                s.session.messages.clear();
                s.session.total_estimated_tokens = 0;
                s.session.compactions.clear();
                s.session.message_store.clear();
                s.session.tree.entries.clear();
                s.session.tree.leaf_id = None;
            }
            Some("conversation history cleared".to_string())
        }
        "cd" => Some(acp_cd(sessions, id, args).await),
        "model" => Some(acp_model(sessions, cfg, id, args, current.provider, current.model).await),
        "mode" => Some(acp_mode(sessions, id, args, current.mode).await),
        _ => None,
    }
}

/// `/cd` over ACP: resolve `args` against the session's stored cwd (so it
/// doesn't depend on the process-global working directory that other sessions
/// may have moved), verify it's a directory, and update the session cwd. An
/// empty arg goes home; a leading `~` expands to the home directory.
async fn acp_cd(sessions: &SessionMap, id: &str, args: &str) -> String {
    let mut map = sessions.lock().await;
    let Some(s) = map.get_mut(id) else {
        return "cd: unknown session".to_string();
    };
    let target = args.trim();
    let path = if target.is_empty() {
        dirs::home_dir().unwrap_or_default()
    } else if let Some(rest) = target.strip_prefix('~') {
        let mut home = dirs::home_dir().unwrap_or_default();
        home.push(rest.trim_start_matches('/'));
        home
    } else {
        let p = PathBuf::from(target);
        if p.is_absolute() { p } else { s.cwd.join(p) }
    };
    match dunce::canonicalize(&path) {
        Ok(canonical) if canonical.is_dir() => {
            s.cwd = canonical.clone();
            format!("changed directory to {}", canonical.display())
        }
        Ok(_) => format!("cd: not a directory: {}", path.display()),
        Err(e) => format!("cd: {}: {e}", path.display()),
    }
}

/// `(model, alias, is_active)` rows for the configured providers that pin a
/// model — the `/model` listing. Mirrors the interactive handler's helper.
fn acp_configured_models(
    providers: &HashMap<String, ProviderEntry>,
    current: &str,
) -> Vec<(String, String, bool)> {
    let mut rows: Vec<(String, String, bool)> = providers
        .iter()
        .filter_map(|(alias, entry)| {
            entry
                .model
                .as_ref()
                .map(|m| (m.clone(), alias.clone(), m == current))
        })
        .collect();
    rows.sort();
    rows
}

/// `/model` over ACP: with no arg, list the current + configured models; with
/// an arg, record a session-scoped switch. Routing mirrors the interactive
/// `/model` via [`resolve_model_switch`], but the switch is applied lazily —
/// the override is stored and the next prompt's `run_prompt` builds the client
/// (surfacing any auth error then), so this stays side-effect-free.
async fn acp_model(
    sessions: &SessionMap,
    cfg: &Config,
    id: &str,
    args: &str,
    current_provider: &str,
    current_model: &str,
) -> String {
    let providers = cfg.providers_map();
    let new_model = args.trim();
    if new_model.is_empty() {
        let mut out = format!("current model: {current_model}\n");
        let rows = acp_configured_models(&providers, current_model);
        if rows.is_empty() {
            out.push_str(
                "no models pinned in `providers` config — /model <id> switches to any model your provider supports",
            );
        } else {
            out.push_str("configured models:\n");
            for (model, alias, is_active) in &rows {
                let marker = if *is_active { "* " } else { "  " };
                out.push_str(&format!("{marker}{model}  ·  {alias}\n"));
            }
            out.push_str("usage: /model <id> to switch");
        }
        return out;
    }

    let (provider_override, note) = match resolve_model_switch(
        &providers,
        current_provider,
        new_model,
    ) {
        ModelSwitch::Switch(alias) => {
            let note = format!("  ·  {alias}");
            (Some(alias), note)
        }
        ModelSwitch::NoProviderForFamily(family) => {
            return format!(
                "'{new_model}' matches the {family} model family, but no {family} provider is configured — keeping model '{current_model}' on '{current_provider}'. Add a provider of type {family} to config to switch to it.",
            );
        }
        ModelSwitch::Keep => (None, String::new()),
    };

    let mut map = sessions.lock().await;
    if let Some(s) = map.get_mut(id) {
        s.model_override = Some(CompactString::new(new_model));
        s.session.model = CompactString::new(new_model);
        if let Some(alias) = &provider_override {
            s.provider_override = Some(CompactString::new(alias));
            s.session.provider = CompactString::new(alias);
        }
    }
    format!("switched to model: {new_model}{note}")
}

/// `/mode` over ACP: with no arg, show the current mode and options; with an
/// arg, record a session-scoped override that `resolve_acp_mode` honors on the
/// next prompt.
async fn acp_mode(
    sessions: &SessionMap,
    id: &str,
    args: &str,
    current_mode: SecurityMode,
) -> String {
    let arg = args.trim();
    if arg.is_empty() {
        return format!(
            "security mode: {current_mode}\n\n  /mode standard      use configured permission rules\n  /mode restrictive   default all tools to ask\n  /mode accept        auto-accept within working directory\n  /mode yolo          auto-accept ALL operations",
        );
    }
    let new_mode = match arg {
        "standard" => SecurityMode::Standard,
        "restrictive" => SecurityMode::Restrictive,
        "accept" => SecurityMode::Accept,
        "yolo" => SecurityMode::Yolo,
        other => return format!("unknown mode: {other}"),
    };
    if let Some(s) = sessions.lock().await.get_mut(id) {
        s.mode_override = Some(new_mode);
    }
    match new_mode {
        SecurityMode::Yolo => "security mode: yolo (all operations allowed)".to_string(),
        SecurityMode::Accept => "security mode: accept (auto-allow within CWD)".to_string(),
        other => format!("security mode: {other}"),
    }
}

/// Snapshot a session's `/model` and `/mode` overrides — `(model, provider,
/// mode)`. Unknown session (a prompt without `new_session`) yields all `None`.
async fn session_overrides(
    sessions: &SessionMap,
    id: &str,
) -> (
    Option<CompactString>,
    Option<CompactString>,
    Option<SecurityMode>,
) {
    let map = sessions.lock().await;
    match map.get(id) {
        Some(s) => (
            s.model_override.clone(),
            s.provider_override.clone(),
            s.mode_override,
        ),
        None => (None, None, None),
    }
}

fn create_acp_client(
    provider_str: &str,
    cfg: &Config,
) -> anyhow::Result<crate::provider::AnyClient> {
    crate::provider::create_client_with_auth(provider_str, None, &cfg.providers_map(), cfg.auth)
}

fn build_acp_permission(
    state: &AcpState,
    mode: SecurityMode,
) -> (Option<PermCheck>, Option<AskSender>) {
    use std::sync::Mutex;

    let no_tools = state.cli.resolve_no_tools(&state.cfg);
    if no_tools {
        return (None, None);
    }

    let perm_config: PermissionConfig = state
        .cfg
        .permission
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let checker = PermissionChecker::new(&perm_config, mode, None);
    let perm: PermCheck = Arc::new(Mutex::new(checker));

    let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(64);
    spawn_acp_ask_drain(ask_rx);
    (Some(perm), Some(ask_tx))
}

/// Two-mode correlator for matching rig `ToolResult` events back
/// to their originating `ToolCall` event when bridging to ACP.
/// Most providers (Anthropic, OpenAI) emit a stable `tool_call.id`
/// on the request and re-emit it on the result → use the id map.
/// Some providers (older OpenAI compat models) emit empty ids →
/// fall back to FIFO since rig emits results in request order.
///
/// Extracted from `run_prompt` so the F5 fix is unit-testable
/// without standing up a full ACP server.
#[derive(Default)]
struct ToolCallCorrelator {
    by_id: std::collections::HashMap<String, ToolCallId>,
    fifo: std::collections::VecDeque<ToolCallId>,
}

impl ToolCallCorrelator {
    /// Record a new `(rig_id → acp_id)` mapping. Empty rig_id
    /// pushes onto the FIFO queue.
    fn record(&mut self, rig_id: &str, acp_id: ToolCallId) {
        if rig_id.is_empty() {
            self.fifo.push_back(acp_id);
        } else {
            self.by_id.insert(rig_id.to_string(), acp_id);
        }
    }

    /// Resolve a result's rig_id to the originally-issued acp_id.
    /// Returns `None` if no matching call is in-flight; callers
    /// emit a stub empty id in that (shouldn't-happen) case.
    fn resolve(&mut self, rig_id: &str) -> Option<ToolCallId> {
        if !rig_id.is_empty() {
            self.by_id.remove(rig_id)
        } else {
            self.fifo.pop_front()
        }
    }
}

/// Drain `ask_rx` by responding to every permission ask with
/// `Deny`. ACP runs are non-interactive — there's no human at a
/// keyboard to confirm prompts. Previously the receiver was simply
/// dropped (`_ask_rx`), so any tool needing `Ask` confirmation
/// hit the 30s permission timeout and surfaced as a generic
/// failure to the editor client. Fail-fast with a clear deny is
/// strictly better: the LLM sees the denial immediately and can
/// re-plan, or the user can configure explicit allow rules.
///
/// **Future work**: route the ask through the ACP protocol as a
/// `requestPermission` notification so the editor client can
/// surface a real dialog. Out of scope for the F1 fix; that's a
/// Phase C5-ish feature requiring ACP protocol wiring.
fn spawn_acp_ask_drain(
    mut ask_rx: tokio::sync::mpsc::Receiver<crate::permission::ask::AskRequest>,
) {
    tokio::spawn(async move {
        while let Some(req) = ask_rx.recv().await {
            // The tool's caller is awaiting on `req.reply`. Dropping
            // it without sending would also surface as a tool error
            // ("Permission system unavailable"), but Deny is a
            // clearer signal that the call was *refused* rather
            // than the system being broken.
            let _ = req.reply.send(crate::permission::ask::UserDecision::Deny);
        }
    });
}

fn resolve_acp_mode(cli: &Cli, cfg: &Config, override_mode: Option<SecurityMode>) -> SecurityMode {
    // dirge-32k9 (gh#714): a session-scoped `/mode` override wins over CLI/config.
    if let Some(mode) = override_mode {
        return mode;
    }
    if cli.yolo || cfg.yolo.unwrap_or(false) {
        SecurityMode::Yolo
    } else if cli.accept_all || cfg.accept_all.unwrap_or(false) {
        SecurityMode::Accept
    } else if cli.restrictive || cfg.restrictive.unwrap_or(false) {
        SecurityMode::Restrictive
    } else if let Some(m) = &cfg.default_permission_mode {
        match m.as_str() {
            "yolo" => SecurityMode::Yolo,
            "accept" => SecurityMode::Accept,
            "restrictive" => SecurityMode::Restrictive,
            _ => SecurityMode::Standard,
        }
    } else {
        SecurityMode::Standard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderAuth;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static ACP_AUTH_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "dirge_acp_{tag}_{}_{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set_path(key: &'static str, value: &Path) -> Self {
            let old = std::env::var(key).ok();
            // SAFETY: ACP_AUTH_ENV_LOCK serializes all mutations in this module.
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }

        fn remove(key: &'static str) -> Self {
            let old = std::env::var(key).ok();
            // SAFETY: ACP_AUTH_ENV_LOCK serializes all mutations in this module.
            unsafe { std::env::remove_var(key) };
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: ACP_AUTH_ENV_LOCK serializes all mutations in this module.
            unsafe {
                match &self.old {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn acp_client_uses_top_level_chatgpt_auth() {
        let _lock = ACP_AUTH_ENV_LOCK.lock().unwrap();
        let dir = TestDir::new("codex_home");
        let _home = EnvGuard::set_path("CODEX_HOME", dir.path());
        let _access = EnvGuard::remove("CODEX_ACCESS_TOKEN");
        let _account = EnvGuard::remove("CHATGPT_ACCOUNT_ID");
        let cfg = Config {
            auth: Some(ProviderAuth::ChatGpt),
            ..Default::default()
        };

        let result = create_acp_client("openai", &cfg);
        let err = match result {
            Ok(_) => panic!("ACP client should attempt ChatGPT auth"),
            Err(err) => err.to_string(),
        };

        assert!(
            err.contains("ChatGPT auth requested"),
            "unexpected error: {err}"
        );
        assert!(!err.contains("OPENAI_API_KEY"), "unexpected error: {err}");
    }

    /// Regression for F1: any `AskRequest` sent through the ACP
    /// ask channel must be promptly responded to with `Deny`,
    /// rather than hanging. Without `spawn_acp_ask_drain`, the
    /// `reply` oneshot is dropped on receiver drop → tool sees
    /// `Permission system unavailable` (technically OK, but slower
    /// and worse signal). With the drain, the tool sees `Deny`
    /// within a tick.
    #[tokio::test]
    async fn acp_ask_drain_responds_with_deny() {
        let (ask_tx, ask_rx) = tokio::sync::mpsc::channel::<crate::permission::ask::AskRequest>(8);
        spawn_acp_ask_drain(ask_rx);

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        ask_tx
            .send(crate::permission::ask::AskRequest {
                tool: "bash".to_string(),
                input: "rm -rf /".to_string(),
                reason: None,
                reply: reply_tx,
            })
            .await
            .expect("send must succeed");

        let resp = tokio::time::timeout(std::time::Duration::from_millis(200), reply_rx)
            .await
            .expect("must reply within 200ms — F1 regression")
            .expect("reply channel must not be dropped");
        assert!(
            matches!(resp, crate::permission::ask::UserDecision::Deny),
            "ACP ask must auto-deny; got {:?}",
            resp,
        );
    }

    /// F5: two parallel tool calls with distinct rig ids → two
    /// results MUST pair with the right ACP ids. Previously the
    /// `last_tool_call_id` single-slot lost the first id when the
    /// second call arrived.
    #[test]
    fn correlator_matches_parallel_tool_calls_by_id() {
        let mut c = ToolCallCorrelator::default();
        let acp_a = ToolCallId::new("acp-A".to_string());
        let acp_b = ToolCallId::new("acp-B".to_string());
        c.record("rig-A", acp_a.clone());
        c.record("rig-B", acp_b.clone());

        // Results can arrive in either order.
        assert_eq!(c.resolve("rig-B"), Some(acp_b));
        assert_eq!(c.resolve("rig-A"), Some(acp_a));
    }

    /// Provider-empty ids fall to the FIFO queue, preserving
    /// request order (rig emits results in dispatch order for
    /// providers that don't supply ids).
    #[test]
    fn correlator_uses_fifo_for_empty_rig_ids() {
        let mut c = ToolCallCorrelator::default();
        let acp_a = ToolCallId::new("acp-A".to_string());
        let acp_b = ToolCallId::new("acp-B".to_string());
        c.record("", acp_a.clone());
        c.record("", acp_b.clone());

        // First result pairs with first call; second with second.
        assert_eq!(c.resolve(""), Some(acp_a));
        assert_eq!(c.resolve(""), Some(acp_b));
    }

    /// Mixed: an id'd call alongside an empty-id call. Each falls
    /// to its respective bucket — no cross-contamination.
    #[test]
    fn correlator_separates_id_and_fifo_buckets() {
        let mut c = ToolCallCorrelator::default();
        let acp_named = ToolCallId::new("acp-named".to_string());
        let acp_anon = ToolCallId::new("acp-anon".to_string());
        c.record("rig-X", acp_named.clone());
        c.record("", acp_anon.clone());

        assert_eq!(c.resolve(""), Some(acp_anon));
        assert_eq!(c.resolve("rig-X"), Some(acp_named));
    }

    /// Stray result (no matching call) → resolve returns None;
    /// the caller can choose a stub id. Don't panic.
    #[test]
    fn correlator_returns_none_for_unknown_id() {
        let mut c = ToolCallCorrelator::default();
        assert_eq!(c.resolve("missing"), None);
        assert_eq!(c.resolve(""), None);
    }

    /// dirge-6po9: an `AgentEvent::Error` must not be swallowed into a
    /// clean EndTurn. The error arm now surfaces the text as an
    /// agent-message chunk (same shape as ContextOverflow) so the editor
    /// client sees why the turn ended. Pin the chunk shape here.
    #[test]
    fn diagnostic_chunk_carries_the_error_text() {
        let sid = SessionId::new("sess-err".to_string());
        let notif = diagnostic_chunk(&sid, "error: provider auth failed".to_string());
        match notif.update {
            SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
                ContentBlock::Text(t) => {
                    assert!(
                        t.text.contains("provider auth failed"),
                        "chunk must carry the diagnostic text, got {:?}",
                        t.text
                    );
                }
                other => panic!("expected a text content block, got {other:?}"),
            },
            other => panic!("expected an AgentMessageChunk, got {other:?}"),
        }
    }

    /// dirge-5wqc: a fresh in-memory session map with one session already
    /// present, for the state-management tests below.
    fn session_map_with(id: &str) -> SessionMap {
        let mut map = std::collections::HashMap::new();
        map.insert(
            id.to_string(),
            AcpSession {
                session: crate::session::Session::new("p", "m", 0),
                cwd: std::env::temp_dir(),
                model_override: None,
                provider_override: None,
                mode_override: None,
                run: None,
            },
        );
        tokio::sync::Mutex::new(map)
    }

    /// Extract the plain text of every User message in a converted history.
    fn user_texts(history: &[rig::completion::Message]) -> Vec<String> {
        history
            .iter()
            .filter_map(|m| match m {
                rig::completion::Message::User { content } => Some(
                    content
                        .iter()
                        .filter_map(|c| match c {
                            rig::message::UserContent::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                ),
                _ => None,
            })
            .collect()
    }

    /// dirge-5wqc: the core statelessness bug. Two turns through `finish_turn`
    /// must accumulate — the second prompt's `convert_history` sees BOTH prior
    /// user turns, not an empty history. Before the fix, every prompt ran with
    /// `vec![]` so the editor conversation lost all context each turn.
    #[tokio::test]
    async fn finish_turn_accumulates_multi_turn_history() {
        let id = "sess-1";
        let sessions = session_map_with(id);

        // Turn 1.
        let (h0, _) = history_and_cwd(&sessions, id).await;
        assert!(h0.is_empty(), "fresh session starts with no history");
        finish_turn(
            &sessions,
            id,
            0,
            "what does foo do?",
            "foo does X.",
            Vec::new(),
        )
        .await;

        // Turn 2 sees turn 1.
        let (h1, _) = history_and_cwd(&sessions, id).await;
        let texts1 = user_texts(&h1);
        assert!(
            texts1.iter().any(|t| t.contains("what does foo do?")),
            "second prompt must see the first user turn, got {texts1:?}"
        );
        finish_turn(&sessions, id, 0, "now change it", "changed.", Vec::new()).await;

        // Turn 3 sees both.
        let (h2, _) = history_and_cwd(&sessions, id).await;
        let texts2 = user_texts(&h2);
        assert!(
            texts2.iter().any(|t| t.contains("what does foo do?"))
                && texts2.iter().any(|t| t.contains("now change it")),
            "history must retain both prior turns, got {texts2:?}"
        );
    }

    /// An unknown session (a prompt without a preceding `new_session`) yields an
    /// empty history rather than panicking.
    #[tokio::test]
    async fn history_for_unknown_session_is_empty() {
        let sessions: SessionMap = tokio::sync::Mutex::new(std::collections::HashMap::new());
        let (h, cwd) = history_and_cwd(&sessions, "nope").await;
        assert!(h.is_empty());
        assert!(cwd.is_none());
    }

    /// dirge-5wqc: `session/cancel` must abort the in-flight run — flip the
    /// cancellation flag, fire the cooperative cancel, and abort the task.
    /// Before the fix the cancel notification hit the "Unhandled" catch-all and
    /// the runner kept executing.
    #[tokio::test]
    async fn cancel_run_aborts_and_flags() {
        let id = "sess-c";
        let sessions = session_map_with(id);

        // A long-lived stand-in for the runner task.
        let task = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let (cancel_tx, mut cancel_rx) = tokio::sync::mpsc::channel::<()>(4);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        register_run(
            &sessions,
            id,
            "p",
            "m",
            AcpRun {
                generation: 0,
                task,
                cancel_tx,
                cancelled: cancelled.clone(),
            },
        )
        .await;

        assert!(
            cancel_run(&sessions, id).await,
            "an active run is cancelled"
        );
        assert!(
            cancelled.load(std::sync::atomic::Ordering::SeqCst),
            "the cancel flag is set so the loop reports Cancelled"
        );
        assert!(
            cancel_rx.try_recv().is_ok(),
            "the cooperative cancel signal was sent"
        );

        // A second cancel is a no-op (the run was already taken).
        assert!(!cancel_run(&sessions, id).await, "no run left to cancel");
    }

    /// A test stand-in for a registered run: a long-sleeping task plus fresh
    /// cancel channel and flag. Returns the run and its cancelled flag.
    fn stub_run() -> (AcpRun, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        let task = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let (cancel_tx, _cancel_rx) = tokio::sync::mpsc::channel::<()>(4);
        // Leak the receiver so try_send doesn't hit a closed channel.
        std::mem::forget(_cancel_rx);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        (
            AcpRun {
                generation: 0,
                task,
                cancel_tx,
                cancelled: cancelled.clone(),
            },
            cancelled,
        )
    }

    /// Overlap regression (dirge-5wqc reintroduction): prompt B replaces
    /// prompt A's run via `register_run` (which aborts A's task). A's loop
    /// exits and calls `finish_turn` with its own — now stale — generation.
    /// That must NOT clear B's run: B stays cancellable, and only B's own
    /// `finish_turn` (matching generation) clears the slot.
    #[tokio::test]
    async fn finish_turn_stale_generation_keeps_replacement_run() {
        let id = "sess-overlap";
        let sessions = session_map_with(id);

        let (run_a, _) = stub_run();
        let gen_a = register_run(&sessions, id, "p", "m", run_a).await;
        let (run_b, cancelled_b) = stub_run();
        let gen_b = register_run(&sessions, id, "p", "m", run_b).await;
        assert_ne!(gen_a, gen_b, "each registration gets a fresh generation");

        // Aborted A's loop exits and finishes its (partial) turn.
        finish_turn(&sessions, id, gen_a, "prompt A", "partial", Vec::new()).await;

        // A's history is still recorded...
        let (h, _) = history_and_cwd(&sessions, id).await;
        assert!(
            user_texts(&h).iter().any(|t| t.contains("prompt A")),
            "the aborted turn still lands in history"
        );
        // ...but B's run handle survives: session/cancel still reaches B.
        assert!(
            cancel_run(&sessions, id).await,
            "B must remain cancellable after A's stale finish_turn"
        );
        assert!(
            cancelled_b.load(std::sync::atomic::Ordering::SeqCst),
            "the cancel hit B's flag"
        );
    }

    /// The matching-generation path still clears the slot: after B's own
    /// `finish_turn`, there is no run left to cancel.
    #[tokio::test]
    async fn finish_turn_matching_generation_clears_run() {
        let id = "sess-own";
        let sessions = session_map_with(id);

        let (run, _) = stub_run();
        let generation = register_run(&sessions, id, "p", "m", run).await;
        finish_turn(&sessions, id, generation, "prompt", "reply", Vec::new()).await;
        assert!(
            !cancel_run(&sessions, id).await,
            "the finished run's slot is cleared"
        );
    }

    /// Cancelling an unknown session is a harmless no-op.
    #[tokio::test]
    async fn cancel_run_unknown_session_is_false() {
        let sessions: SessionMap = tokio::sync::Mutex::new(std::collections::HashMap::new());
        assert!(!cancel_run(&sessions, "ghost").await);
    }

    // dirge-32k9 (gh#714): ACP slash-command exposure + handling.

    /// The advertised set is exactly the curated commands, names carry no
    /// leading `/`, and only the arg-taking ones declare an input hint.
    #[test]
    fn available_commands_are_the_curated_set() {
        let cmds = acp_available_commands();
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["help", "clear", "cd", "model", "mode"]);
        assert!(
            names.iter().all(|n| !n.starts_with('/')),
            "ACP command names must not carry the leading slash",
        );
        for c in &cmds {
            assert!(!c.description.is_empty(), "{} needs a description", c.name);
            let wants_input = matches!(c.name.as_str(), "cd" | "model" | "mode");
            assert_eq!(
                c.input.is_some(),
                wants_input,
                "input hint presence wrong for {}",
                c.name,
            );
        }
    }

    /// `/help` text lists every advertised command so the two can't drift.
    #[test]
    fn help_text_lists_every_command() {
        let text = acp_help_text();
        for (name, _, _) in ACP_COMMANDS {
            assert!(text.contains(&format!("/{name}")), "help missing /{name}");
        }
    }

    #[test]
    fn parse_slash_splits_command_and_args() {
        assert_eq!(parse_acp_slash("/help"), Some(("help", "")));
        assert_eq!(
            parse_acp_slash("  /cd /tmp/foo  "),
            Some(("cd", "/tmp/foo"))
        );
        assert_eq!(
            parse_acp_slash("/model gpt-4o mini"),
            Some(("model", "gpt-4o mini")),
        );
        // Not slash commands.
        assert_eq!(parse_acp_slash("hello /help"), None);
        assert_eq!(parse_acp_slash("/"), None);
        assert_eq!(parse_acp_slash("/ spaced"), None);
        assert_eq!(parse_acp_slash(""), None);
    }

    /// An un-curated slash command is not intercepted — the caller forwards it
    /// to the model.
    #[tokio::test]
    async fn unknown_slash_command_is_not_handled() {
        let sessions = session_map_with("s");
        let cfg = Config::default();
        let out = handle_acp_slash(
            &sessions,
            &cfg,
            "s",
            "panel",
            "",
            AcpCurrent {
                provider: "openrouter",
                model: "some-model",
                mode: SecurityMode::Standard,
            },
        )
        .await;
        assert!(
            out.is_none(),
            "TUI-only /panel must fall through to the LLM"
        );
    }

    /// `/clear` wipes the accumulated conversation so the next prompt's
    /// converted history is empty.
    #[tokio::test]
    async fn clear_empties_session_history() {
        let id = "s";
        let sessions = session_map_with(id);
        finish_turn(&sessions, id, 0, "hi", "hello", Vec::new()).await;
        let (before, _) = history_and_cwd(&sessions, id).await;
        assert!(!before.is_empty(), "precondition: a turn is recorded");

        let reply = handle_acp_slash(
            &sessions,
            &Config::default(),
            id,
            "clear",
            "",
            AcpCurrent {
                provider: "p",
                model: "m",
                mode: SecurityMode::Standard,
            },
        )
        .await;
        assert!(reply.unwrap().contains("cleared"));

        let (after, _) = history_and_cwd(&sessions, id).await;
        assert!(after.is_empty(), "history must be empty after /clear");
    }

    /// `/cd` into a real directory updates the session cwd; a bad path reports
    /// an error and leaves the cwd untouched.
    #[tokio::test]
    async fn cd_updates_cwd_and_rejects_bad_paths() {
        let id = "s";
        let sessions = session_map_with(id);
        let dir = TestDir::new("cd");
        let canonical = dunce::canonicalize(dir.path()).unwrap();

        let reply = acp_cd(&sessions, id, &dir.path().to_string_lossy()).await;
        assert!(reply.contains("changed directory"), "got {reply}");
        assert_eq!(
            sessions.lock().await.get(id).unwrap().cwd,
            canonical,
            "cwd updated to the canonical target",
        );

        let bad = acp_cd(&sessions, id, "/no/such/dir/exists/here").await;
        assert!(bad.starts_with("cd:"), "bad path reports an error: {bad}");
        assert_eq!(
            sessions.lock().await.get(id).unwrap().cwd,
            canonical,
            "a failed /cd leaves the cwd unchanged",
        );
    }

    /// `/model` with no arg lists the current model; with an unclassifiable id
    /// it records a same-provider switch as a session override.
    #[tokio::test]
    async fn model_lists_then_switches() {
        let id = "s";
        let sessions = session_map_with(id);
        let cfg = Config::default();

        let list = acp_model(&sessions, &cfg, id, "", "openrouter", "current-x").await;
        assert!(list.contains("current model: current-x"), "got {list}");

        // `llama-3.1` has no recognized family → ModelSwitch::Keep → override
        // set, provider left alone.
        let set = acp_model(&sessions, &cfg, id, "llama-3.1", "openrouter", "current-x").await;
        assert!(set.contains("switched to model: llama-3.1"), "got {set}");
        let (model_ovr, provider_ovr, _) = session_overrides(&sessions, id).await;
        assert_eq!(model_ovr.as_deref(), Some("llama-3.1"));
        assert!(
            provider_ovr.is_none(),
            "a same-provider swap keeps the provider",
        );
    }

    /// `/mode` shows the current mode with no arg, records a valid mode as an
    /// override, and rejects an unknown one without touching state.
    #[tokio::test]
    async fn mode_views_sets_and_rejects() {
        let id = "s";
        let sessions = session_map_with(id);

        let view = acp_mode(&sessions, id, "", SecurityMode::Standard).await;
        assert!(view.contains("security mode: standard"), "got {view}");

        let set = acp_mode(&sessions, id, "yolo", SecurityMode::Standard).await;
        assert!(set.contains("yolo"), "got {set}");
        let (_, _, mode_ovr) = session_overrides(&sessions, id).await;
        assert_eq!(mode_ovr, Some(SecurityMode::Yolo));

        let bad = acp_mode(&sessions, id, "bogus", SecurityMode::Standard).await;
        assert!(bad.contains("unknown mode"), "got {bad}");
        let (_, _, still_yolo) = session_overrides(&sessions, id).await;
        assert_eq!(still_yolo, Some(SecurityMode::Yolo), "bad arg is a no-op");
    }

    /// A session `/mode` override wins over CLI/config resolution.
    #[test]
    fn resolve_mode_prefers_session_override() {
        use clap::Parser as _;
        let cli = Cli::parse_from(["dirge"]);
        let cfg = Config::default();
        assert_eq!(
            resolve_acp_mode(&cli, &cfg, Some(SecurityMode::Accept)),
            SecurityMode::Accept,
        );
        // No override → the CLI/config default (Standard here).
        assert_eq!(resolve_acp_mode(&cli, &cfg, None), SecurityMode::Standard,);
    }

    /// Multiple concurrent asks all get responded to.
    #[tokio::test]
    async fn acp_ask_drain_handles_multiple_concurrent_asks() {
        let (ask_tx, ask_rx) = tokio::sync::mpsc::channel::<crate::permission::ask::AskRequest>(8);
        spawn_acp_ask_drain(ask_rx);

        let mut replies = Vec::new();
        for i in 0..5 {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            ask_tx
                .send(crate::permission::ask::AskRequest {
                    tool: format!("bash-{i}"),
                    input: format!("cmd-{i}"),
                    reason: None,
                    reply: reply_tx,
                })
                .await
                .unwrap();
            replies.push(reply_rx);
        }

        for reply_rx in replies {
            let resp = tokio::time::timeout(std::time::Duration::from_millis(500), reply_rx)
                .await
                .expect("each reply must arrive promptly")
                .expect("reply channel dropped");
            assert!(matches!(resp, crate::permission::ask::UserDecision::Deny));
        }
    }
}
