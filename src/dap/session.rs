//! DAP session manager — launch, attach, breakpoint cache, event handling.
//!
//! Manages a single active debug session. Launching a new session
//! terminates any existing one (single-session enforcement).

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Mutex, mpsc};

use crate::agent::agent_loop::tool::AbortSignal;
use crate::agent::tools::ToolError;
use crate::dap::client::{DapClient, RpcError};

#[cfg(test)]
use crate::dap::client::DapRpc;
use crate::dap::types::*;
use crate::permission::checker::PermCheck;

/// Global DAP session manager — set during `DebugTool` construction,
/// read by the UI loop for debug panel snapshots. Uses a std Mutex
/// (not tokio) so it can be written from sync constructors and read
/// from the UI loop without an async context.
pub static DAP_MANAGER: StdMutex<Option<std::sync::Arc<DapSessionManager>>> = StdMutex::new(None);

/// Global DAP permission checker — set during `DebugTool` construction
/// alongside `DAP_MANAGER`. Read by the Janet FFI bridge to gate
/// expression evaluation (`dap/eval`) through the same permission
/// engine the agent tool path uses. Ask results from the engine are
/// treated as denial (no dialog in the bridge task).
pub static DAP_PERM_CHECK: StdMutex<Option<PermCheck>> = StdMutex::new(None);

// ---------------------------------------------------------------------------
// Output cap
// ---------------------------------------------------------------------------

/// Maximum bytes of accumulated output we retain per session.
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// dirge-p3r7: how long `attach` waits for an initial stopped event before
/// concluding the debuggee is running. Attaching rarely produces a stop, so
/// this is a short grace window — not the full request timeout, which would
/// stall the attach and race the bridge command timeout.
const ATTACH_STOP_GRACE: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Per-session event channels
// ---------------------------------------------------------------------------

/// Bundled receivers for DAP events during a session.
struct EventReceivers {
    stopped: mpsc::UnboundedReceiver<StoppedEventBody>,
    output: mpsc::UnboundedReceiver<OutputEventBody>,
    terminated: mpsc::UnboundedReceiver<TerminatedEventBody>,
    exited: mpsc::UnboundedReceiver<ExitedEventBody>,
}

impl EventReceivers {
    /// A placeholder set of receivers whose senders are already dropped.
    /// dirge-acgj: `continue_` swaps the live receivers out from behind the
    /// `active` lock while it parks on the stop-wait, leaving this dead set
    /// in the session so the lock can be released. `try_recv` on these is a
    /// harmless no-op; a concurrent waiter is deflected earlier by the
    /// `stop_wait_in_flight` guard, so it never blocks on them.
    fn dead() -> Self {
        let (_, stopped) = mpsc::unbounded_channel();
        let (_, output) = mpsc::unbounded_channel();
        let (_, terminated) = mpsc::unbounded_channel();
        let (_, exited) = mpsc::unbounded_channel();
        EventReceivers {
            stopped,
            output,
            terminated,
            exited,
        }
    }
}

/// Register handlers on `client` that forward events into channels.
async fn register_event_channels(client: &DapClient) -> EventReceivers {
    let (stopped_tx, stopped_rx) = mpsc::unbounded_channel();
    let (output_tx, output_rx) = mpsc::unbounded_channel();
    let (terminated_tx, terminated_rx) = mpsc::unbounded_channel();
    let (exited_tx, exited_rx) = mpsc::unbounded_channel();

    client
        .on_event(
            "stopped",
            Box::new(move |v| {
                if let Ok(body) = serde_json::from_value::<StoppedEventBody>(v) {
                    let _ = stopped_tx.send(body);
                }
            }),
        )
        .await;
    client
        .on_event(
            "output",
            Box::new(move |v| {
                if let Ok(body) = serde_json::from_value::<OutputEventBody>(v) {
                    let _ = output_tx.send(body);
                }
            }),
        )
        .await;
    client
        .on_event(
            "terminated",
            Box::new(move |v| {
                if let Ok(body) = serde_json::from_value::<TerminatedEventBody>(v) {
                    let _ = terminated_tx.send(body);
                }
            }),
        )
        .await;
    client
        .on_event(
            "exited",
            Box::new(move |v| {
                if let Ok(body) = serde_json::from_value::<ExitedEventBody>(v) {
                    let _ = exited_tx.send(body);
                }
            }),
        )
        .await;

    EventReceivers {
        stopped: stopped_rx,
        output: output_rx,
        terminated: terminated_rx,
        exited: exited_rx,
    }
}

// ---------------------------------------------------------------------------
// DapSession — active debug session state
// ---------------------------------------------------------------------------

struct DapSession {
    id: String,
    client: DapClient,
    status: SessionStatus,
    breakpoints: HashMap<PathBuf, Vec<BreakpointRecord>>,
    function_breakpoints: Vec<FunctionBreakpoint>,
    output: String,
    output_truncated: bool,
    exit_code: Option<u32>,
    events: EventReceivers,
    /// Cached for TUI debug panel snapshots.
    cached_threads: Vec<Thread>,
    /// Cached for TUI debug panel snapshots.
    cached_frames: Vec<StackFrame>,
    /// Cached for TUI debug panel snapshots (last variables request).
    cached_variables: Vec<Variable>,
    /// dirge-vept: threadId from the most recent stopped event. The Janet
    /// bridge calls step/continue/stackTrace with thread_id 0 (unspecified);
    /// strict adapters (debugpy) reject an id of 0 with "thread not found", so
    /// the session substitutes this last stopped thread when a caller passes 0.
    last_stopped_thread_id: Option<u32>,
    languages: Vec<String>,
    /// dirge-acgj: true while `continue_` has handed the event receivers off
    /// and is parked on the stop-wait with the `active` lock released. A
    /// concurrent step/pause that reaches the session in this window has its
    /// own request issued but must not also wait for the stop — the parked
    /// `continue_` owns the event stream and will report the resulting stop.
    stop_wait_in_flight: bool,
}

impl DapSession {
    fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            adapter_name: self.client.adapter_name.clone(),
            program: None,
            status: self.status.clone(),
            breakpoint_count: self.breakpoints.values().map(|v| v.len()).sum(),
            function_breakpoint_count: self.function_breakpoints.len(),
            stop_reason: None,
            thread_id: None,
            output: String::new(),
            output_truncated: false,
            exit_code: None,
            capabilities: self
                .client
                .capabilities
                .try_lock()
                .ok()
                .and_then(|g| g.clone()),
            languages: self.languages.clone(),
        }
    }

    /// Drain all pending output events into the output buffer.
    fn drain_output(&mut self) {
        while let Ok(evt) = self.events.output.try_recv() {
            // Stop appending once at the cap (keep draining the channel so a
            // flooding adapter can't back it up), so the buffer can't grow
            // unbounded before the post-hoc truncate.
            if self.output.len() >= MAX_OUTPUT_BYTES {
                self.output_truncated = true;
                continue;
            }
            self.output.push_str(&evt.output);
        }
        if self.output.len() > MAX_OUTPUT_BYTES {
            // `String::truncate` panics if the index isn't on a char
            // boundary, and `evt.output` is adapter-controlled — back off to
            // the nearest boundary at or below the cap.
            let mut cut = MAX_OUTPUT_BYTES;
            while cut > 0 && !self.output.is_char_boundary(cut) {
                cut -= 1;
            }
            self.output.truncate(cut);
            self.output_truncated = true;
        }
    }

    /// dirge-un0g: discard any stopped events left queued from a previous
    /// stop before issuing the next step/continue/pause. The stopped channel
    /// is unbounded and drained only on demand, so a stale event (e.g. a
    /// second thread that halted alongside the first) would otherwise satisfy
    /// the next wait instantly — reporting the previous stop's reason/thread
    /// and skewing every following op by one. Mirrors `drain_output`.
    fn drain_stopped(&mut self) {
        while self.events.stopped.try_recv().is_ok() {}
    }

    /// Drain queued stopped events, returning the most recent one. `pause`
    /// uses this instead of the plain drain: a queued stop with no waiter can
    /// be a genuine never-reported halt (e.g. a breakpoint hit after a
    /// timed-out continue, status still Running). Pausing an already-stopped
    /// program produces no new event, so discarding the drained stop would
    /// lose it — the wait times out and the status sticks at Running.
    fn drain_stopped_latest(&mut self) -> Option<StoppedEventBody> {
        let mut latest = None;
        while let Ok(evt) = self.events.stopped.try_recv() {
            latest = Some(evt);
        }
        latest
    }

    /// Drain and check for terminated/exited events.
    fn drain_termination(&mut self) {
        if self.events.terminated.try_recv().is_ok() {
            self.status = SessionStatus::Terminated;
        }
        if let Ok(evt) = self.events.exited.try_recv() {
            self.exit_code = Some(evt.exit_code as u32);
        }
    }

    /// Wait for a stopped event with timeout.
    async fn wait_for_stopped(&mut self, timeout: Duration) -> Result<StoppedEventBody, ToolError> {
        // dirge-acgj: a `continue_` is parked on the stop-wait with the event
        // receivers handed off; this session's copy is a dead placeholder.
        // Don't block on it — our request already reached the adapter and the
        // parked continue will report the stop it induces.
        if self.stop_wait_in_flight {
            return Err(ToolError::Msg(
                "a continue is already waiting for the next stop; its result will reflect this request".into(),
            ));
        }
        let stopped = tokio::time::timeout(timeout, self.events.stopped.recv())
            .await
            .map_err(|_| {
                ToolError::Msg(format!(
                    "timed out after {timeout:?} waiting for stopped event"
                ))
            })?
            .ok_or_else(|| ToolError::Msg("debug adapter disconnected".into()))?;
        self.record_stopped_thread(&stopped);
        Ok(stopped)
    }

    /// dirge-vept: remember the thread the adapter last stopped on, so a
    /// later step/continue/stackTrace with the 0 sentinel can target it.
    fn record_stopped_thread(&mut self, stopped: &StoppedEventBody) {
        if let Some(tid) = stopped.thread_id {
            self.last_stopped_thread_id = Some(tid as u32);
        }
    }

    /// dirge-vept: resolve a caller-supplied thread id. A `requested` of 0
    /// means "unspecified" (the Janet bridge hardcodes it); fall back to the
    /// last stopped thread. A non-zero id is honored verbatim.
    fn resolve_thread_id(&self, requested: u32) -> u32 {
        if requested == 0 {
            self.last_stopped_thread_id.unwrap_or(requested)
        } else {
            requested
        }
    }
}

// ---------------------------------------------------------------------------
// DapSessionManager — public API
// ---------------------------------------------------------------------------

pub struct DapSessionManager {
    /// In an `Arc` so `continue_` can hand the stop-wait (phases 2+3) to a
    /// spawned task that outlives a cancelled caller — see dirge-acgj notes
    /// in `continue_`.
    active: Arc<Mutex<Option<DapSession>>>,
    next_id: std::sync::atomic::AtomicU64,
    /// Last successfully-built panel snapshot. The session methods hold
    /// `active` across their adapter round-trip, so the UI's `try_lock` in
    /// `debug_snapshot` fails for that whole window — returning this cached
    /// copy keeps the debug panel showing the last-known state instead of
    /// blanking out. A plain `std::sync::Mutex`, never held across `.await`.
    last_snapshot: std::sync::Mutex<Option<DebugPanelData>>,
}

impl DapSessionManager {
    pub fn new() -> Self {
        Self {
            active: Arc::new(Mutex::new(None)),
            next_id: std::sync::atomic::AtomicU64::new(1),
            last_snapshot: std::sync::Mutex::new(None),
        }
    }

    fn next_id(&self) -> String {
        use std::sync::atomic::Ordering;
        let n = self.next_id.fetch_add(1, Ordering::SeqCst);
        format!("dap-{n}")
    }

    /// Launch a debug session.
    ///
    /// Terminates any existing active session first.
    /// Returns a summary once the program is stopped (on entry or breakpoint).
    #[allow(clippy::too_many_arguments)]
    pub async fn launch(
        &self,
        adapter_name: &str,
        adapter_cmd: &str,
        adapter_args: &[String],
        cwd: &str,
        program: Option<&str>,
        module: Option<&str>,
        program_args: &[String],
        stop_on_entry: Option<bool>,
        launch_extra: Option<serde_json::Value>,
        signal: &AbortSignal,
        timeout: Duration,
        languages: Vec<String>,
    ) -> Result<SessionSummary, ToolError> {
        self.terminate_active().await;

        let client = DapClient::spawn_stdio(
            adapter_name,
            Path::new(adapter_cmd),
            adapter_args,
            Path::new(cwd),
        )
        .await
        .map_err(|e| ToolError::Msg(format!("failed to spawn adapter: {e}")))?;

        self.launch_with_client(
            adapter_name,
            cwd,
            program,
            module,
            program_args,
            stop_on_entry,
            launch_extra,
            signal,
            client,
            timeout,
            languages,
        )
        .await
    }

    /// Core launch logic — used by both public launch and tests.
    ///
    /// `_signal` is reserved for future cancellation integration — when wired,
    /// a `tokio::select!` on `signal.received()` will abort the initial-stop
    /// wait so a user can cancel a hung launch.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn launch_with_client(
        &self,
        adapter_name: &str,
        cwd: &str,
        program: Option<&str>,
        module: Option<&str>,
        program_args: &[String],
        stop_on_entry: Option<bool>,
        launch_extra: Option<serde_json::Value>,
        _signal: &AbortSignal,
        client: DapClient,
        timeout: Duration,
        languages: Vec<String>,
    ) -> Result<SessionSummary, ToolError> {
        // Register event handlers.
        let mut events = register_event_channels(&client).await;

        // Initialize handshake.
        let init_args = InitializeArgs {
            adapter_id: adapter_name.to_string(),
            ..Default::default()
        };

        let caps: Capabilities = client
            .request("initialize", &init_args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        *client.capabilities.lock().await = Some(caps.clone());

        // Build launch arguments.
        let mut launch_args = LaunchArgs {
            program: program.map(|s| s.to_string()),
            module: module.map(|s| s.to_string()),
            cwd: Some(cwd.to_string()),
            args: Some(program_args.to_vec()),
            stop_on_entry,
            ..Default::default()
        };

        if let Some(extra) = launch_extra {
            launch_args.extra = extra;
        }

        // Send launch request as fire-and-forget — some adapters (debugpy)
        // won't respond to launch until configurationDone is received. We must
        // send configurationDone immediately to avoid a deadlock.
        //
        // Tradeoff: launch errors (bad program path, permissions) from adapters
        // that don't reply to launch-notify surface only as a stopped-event
        // timeout rather than a direct failure response. This is a protocol
        // limitation — the adapter won't respond to launch until we signal
        // configurationDone, and by then the launch is already in flight.
        client
            .notify("launch", &launch_args)
            .await
            .map_err(rpc_to_tool_error)?;

        // Send configurationDone if adapter supports it.
        if caps.supports_configuration_done_request.unwrap_or(false) {
            client
                .notify("configurationDone", &ConfigurationDoneArgs::default())
                .await
                .map_err(rpc_to_tool_error)?;
        }

        // Wait for the initial stopped event (stopOnEntry).
        // events is moved into DapSession later, so we destructure carefully.
        let stopped = tokio::time::timeout(timeout, events.stopped.recv())
            .await
            .map_err(|_| {
                ToolError::Msg(format!(
                    "timed out after {timeout:?} waiting for initial stop"
                ))
            })?
            .ok_or_else(|| {
                ToolError::Msg("debug adapter disconnected before stopped event".into())
            })?;

        let id = self.next_id();
        let mut session = DapSession {
            id: id.clone(),
            status: SessionStatus::Stopped,
            breakpoints: HashMap::new(),
            function_breakpoints: Vec::new(),
            output: String::new(),
            output_truncated: false,
            exit_code: None,
            events,
            client,
            cached_threads: Vec::new(),
            cached_frames: Vec::new(),
            cached_variables: Vec::new(),
            last_stopped_thread_id: stopped.thread_id.map(|id| id as u32),
            languages,
            stop_wait_in_flight: false,
        };
        session.drain_output();

        let mut summary = session.summary();
        summary.stop_reason = Some(stopped.reason.as_str().to_string());
        summary.thread_id = stopped.thread_id.map(|id| id as u32);
        *self.active.lock().await = Some(session);

        Ok(summary)
    }

    /// Attach to a running process.
    ///
    /// `_signal` is reserved for future cancellation integration.
    #[allow(clippy::too_many_arguments)]
    pub async fn attach(
        &self,
        adapter_name: &str,
        adapter_cmd: &str,
        adapter_args: &[String],
        cwd: &str,
        pid: Option<u32>,
        port: Option<u16>,
        host: Option<String>,
        attach_extra: Option<serde_json::Value>,
        _signal: &AbortSignal,
        timeout: Duration,
        languages: Vec<String>,
    ) -> Result<SessionSummary, ToolError> {
        self.terminate_active().await;

        let client = DapClient::spawn_stdio(
            adapter_name,
            Path::new(adapter_cmd),
            adapter_args,
            Path::new(cwd),
        )
        .await
        .map_err(|e| ToolError::Msg(format!("failed to spawn adapter: {e}")))?;

        self.attach_with_client(
            adapter_name,
            cwd,
            pid,
            port,
            host,
            attach_extra,
            _signal,
            client,
            timeout,
            languages,
        )
        .await
    }

    /// Core attach logic — used by both public attach and tests.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn attach_with_client(
        &self,
        adapter_name: &str,
        cwd: &str,
        pid: Option<u32>,
        port: Option<u16>,
        host: Option<String>,
        attach_extra: Option<serde_json::Value>,
        _signal: &AbortSignal,
        client: DapClient,
        timeout: Duration,
        languages: Vec<String>,
    ) -> Result<SessionSummary, ToolError> {
        let mut events = register_event_channels(&client).await;

        let init_args = InitializeArgs {
            adapter_id: adapter_name.to_string(),
            ..Default::default()
        };
        let caps: Capabilities = client
            .request("initialize", &init_args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        *client.capabilities.lock().await = Some(caps.clone());

        let mut attach_args = AttachArgs {
            pid,
            port,
            host,
            cwd: Some(cwd.to_string()),
            ..Default::default()
        };

        if let Some(extra) = attach_extra {
            attach_args.extra = extra;
        }

        client
            .request::<_, Value>("attach", &attach_args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        if caps.supports_configuration_done_request.unwrap_or(false) {
            client
                .notify("configurationDone", &ConfigurationDoneArgs::default())
                .await
                .map_err(rpc_to_tool_error)?;
        }

        // dirge-p3r7: attaching to an already-running process usually yields
        // no stopped event at all. Waiting the full request timeout here
        // stalls ~30s and races the Janet bridge's own DAP_CMD_TIMEOUT; then
        // recording Stopped unconditionally makes the debug panel and
        // summaries misreport a live debuggee as halted. Wait only a short
        // grace period, and set the status from what actually arrived.
        let grace = ATTACH_STOP_GRACE.min(timeout);
        let stopped = match tokio::time::timeout(grace, events.stopped.recv()).await {
            Ok(Some(body)) => Some(body),
            _ => None,
        };
        let status = if stopped.is_some() {
            SessionStatus::Stopped
        } else {
            SessionStatus::Running
        };

        let id = self.next_id();
        let mut session = DapSession {
            id: id.clone(),
            status,
            breakpoints: HashMap::new(),
            function_breakpoints: Vec::new(),
            output: String::new(),
            output_truncated: false,
            exit_code: None,
            events,
            client,
            cached_threads: Vec::new(),
            cached_frames: Vec::new(),
            cached_variables: Vec::new(),
            last_stopped_thread_id: stopped
                .as_ref()
                .and_then(|s| s.thread_id)
                .map(|id| id as u32),
            languages,
            stop_wait_in_flight: false,
        };
        session.drain_output();

        let mut summary = session.summary();
        if let Some(stopped) = stopped {
            summary.stop_reason = Some(stopped.reason.as_str().to_string());
            summary.thread_id = stopped.thread_id.map(|id| id as u32);
        }

        *self.active.lock().await = Some(session);
        Ok(summary)
    }

    /// Set file breakpoints for the active session.
    pub async fn set_breakpoints(
        &self,
        file: &str,
        breakpoints: Vec<SourceBreakpoint>,
        timeout: Duration,
    ) -> Result<Vec<Breakpoint>, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let source = Source {
            path: Some(file.to_string()),
            ..Default::default()
        };

        let args = SetBreakpointsArgs {
            source,
            breakpoints: Some(breakpoints.clone()),
            breakpoints_deprecated: None,
            source_modified: None,
        };

        let response: SetBreakpointsResponse = session
            .client
            .request("setBreakpoints", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        let path = PathBuf::from(file);
        session.breakpoints.insert(
            path,
            vec![BreakpointRecord {
                file: file.to_string(),
                breakpoints,
                verified: Some(response.breakpoints.clone()),
            }],
        );

        Ok(response.breakpoints)
    }

    /// Set function breakpoints.
    #[allow(dead_code)] // reserved for future agent tool action
    pub async fn set_function_breakpoints(
        &self,
        breakpoints: Vec<FunctionBreakpoint>,
        timeout: Duration,
    ) -> Result<Vec<Breakpoint>, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = SetFunctionBreakpointsArgs {
            breakpoints: breakpoints.clone(),
        };

        let response: SetFunctionBreakpointsResponse = session
            .client
            .request("setFunctionBreakpoints", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.function_breakpoints = breakpoints;
        Ok(response.breakpoints)
    }

    /// Continue execution and wait for the next stop event.
    pub async fn continue_(
        &self,
        thread_id: u32,
        _signal: &AbortSignal,
        timeout: Duration,
    ) -> Result<ContinueOutcome, ToolError> {
        // Phase 1: under the lock, issue the continue and hand the event
        // receivers off. dirge-acgj: the wait below used to run with `active`
        // held, so a pause meant to interrupt a free-running program blocked
        // on the mutex for this continue's full timeout — the one window where
        // pause is meaningful. Take the receivers out and release the lock so
        // concurrent requests (pause, evaluate) can reach the adapter.
        // dirge-8gdv: capture the session identity under the lock so phase 3 can
        // detect that a launch/attach replaced `active` while we were parked,
        // and refuse to restore our stale receivers into the new session.
        let (mut receivers, session_id) = {
            let mut active = self.active.lock().await;
            let session = active
                .as_mut()
                .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

            // A previous continue is still parked on its stop-wait and owns
            // the event stream; the session holds the dead placeholder.
            // Proceeding would issue a real continue, park on the dead
            // receivers (instant false "disconnected"), and clear
            // `stop_wait_in_flight` out from under the first waiter.
            if session.stop_wait_in_flight {
                return Err(ToolError::Msg(
                    "a continue is already waiting for the next stop; its result will reflect this request".into(),
                ));
            }

            // dirge-un0g: clear stops queued from a prior halt so we wait for
            // the fresh one this continue induces, not a stale event.
            session.drain_stopped();

            let args = ContinueArgs {
                thread_id: session.resolve_thread_id(thread_id),
                single_thread: None,
            };

            session
                .client
                .request::<_, ContinueResponse>("continue", &args, timeout)
                .await
                .map_err(rpc_to_tool_error)?;

            session.status = SessionStatus::Running;
            session.stop_wait_in_flight = true;
            let session_id = session.id.clone();
            (
                std::mem::replace(&mut session.events, EventReceivers::dead()),
                session_id,
            )
        };

        // Phases 2+3 run in a spawned task so they survive caller drop: the
        // agent tool executor drops the tool future on cancel, and a drop
        // mid-park would destroy the live receivers and leave
        // `stop_wait_in_flight` set forever — every later step/pause
        // deflected by the guard, every continue parked on dead channels.
        // Dropping the JoinHandle below merely detaches the task; it still
        // restores the receivers and clears the flag whenever the adapter
        // eventually stops, terminates, or the timeout fires.
        let active = Arc::clone(&self.active);
        let park = tokio::spawn(async move {
            // Phase 2: wait for the stop with the lock released.
            enum StopOutcome {
                Stopped(StoppedEventBody),
                Terminated,
                Disconnected,
                TimedOut,
            }
            let outcome = tokio::select! {
                s = receivers.stopped.recv() => match s {
                    Some(stopped) => StopOutcome::Stopped(stopped),
                    None => StopOutcome::Disconnected,
                },
                _ = receivers.terminated.recv() => StopOutcome::Terminated,
                _ = tokio::time::sleep(timeout) => StopOutcome::TimedOut,
            };

            // Phase 3: re-acquire, restore the receivers, record the result.
            let mut active = active.lock().await;
            let session = active
                .as_mut()
                .ok_or_else(|| ToolError::Msg("debug session ended during continue".into()))?;
            // dirge-8gdv: if a launch/attach replaced the active session while
            // we were parked, the session now in `active` is a different one.
            // Restoring our (taken, now-stale) receivers into it would clobber
            // the new session's live event channels and stomp its
            // stop_wait_in_flight/status — it would then never see another
            // event. Bail instead: drop the stale receivers and leave the
            // replacement untouched.
            if session.id != session_id {
                return Err(ToolError::Msg(
                    "debug session replaced during continue".into(),
                ));
            }
            session.events = receivers;
            session.stop_wait_in_flight = false;

            let (stop_reason, stop_thread_id) = match outcome {
                StopOutcome::Stopped(stopped) => {
                    session.status = SessionStatus::Stopped;
                    session.record_stopped_thread(&stopped);
                    (
                        Some(stopped.reason.as_str().to_string()),
                        stopped.thread_id.map(|id| id as u32),
                    )
                }
                StopOutcome::Terminated => {
                    session.status = SessionStatus::Terminated;
                    (Some("terminated".into()), None)
                }
                StopOutcome::Disconnected => {
                    return Err(ToolError::Msg("debug adapter disconnected".into()));
                }
                StopOutcome::TimedOut => {
                    return Err(ToolError::Msg(format!(
                        "timed out after {timeout:?} waiting for stop after continue"
                    )));
                }
            };

            session.drain_output();
            session.drain_termination();

            Ok(ContinueOutcome {
                status: session.status.clone(),
                output: session.output.clone(),
                output_truncated: session.output_truncated,
                exit_code: session.exit_code,
                stop_reason,
                thread_id: stop_thread_id,
            })
        });

        park.await
            .map_err(|e| ToolError::Msg(format!("continue stop-wait task failed: {e}")))?
    }

    /// Step over (next).
    pub async fn step_over(
        &self,
        thread_id: u32,
        _signal: &AbortSignal,
        timeout: Duration,
    ) -> Result<SessionSummary, ToolError> {
        self.step("next", thread_id, timeout).await
    }

    /// Step into.
    pub async fn step_in(
        &self,
        thread_id: u32,
        _signal: &AbortSignal,
        timeout: Duration,
    ) -> Result<SessionSummary, ToolError> {
        self.step("stepIn", thread_id, timeout).await
    }

    /// Step out.
    pub async fn step_out(
        &self,
        thread_id: u32,
        _signal: &AbortSignal,
        timeout: Duration,
    ) -> Result<SessionSummary, ToolError> {
        self.step("stepOut", thread_id, timeout).await
    }

    async fn step(
        &self,
        command: &str,
        thread_id: u32,
        timeout: Duration,
    ) -> Result<SessionSummary, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        // dirge-un0g: clear stops queued from a prior halt so this step waits
        // for the fresh one it induces, not a stale event.
        session.drain_stopped();

        // dirge-vept: substitute the last stopped thread when the bridge
        // passes the 0 sentinel.
        let thread_id = session.resolve_thread_id(thread_id);
        let args = match command {
            "next" => serde_json::to_value(NextArgs {
                thread_id,
                single_thread: None,
                granularity: None,
            })
            .unwrap(),
            "stepIn" => serde_json::to_value(StepInArgs {
                thread_id,
                single_thread: None,
                granularity: None,
                target_id: None,
            })
            .unwrap(),
            "stepOut" => serde_json::to_value(StepOutArgs {
                thread_id,
                single_thread: None,
                granularity: None,
            })
            .unwrap(),
            _ => return Err(ToolError::Msg(format!("unknown step command: {command}"))),
        };
        session
            .client
            .request::<_, Value>(command, &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.status = SessionStatus::Running;

        let stopped = session.wait_for_stopped(timeout).await?;
        session.status = SessionStatus::Stopped;
        session.drain_output();
        session.drain_termination();

        let mut summary = session.summary();
        summary.stop_reason = Some(stopped.reason.as_str().to_string());
        summary.thread_id = stopped.thread_id.map(|id| id as u32);
        Ok(summary)
    }

    /// Pause execution.
    pub async fn pause(
        &self,
        thread_id: u32,
        timeout: Duration,
    ) -> Result<SessionSummary, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        // A queued stop here can be a genuine never-reported halt (breakpoint
        // hit after a timed-out continue, with nothing waiting on the
        // channel). Pausing an already-stopped program yields no new event,
        // so the plain dirge-un0g drain would eat the stop and the wait below
        // would time out with the status stuck at Running. Treat the most
        // recent drained stop as the pause result instead.
        if let Some(stopped) = session.drain_stopped_latest() {
            session.status = SessionStatus::Stopped;
            session.record_stopped_thread(&stopped);
            session.drain_output();
            session.drain_termination();

            let mut summary = session.summary();
            summary.stop_reason = Some(stopped.reason.as_str().to_string());
            summary.thread_id = stopped.thread_id.map(|id| id as u32);
            return Ok(summary);
        }

        // dirge-8q3w: resolve the 0-sentinel to the last stopped thread,
        // exactly as continue_/step/stack_trace do — strict adapters
        // (debugpy) reject a literal threadId 0 with "thread not found".
        let args = PauseArgs {
            thread_id: session.resolve_thread_id(thread_id),
        };
        session
            .client
            .request::<_, Value>("pause", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        let stopped = session.wait_for_stopped(timeout).await?;
        session.status = SessionStatus::Stopped;
        session.drain_output();
        session.drain_termination();

        let mut summary = session.summary();
        summary.stop_reason = Some(stopped.reason.as_str().to_string());
        summary.thread_id = stopped.thread_id.map(|id| id as u32);
        Ok(summary)
    }

    /// Get stack trace.
    pub async fn stack_trace(
        &self,
        thread_id: u32,
        levels: Option<u32>,
        timeout: Duration,
    ) -> Result<Vec<StackFrame>, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = StackTraceArgs {
            // dirge-vept: 0 from the bridge means the last stopped thread.
            thread_id: session.resolve_thread_id(thread_id),
            start_frame: None,
            levels,
            format: None,
        };

        let response: StackTraceResponse = session
            .client
            .request("stackTrace", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.cached_frames = response.stack_frames.clone();
        Ok(response.stack_frames)
    }

    /// Get scopes for a frame.
    pub async fn scopes(&self, frame_id: u32, timeout: Duration) -> Result<Vec<Scope>, ToolError> {
        let active = self.active.lock().await;
        let session = active
            .as_ref()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = ScopesArgs { frame_id };
        let response: ScopesResponse = session
            .client
            .request("scopes", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        Ok(response.scopes)
    }

    /// Get variables.
    pub async fn variables(
        &self,
        variables_reference: u32,
        timeout: Duration,
    ) -> Result<Vec<Variable>, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = VariablesArgs {
            variables_reference,
            filter: None,
            start: None,
            count: None,
            format: None,
        };

        let response: VariablesResponse = session
            .client
            .request("variables", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.cached_variables = response.variables.clone();
        Ok(response.variables)
    }

    /// Evaluate expression.
    pub async fn evaluate(
        &self,
        expression: &str,
        frame_id: Option<u32>,
        context: Option<&str>,
        timeout: Duration,
    ) -> Result<EvaluateResponse, ToolError> {
        let active = self.active.lock().await;
        let session = active
            .as_ref()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = EvaluateArgs {
            expression: expression.to_string(),
            frame_id,
            context: context.map(|s| s.to_string()),
            format: None,
        };

        let response: EvaluateResponse = session
            .client
            .request("evaluate", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        Ok(response)
    }

    /// List threads.
    pub async fn threads(&self, timeout: Duration) -> Result<Vec<Thread>, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let response: ThreadsResponse = session
            .client
            .request("threads", &ThreadsArgs {}, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.cached_threads = response.threads.clone();
        Ok(response.threads)
    }

    /// Terminate the debuggee.
    pub async fn terminate(&self, timeout: Duration) -> Result<SessionSummary, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        session
            .client
            .request::<_, Value>("terminate", &TerminateArgs::default(), timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.drain_output();
        session.drain_termination();
        session.status = SessionStatus::Terminated;

        Ok(session.summary())
    }

    /// Disconnect from the debug adapter.
    pub async fn disconnect(&self, restart: bool, timeout: Duration) -> Result<(), ToolError> {
        let mut active = self.active.lock().await;
        let mut result = Ok(());
        if let Some(session) = active.as_mut() {
            let args = DisconnectArgs {
                restart: Some(restart),
                terminate_debuggee: None,
                extra: Default::default(),
            };
            // dirge-ibq5: clear the slot even if the request fails. A crashed
            // adapter returns ConnectionClosed here; propagating that with `?`
            // BEFORE clearing left the dead session stuck active forever — the
            // panel kept showing it and every op errored until a new launch
            // called terminate_active. Mirror terminate_active: attempt the
            // request, mark Terminated, and always clear `active`. The
            // transport error is still returned so the caller learns it failed.
            result = session
                .client
                .request::<_, Value>("disconnect", &args, timeout)
                .await
                .map(|_: Value| ())
                .map_err(rpc_to_tool_error);
            session.status = SessionStatus::Terminated;
        }
        *active = None;
        result
    }

    /// Restart a stack frame — re-execute from the beginning of the frame.
    /// Useful for edit-and-continue workflows after modifying source code.
    pub async fn restart_frame(&self, frame_id: u32, timeout: Duration) -> Result<(), ToolError> {
        let active = self.active.lock().await;
        let session = active
            .as_ref()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = RestartFrameArgs { frame_id };
        session
            .client
            .request::<_, Value>("restartFrame", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        Ok(())
    }

    /// Return a summary of the active session, if any.
    pub async fn active_summary(&self) -> Option<SessionSummary> {
        let active = self.active.lock().await;
        active.as_ref().map(|s| s.summary())
    }

    /// Build a `DebugPanelData` snapshot from the active session's
    /// cached state. Non-async — uses `try_lock` so the UI loop
    /// never blocks waiting for a DAP tool call. Returns `None`
    /// when no session is active or the lock is held by a tool.
    pub fn debug_snapshot(&self) -> Option<DebugPanelData> {
        // If `active` is locked (an op is mid-round-trip), fall back to the
        // last cached snapshot so the panel doesn't blank out for the whole
        // call. On a successful lock, rebuild and refresh the cache.
        let active = match self.active.try_lock() {
            Ok(active) => active,
            Err(_) => {
                return self.last_snapshot.lock_ignore_poison().clone();
            }
        };
        let snapshot = active.as_ref().map(|session| DebugPanelData {
            adapter: session.client.adapter_name.clone(),
            status: session.status.clone(),
            session_summary: Some(session.summary()),
            threads: session.cached_threads.clone(),
            frames: session.cached_frames.clone(),
            variables: session.cached_variables.clone(),
            scopes: Vec::new(),
            breakpoints: session.breakpoints.values().flatten().cloned().collect(),
            output: session.output.clone(),
            output_truncated: session.output_truncated,
            exit_code: session.exit_code,
        });
        *self.last_snapshot.lock_ignore_poison() = snapshot.clone();
        snapshot
    }

    /// Force-terminate the active session (drop = kill_on_drop).
    async fn terminate_active(&self) {
        let mut active = self.active.lock().await;
        if let Some(session) = active.as_mut() {
            // Best-effort graceful disconnect (terminate the debuggee) before
            // dropping, which otherwise hard-SIGKILLs the process group. Short
            // timeout; errors are ignored — the drop is the fallback.
            let args = DisconnectArgs {
                restart: Some(false),
                terminate_debuggee: Some(true),
                extra: Default::default(),
            };
            let _ = session
                .client
                .request::<_, Value>("disconnect", &args, Duration::from_secs(2))
                .await;
        }
        *active = None;
    }
}

/// Best-effort teardown of the active DAP session at process exit
/// (dirge-ixcw). [`DAP_MANAGER`] is a `static`, so its `Drop` never runs
/// on a normal exit — without an explicit call here a `setsid()`-isolated
/// adapter + debuggee in their own process group could be orphaned. Mirrors
/// the LSP `close_all_files` shutdown step. Force-terminates (the manager's
/// `terminate_active` already attempts a 2s graceful disconnect, then drops
/// to `kill_on_drop`) rather than blocking the exit on a slow adapter.
pub async fn shutdown_active_session() {
    let mgr = DAP_MANAGER.lock().ok().and_then(|guard| guard.clone());
    if let Some(mgr) = mgr {
        mgr.terminate_active().await;
    }
}

impl Default for DapSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn rpc_to_tool_error(e: RpcError) -> ToolError {
    match &e {
        RpcError::Server(msg) => ToolError::Msg(format!("adapter error: {msg}")),
        other => ToolError::Msg(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc_framing::{decode_frame, encode_frame};
    use serde_json::Value;
    use tokio::io::{AsyncBufRead, AsyncWrite};

    /// A fake DAP adapter that handles:
    /// 1. initialize request → capabilities response
    /// 2. launch request → success response → stopped event
    /// 3. configurationDone request → success response
    async fn fake_launch_adapter(
        mut reader: impl AsyncBufRead + Unpin,
        mut writer: impl AsyncWrite + Unpin,
    ) {
        // --- initialize ---
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "initialize");
        let seq = msg["seq"].as_u64().unwrap();

        let resp = serde_json::json!({
            "type": "response",
            "seq": 1,
            "request_seq": seq,
            "success": true,
            "command": "initialize",
            "body": {
                "supportsConfigurationDoneRequest": true,
                "supportsFunctionBreakpoints": false,
            }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // --- launch ---
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "launch");
        let seq = msg["seq"].as_u64().unwrap();

        let resp = serde_json::json!({
            "type": "response",
            "seq": 2,
            "request_seq": seq,
            "success": true,
            "command": "launch",
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // Stopped event (stopOnEntry).
        let evt = serde_json::json!({
            "type": "event",
            "seq": 3,
            "event": "stopped",
            "body": {
                "reason": "entry",
                "threadId": 1,
            }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&evt).unwrap())
            .await
            .unwrap();

        // --- configurationDone ---
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "configurationDone");
        let seq = msg["seq"].as_u64().unwrap();

        let resp = serde_json::json!({
            "type": "response",
            "seq": 4,
            "request_seq": seq,
            "success": true,
            "command": "configurationDone",
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // --- setBreakpoints ---
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "setBreakpoints");
        let seq = msg["seq"].as_u64().unwrap();

        let resp = serde_json::json!({
            "type": "response",
            "seq": 5,
            "request_seq": seq,
            "success": true,
            "command": "setBreakpoints",
            "body": {
                "breakpoints": [
                    {"id": 1, "verified": true, "line": 10}
                ]
            }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // --- continue ---
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "continue");
        let seq = msg["seq"].as_u64().unwrap();

        let resp = serde_json::json!({
            "type": "response",
            "seq": 6,
            "request_seq": seq,
            "success": true,
            "command": "continue",
            "body": { "allThreadsContinued": true }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // Stopped event (breakpoint hit).
        let evt = serde_json::json!({
            "type": "event",
            "seq": 7,
            "event": "stopped",
            "body": {
                "reason": "breakpoint",
                "threadId": 1,
            }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&evt).unwrap())
            .await
            .unwrap();

        // --- terminate ---
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "terminate");
        let seq = msg["seq"].as_u64().unwrap();

        let resp = serde_json::json!({
            "type": "response",
            "seq": 8,
            "request_seq": seq,
            "success": true,
            "command": "terminate",
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();
    }

    /// dirge-ibq5: a fake adapter that completes the launch handshake, then
    /// RETURNS — dropping its transport so a subsequent request (e.g.
    /// `disconnect`) fails like a crashed adapter would.
    async fn fake_launch_then_die(
        mut reader: impl AsyncBufRead + Unpin,
        mut writer: impl AsyncWrite + Unpin,
    ) {
        // initialize
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        let seq = msg["seq"].as_u64().unwrap();
        let resp = serde_json::json!({
            "type": "response", "seq": 1, "request_seq": seq, "success": true,
            "command": "initialize",
            "body": { "supportsConfigurationDoneRequest": true }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // launch
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        let seq = msg["seq"].as_u64().unwrap();
        let resp = serde_json::json!({
            "type": "response", "seq": 2, "request_seq": seq, "success": true,
            "command": "launch",
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // stopped (stopOnEntry)
        let evt = serde_json::json!({
            "type": "event", "seq": 3, "event": "stopped",
            "body": { "reason": "entry", "threadId": 1 }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&evt).unwrap())
            .await
            .unwrap();

        // configurationDone
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        let seq = msg["seq"].as_u64().unwrap();
        let resp = serde_json::json!({
            "type": "response", "seq": 4, "request_seq": seq, "success": true,
            "command": "configurationDone",
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();
        // Return — transport drops, the adapter is now "dead".
    }

    /// dirge-ibq5: disconnecting a crashed adapter must clear the active slot,
    /// not leave the session stuck active. The disconnect request fails
    /// (transport gone), but `active` is cleared so subsequent ops see no
    /// session instead of erroring against a dead one forever.
    #[cfg(feature = "dap")]
    #[tokio::test]
    async fn disconnect_clears_active_even_when_adapter_is_dead() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();

        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, server_write) = tokio::io::split(server_side);
        let (rpc, _read_task) = DapRpc::new(tokio::io::BufReader::new(client_read), client_write);
        tokio::spawn(async move {
            fake_launch_then_die(tokio::io::BufReader::new(server_read), server_write).await;
        });
        let client = DapClient::from_rpc(rpc, "fake-adapter");

        let summary = mgr
            .launch_with_client(
                "fake-adapter",
                "/tmp",
                Some("prog"),
                None,
                &[],
                Some(true),
                None,
                &signal,
                client,
                Duration::from_secs(5),
                vec![],
            )
            .await
            .expect("launch should succeed against the fake adapter");
        assert_eq!(summary.status, SessionStatus::Stopped);

        // Adapter is dead now; the disconnect request fails, but the slot must
        // still be cleared (return value may be Err — we only care about state).
        let _ = mgr.disconnect(false, Duration::from_secs(2)).await;

        // Proof the slot is cleared: an op needing an active session reports
        // none, rather than erroring against the dead adapter.
        let err = mgr
            .terminate(Duration::from_secs(2))
            .await
            .expect_err("no session should remain active");
        assert!(
            err.to_string().contains("no active debug session"),
            "active session was not cleared; got: {err}"
        );
    }

    /// Build a DapClient over duplex channels connected to a fake adapter.
    fn client_with_fake_adapter() -> DapClient {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, server_write) = tokio::io::split(server_side);

        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, _read_task) = DapRpc::new(client_reader, client_write);

        tokio::spawn(async move {
            fake_launch_adapter(tokio::io::BufReader::new(server_read), server_write).await;
        });

        DapClient::from_rpc(rpc, "fake-adapter")
    }

    /// Full launch → setBreakpoints → continue → terminate flow over duplex.
    #[tokio::test]
    async fn launch_breakpoint_continue_terminate() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();
        let client = client_with_fake_adapter();

        let summary = mgr
            .launch_with_client(
                "fake-adapter",
                "/tmp",
                Some("test-program"),
                None,
                &[],
                Some(true),
                None,
                &signal,
                client,
                Duration::from_secs(5),
                vec![],
            )
            .await
            .unwrap();

        assert_eq!(summary.status, SessionStatus::Stopped);
        assert_eq!(summary.stop_reason.as_deref(), Some("entry"));
        assert_eq!(summary.thread_id, Some(1));

        // set breakpoints
        let bps = mgr
            .set_breakpoints(
                "/tmp/test.rs",
                vec![SourceBreakpoint {
                    line: 10,
                    column: None,
                    condition: None,
                    hit_condition: None,
                    log_message: None,
                }],
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(bps.len(), 1);
        assert_eq!(bps[0].id, Some(1));
        assert!(bps[0].verified);

        // continue → wait for breakpoint hit
        let outcome = mgr
            .continue_(1, &signal, Duration::from_secs(5))
            .await
            .unwrap();

        assert_eq!(outcome.status, SessionStatus::Stopped);
        assert_eq!(outcome.stop_reason.as_deref(), Some("breakpoint"));

        // terminate
        let term = mgr.terminate(Duration::from_secs(5)).await.unwrap();
        assert_eq!(term.status, SessionStatus::Terminated);
    }

    /// dirge-vept: a fake adapter that stops on thread 42 at entry, then on
    /// `continue` echoes back the `threadId` it actually received (in the next
    /// stopped event's threadId). Lets a test observe what thread id the
    /// manager sent, proving the 0-sentinel was substituted.
    async fn fake_echo_thread_adapter(
        mut reader: impl AsyncBufRead + Unpin,
        mut writer: impl AsyncWrite + Unpin,
    ) {
        // initialize
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        let seq = msg["seq"].as_u64().unwrap();
        let resp = serde_json::json!({
            "type": "response", "seq": 1, "request_seq": seq, "success": true,
            "command": "initialize",
            "body": { "supportsConfigurationDoneRequest": true }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // launch → success → stopped on thread 42
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        let seq = msg["seq"].as_u64().unwrap();
        let resp = serde_json::json!({
            "type": "response", "seq": 2, "request_seq": seq, "success": true,
            "command": "launch",
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();
        let evt = serde_json::json!({
            "type": "event", "seq": 3, "event": "stopped",
            "body": { "reason": "entry", "threadId": 42 }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&evt).unwrap())
            .await
            .unwrap();

        // configurationDone (notify — no response)
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "configurationDone");

        // continue → echo the received threadId into the next stopped event.
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "continue");
        let seq = msg["seq"].as_u64().unwrap();
        let received_tid = msg["arguments"]["threadId"].as_u64().unwrap();
        let resp = serde_json::json!({
            "type": "response", "seq": 4, "request_seq": seq, "success": true,
            "command": "continue", "body": { "allThreadsContinued": true }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();
        let evt = serde_json::json!({
            "type": "event", "seq": 5, "event": "stopped",
            "body": { "reason": "breakpoint", "threadId": received_tid }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&evt).unwrap())
            .await
            .unwrap();
    }

    fn client_with_echo_thread_adapter() -> DapClient {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, server_write) = tokio::io::split(server_side);
        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, _read_task) = DapRpc::new(client_reader, client_write);
        tokio::spawn(async move {
            fake_echo_thread_adapter(tokio::io::BufReader::new(server_read), server_write).await;
        });
        DapClient::from_rpc(rpc, "fake-adapter")
    }

    /// dirge-vept: the Janet bridge calls `continue_(0, …)`. A `thread_id` of
    /// 0 is the "unspecified" sentinel and must be substituted with the last
    /// stopped thread (42 here), or strict adapters (debugpy) reject it with
    /// "thread not found". The echo adapter reports the id it received.
    #[tokio::test]
    async fn continue_with_zero_thread_substitutes_last_stopped() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();
        let client = client_with_echo_thread_adapter();

        let summary = mgr
            .launch_with_client(
                "fake-adapter",
                "/tmp",
                Some("p"),
                None,
                &[],
                Some(true),
                None,
                &signal,
                client,
                Duration::from_secs(5),
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(summary.thread_id, Some(42));

        // Bridge passes 0; the manager must send the last stopped thread.
        let outcome = mgr
            .continue_(0, &signal, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            outcome.thread_id,
            Some(42),
            "continue(0) must target the last stopped thread, not 0"
        );
    }

    /// dirge-8q3w: pause echo adapter. After the entry stop on thread 42
    /// it answers a `pause` by echoing the received threadId into the
    /// resulting stopped event, so the test can see which id pause sent.
    async fn fake_echo_pause_adapter(
        mut reader: impl AsyncBufRead + Unpin,
        mut writer: impl AsyncWrite + Unpin,
    ) {
        // initialize
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        let seq = msg["seq"].as_u64().unwrap();
        encode_frame(
            &mut writer,
            &serde_json::to_vec(&serde_json::json!({
                "type": "response", "seq": 1, "request_seq": seq, "success": true,
                "command": "initialize",
                "body": { "supportsConfigurationDoneRequest": true }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        // launch → success → stopped on thread 42
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        let seq = msg["seq"].as_u64().unwrap();
        encode_frame(
            &mut writer,
            &serde_json::to_vec(&serde_json::json!({
                "type": "response", "seq": 2, "request_seq": seq, "success": true,
                "command": "launch",
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        encode_frame(
            &mut writer,
            &serde_json::to_vec(&serde_json::json!({
                "type": "event", "seq": 3, "event": "stopped",
                "body": { "reason": "entry", "threadId": 42 }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        // configurationDone (notify — no response)
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "configurationDone");

        // pause → echo the received threadId into the stopped event.
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "pause");
        let seq = msg["seq"].as_u64().unwrap();
        let received_tid = msg["arguments"]["threadId"].as_u64().unwrap();
        encode_frame(
            &mut writer,
            &serde_json::to_vec(&serde_json::json!({
                "type": "response", "seq": 4, "request_seq": seq, "success": true,
                "command": "pause"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        encode_frame(
            &mut writer,
            &serde_json::to_vec(&serde_json::json!({
                "type": "event", "seq": 5, "event": "stopped",
                "body": { "reason": "pause", "threadId": received_tid }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    }

    fn client_with_echo_pause_adapter() -> DapClient {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, server_write) = tokio::io::split(server_side);
        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, _read_task) = DapRpc::new(client_reader, client_write);
        tokio::spawn(async move {
            fake_echo_pause_adapter(tokio::io::BufReader::new(server_read), server_write).await;
        });
        DapClient::from_rpc(rpc, "fake-adapter")
    }

    /// dirge-8q3w: pause built `PauseArgs { thread_id }` verbatim, skipping
    /// the `resolve_thread_id` its sibling ops (continue/step/stackTrace)
    /// apply. The debug tool defaults pause to thread 0, and strict
    /// adapters reject threadId 0 with "thread not found". pause(0) must
    /// target the last stopped thread (42), not 0.
    #[tokio::test]
    async fn pause_with_zero_thread_substitutes_last_stopped() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();
        let client = client_with_echo_pause_adapter();

        let summary = mgr
            .launch_with_client(
                "fake-adapter",
                "/tmp",
                Some("p"),
                None,
                &[],
                Some(true),
                None,
                &signal,
                client,
                Duration::from_secs(5),
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(summary.thread_id, Some(42));

        // The debug tool / Janet bridge passes 0; pause must send the last
        // stopped thread instead.
        let outcome = mgr.pause(0, Duration::from_secs(5)).await.unwrap();
        assert_eq!(
            outcome.thread_id,
            Some(42),
            "pause(0) must target the last stopped thread, not 0"
        );
    }

    /// Session summary reflects the active session.
    #[tokio::test]
    async fn active_summary_after_launch() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();
        let client = client_with_fake_adapter();

        let summary = mgr
            .launch_with_client(
                "fake-adapter",
                "/tmp",
                Some("hello"),
                None,
                &[],
                Some(true),
                None,
                &signal,
                client,
                Duration::from_secs(5),
                vec![],
            )
            .await
            .unwrap();

        assert_eq!(summary.status, SessionStatus::Stopped);

        let active = mgr.active_summary().await;
        assert!(active.is_some());
        assert_eq!(active.unwrap().id, summary.id);
    }

    /// Single-session enforcement: launching a new session drops the old one.
    #[tokio::test]
    async fn second_launch_replaces_first() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();
        let client = client_with_fake_adapter();

        let first = mgr
            .launch_with_client(
                "fake-adapter",
                "/tmp",
                Some("first"),
                None,
                &[],
                Some(true),
                None,
                &signal,
                client,
                Duration::from_secs(5),
                vec![],
            )
            .await
            .unwrap();

        let first_id = first.id;

        let active = mgr.active_summary().await;
        assert!(active.is_some());
        assert_eq!(active.unwrap().id, first_id);

        // Manually clear to verify terminate_active works
        mgr.terminate_active().await;
        assert!(mgr.active_summary().await.is_none());
    }

    /// E2E: DapSessionManager::launch_with_client against real debugpy.
    /// Reproduces dirge-go4b timeout bug.
    #[tokio::test]
    async fn e2e_debugpy_launch_with_client() {
        if std::process::Command::new("python3")
            .args(["-c", "import debugpy"])
            .output()
            .map_or(true, |o| !o.status.success())
        {
            eprintln!("SKIP: debugpy not installed");
            return;
        }

        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("tests")
            .join("dap")
            .join("fixtures")
            .join("test_program.py");
        assert!(fixture.exists(), "test_program.py must exist");

        let client = DapClient::spawn_stdio(
            "debugpy",
            std::path::Path::new("python3"),
            &["-m".to_string(), "debugpy.adapter".to_string()],
            std::path::Path::new("."),
        )
        .await
        .expect("debugpy adapter should spawn");

        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();

        let summary = mgr
            .launch_with_client(
                "debugpy",
                ".",
                Some(fixture.to_str().unwrap()),
                None,
                &[],
                Some(true),
                None,
                &signal,
                client,
                std::time::Duration::from_secs(15),
                vec!["python".into()],
            )
            .await
            .expect("launch_with_client should succeed");

        assert_eq!(summary.status, SessionStatus::Stopped);
        assert!(summary.stop_reason.is_some(), "should have stop reason");

        // Terminate and disconnect.
        mgr.terminate(std::time::Duration::from_secs(10))
            .await
            .expect("terminate should succeed");

        mgr.disconnect(false, std::time::Duration::from_secs(10))
            .await
            .expect("disconnect should succeed");
    }

    /// Launch a Python module (python -m test_mod) via debugpy using
    /// launch_with_client and exercise the full session-manager lifecycle.
    #[cfg(feature = "dap")]
    #[tokio::test]
    async fn e2e_debugpy_launch_module() {
        if std::process::Command::new("python3")
            .args(["-c", "import debugpy"])
            .output()
            .map_or(true, |o| !o.status.success())
        {
            eprintln!("SKIP: debugpy not installed");
            return;
        }

        let fixtures_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("tests")
            .join("dap")
            .join("fixtures");
        assert!(
            fixtures_dir.join("test_mod").exists(),
            "test_mod package must exist"
        );

        let client = DapClient::spawn_stdio(
            "debugpy",
            std::path::Path::new("python3"),
            &["-m".to_string(), "debugpy.adapter".to_string()],
            std::path::Path::new("."),
        )
        .await
        .expect("debugpy adapter should spawn");

        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();

        // launch with module instead of program — debugpy runs python -m test_mod
        let summary = mgr
            .launch_with_client(
                "debugpy",
                fixtures_dir.to_str().unwrap(),
                None,             // program: None
                Some("test_mod"), // module: Some("test_mod")
                &[],
                Some(true), // stop_on_entry
                None,
                &signal,
                client,
                std::time::Duration::from_secs(15),
                vec!["python".into()],
            )
            .await
            .expect("launch_with_client should succeed");

        assert_eq!(summary.status, SessionStatus::Stopped);
        assert!(summary.stop_reason.is_some(), "should have stop reason");

        // Terminate and disconnect.
        mgr.terminate(std::time::Duration::from_secs(10))
            .await
            .expect("terminate should succeed");

        mgr.disconnect(false, std::time::Duration::from_secs(10))
            .await
            .expect("disconnect should succeed");
    }

    // -----------------------------------------------------------------------
    // dirge-un0g: stale queued stopped events must be drained before the next
    // continue/step/pause, or they satisfy the wait instantly and every op is
    // reported one stop behind.
    // -----------------------------------------------------------------------

    /// Launch handshake (init → launch → stopped(entry, 1) → configurationDone)
    /// then queues a STALE stopped event before answering `continue` with a
    /// fresh breakpoint stop on thread 1.
    async fn fake_stale_stop_adapter(
        mut reader: impl AsyncBufRead + Unpin,
        mut writer: impl AsyncWrite + Unpin,
    ) {
        // initialize
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        let seq = msg["seq"].as_u64().unwrap();
        encode_frame(
            &mut writer,
            &serde_json::to_vec(&serde_json::json!({
                "type": "response", "seq": 1, "request_seq": seq, "success": true,
                "command": "initialize",
                "body": { "supportsConfigurationDoneRequest": true }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        // launch (notify) → stopped on entry, thread 1
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "launch");
        encode_frame(
            &mut writer,
            &serde_json::to_vec(&serde_json::json!({
                "type": "event", "seq": 2, "event": "stopped",
                "body": { "reason": "entry", "threadId": 1 }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        // configurationDone (notify)
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "configurationDone");

        // A stale stopped event, as if a second thread halted alongside the
        // first. Nothing has consumed it — it sits queued in the channel.
        encode_frame(
            &mut writer,
            &serde_json::to_vec(&serde_json::json!({
                "type": "event", "seq": 3, "event": "stopped",
                "body": { "reason": "step", "threadId": 99 }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        // continue → respond, then send the genuine fresh stop.
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "continue");
        let seq = msg["seq"].as_u64().unwrap();
        encode_frame(
            &mut writer,
            &serde_json::to_vec(&serde_json::json!({
                "type": "response", "seq": 4, "request_seq": seq, "success": true,
                "command": "continue", "body": { "allThreadsContinued": true }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        encode_frame(
            &mut writer,
            &serde_json::to_vec(&serde_json::json!({
                "type": "event", "seq": 5, "event": "stopped",
                "body": { "reason": "breakpoint", "threadId": 1 }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    }

    fn client_with_stale_stop_adapter() -> DapClient {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, server_write) = tokio::io::split(server_side);
        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, _read_task) = DapRpc::new(client_reader, client_write);
        tokio::spawn(async move {
            fake_stale_stop_adapter(tokio::io::BufReader::new(server_read), server_write).await;
        });
        DapClient::from_rpc(rpc, "fake-adapter")
    }

    #[tokio::test]
    async fn continue_drains_stale_stopped_and_reports_the_fresh_stop() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();
        let client = client_with_stale_stop_adapter();

        let summary = mgr
            .launch_with_client(
                "fake-adapter",
                "/tmp",
                Some("p"),
                None,
                &[],
                Some(true),
                None,
                &signal,
                client,
                Duration::from_secs(5),
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(summary.stop_reason.as_deref(), Some("entry"));

        // Let the stale stopped event land in the channel before we continue.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let outcome = mgr
            .continue_(1, &signal, Duration::from_secs(5))
            .await
            .unwrap();

        // Without draining, the queued step/thread-99 event would satisfy the
        // wait instantly. The fix drains it and reports the fresh breakpoint.
        assert_eq!(
            outcome.stop_reason.as_deref(),
            Some("breakpoint"),
            "continue must report the fresh stop, not a stale queued one"
        );
        assert_eq!(outcome.thread_id, Some(1));
    }

    // -----------------------------------------------------------------------
    // dirge-p3r7: attach must not stall the full request timeout waiting for a
    // stop that a running process never sends, nor record a running debuggee
    // as Stopped.
    // -----------------------------------------------------------------------

    async fn attach_handshake(
        reader: &mut (impl AsyncBufRead + Unpin),
        writer: &mut (impl AsyncWrite + Unpin),
    ) {
        // initialize
        let frame = decode_frame(reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        let seq = msg["seq"].as_u64().unwrap();
        encode_frame(
            writer,
            &serde_json::to_vec(&serde_json::json!({
                "type": "response", "seq": 1, "request_seq": seq, "success": true,
                "command": "initialize",
                "body": { "supportsConfigurationDoneRequest": true }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        // attach (request) → success
        let frame = decode_frame(reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "attach");
        let seq = msg["seq"].as_u64().unwrap();
        encode_frame(
            writer,
            &serde_json::to_vec(&serde_json::json!({
                "type": "response", "seq": 2, "request_seq": seq, "success": true,
                "command": "attach"
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        // configurationDone (notify)
        let frame = decode_frame(reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "configurationDone");
    }

    fn client_with_attach_adapter<F, Fut>(body: F) -> DapClient
    where
        F: FnOnce(
                tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
                tokio::io::WriteHalf<tokio::io::DuplexStream>,
            ) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, server_write) = tokio::io::split(server_side);
        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, _read_task) = DapRpc::new(client_reader, client_write);
        tokio::spawn(async move {
            body(tokio::io::BufReader::new(server_read), server_write).await;
        });
        DapClient::from_rpc(rpc, "fake-adapter")
    }

    #[tokio::test]
    async fn attach_to_running_process_returns_promptly_as_running() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();
        // Adapter completes the handshake but never sends a stopped event, then
        // holds the connection open past the grace window.
        let client = client_with_attach_adapter(|mut reader, mut writer| async move {
            attach_handshake(&mut reader, &mut writer).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        // A generous request timeout — the fix must not wait it out.
        let summary = tokio::time::timeout(
            Duration::from_secs(3),
            mgr.attach_with_client(
                "fake-adapter",
                "/tmp",
                Some(1234),
                None,
                None,
                None,
                &signal,
                client,
                Duration::from_secs(30),
                vec![],
            ),
        )
        .await
        .expect("attach must return within the grace window, not the full timeout")
        .unwrap();

        assert_eq!(
            summary.status,
            SessionStatus::Running,
            "a running debuggee with no stop event must be reported Running, not Stopped"
        );
        assert!(summary.stop_reason.is_none());
    }

    #[tokio::test]
    async fn attach_that_stops_immediately_reports_stopped() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();
        let client = client_with_attach_adapter(|mut reader, mut writer| async move {
            attach_handshake(&mut reader, &mut writer).await;
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "event", "seq": 3, "event": "stopped",
                    "body": { "reason": "breakpoint", "threadId": 7 }
                }))
                .unwrap(),
            )
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let summary = mgr
            .attach_with_client(
                "fake-adapter",
                "/tmp",
                Some(1234),
                None,
                None,
                None,
                &signal,
                client,
                Duration::from_secs(30),
                vec![],
            )
            .await
            .unwrap();

        assert_eq!(summary.status, SessionStatus::Stopped);
        assert_eq!(summary.stop_reason.as_deref(), Some("breakpoint"));
        assert_eq!(summary.thread_id, Some(7));
    }

    // -----------------------------------------------------------------------
    // dirge-acgj: continue must not hold the `active` lock across its stop-wait,
    // or a concurrent request (pause, snapshot) blocks for continue's whole
    // timeout — exactly the window where interrupting a free-running program
    // matters.
    // -----------------------------------------------------------------------

    /// Launch handshake, then answers `continue` but withholds the stop until
    /// `release` fires — simulating a free-running program.
    fn client_with_free_running_adapter(release: tokio::sync::oneshot::Receiver<()>) -> DapClient {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, server_write) = tokio::io::split(server_side);
        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, _read_task) = DapRpc::new(client_reader, client_write);
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(server_read);
            let mut writer = server_write;

            // initialize
            let frame = decode_frame(&mut reader).await.unwrap();
            let msg: Value = serde_json::from_slice(&frame).unwrap();
            let seq = msg["seq"].as_u64().unwrap();
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "response", "seq": 1, "request_seq": seq, "success": true,
                    "command": "initialize",
                    "body": { "supportsConfigurationDoneRequest": true }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

            // launch → stopped(entry, 1)
            let frame = decode_frame(&mut reader).await.unwrap();
            let msg: Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(msg["command"], "launch");
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "event", "seq": 2, "event": "stopped",
                    "body": { "reason": "entry", "threadId": 1 }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

            // configurationDone
            let frame = decode_frame(&mut reader).await.unwrap();
            let msg: Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(msg["command"], "configurationDone");

            // continue → respond, but withhold the stop until released.
            let frame = decode_frame(&mut reader).await.unwrap();
            let msg: Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(msg["command"], "continue");
            let seq = msg["seq"].as_u64().unwrap();
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "response", "seq": 3, "request_seq": seq, "success": true,
                    "command": "continue", "body": { "allThreadsContinued": true }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

            let _ = release.await;
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "event", "seq": 4, "event": "stopped",
                    "body": { "reason": "breakpoint", "threadId": 1 }
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        });
        DapClient::from_rpc(rpc, "fake-adapter")
    }

    #[tokio::test]
    async fn continue_does_not_hold_the_active_lock_while_parked() {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mgr = std::sync::Arc::new(DapSessionManager::new());
        let signal = AbortSignal::new();
        let client = client_with_free_running_adapter(release_rx);

        mgr.launch_with_client(
            "fake-adapter",
            "/tmp",
            Some("p"),
            None,
            &[],
            Some(true),
            None,
            &signal,
            client,
            Duration::from_secs(5),
            vec![],
        )
        .await
        .unwrap();

        // Park a continue that will not return until we release the stop.
        let mgr2 = mgr.clone();
        let cont = tokio::spawn(async move {
            let signal = AbortSignal::new();
            mgr2.continue_(1, &signal, Duration::from_secs(5)).await
        });

        // Let continue issue its request and reach the parked stop-wait.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The active lock must be free while continue parks — a concurrent
        // request completes instead of blocking for continue's full timeout.
        let summary = tokio::time::timeout(Duration::from_secs(1), mgr.active_summary())
            .await
            .expect("active_summary must not block while continue is parked (dirge-acgj)")
            .expect("a session is active");
        assert_eq!(summary.status, SessionStatus::Running);

        // Release the stop; continue completes normally.
        release_tx.send(()).unwrap();
        let outcome = cont.await.unwrap().unwrap();
        assert_eq!(outcome.stop_reason.as_deref(), Some("breakpoint"));
    }

    /// A second continue issued while the first is parked on its stop-wait
    /// must be rejected in phase 1, under the lock, before any request
    /// reaches the adapter — otherwise it swaps out the dead placeholder as
    /// its "receivers" (instant false disconnect) and clears
    /// `stop_wait_in_flight` out from under the first waiter.
    #[tokio::test]
    async fn second_continue_while_parked_is_rejected() {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mgr = std::sync::Arc::new(DapSessionManager::new());
        let signal = AbortSignal::new();
        let client = client_with_free_running_adapter(release_rx);

        mgr.launch_with_client(
            "fake-adapter",
            "/tmp",
            Some("p"),
            None,
            &[],
            Some(true),
            None,
            &signal,
            client,
            Duration::from_secs(5),
            vec![],
        )
        .await
        .unwrap();

        // Park the first continue.
        let mgr2 = mgr.clone();
        let cont = tokio::spawn(async move {
            let signal = AbortSignal::new();
            mgr2.continue_(1, &signal, Duration::from_secs(5)).await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The second continue must bail with the guard error, quickly.
        let err = mgr
            .continue_(1, &signal, Duration::from_secs(1))
            .await
            .expect_err("a second concurrent continue must be rejected");
        assert!(
            err.to_string().contains("already waiting"),
            "unexpected error: {err}"
        );

        // The first continue is unaffected: release the stop, it reports it.
        release_tx.send(()).unwrap();
        let outcome = cont.await.unwrap().unwrap();
        assert_eq!(outcome.stop_reason.as_deref(), Some("breakpoint"));
    }

    /// dirge-8gdv: a `continue_` parked in phase 2 must not, in phase 3,
    /// restore its taken receivers / clear `stop_wait_in_flight` / record the
    /// stop into a session that was replaced (launch/attach) while it was
    /// parked. Phase 3 must check session identity and bail when the active
    /// session is no longer the one the continue started under — otherwise it
    /// overwrites the new session's live event receivers with the old client's
    /// channels and the new session never sees another event.
    #[tokio::test]
    async fn continue_does_not_clobber_a_replacement_session() {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mgr = std::sync::Arc::new(DapSessionManager::new());
        let signal = AbortSignal::new();
        let client = client_with_free_running_adapter(release_rx);

        // Install session #1 (id "dap-1"), stopped on entry.
        mgr.launch_with_client(
            "fake-adapter",
            "/tmp",
            Some("p"),
            None,
            &[],
            Some(true),
            None,
            &signal,
            client,
            Duration::from_secs(5),
            vec![],
        )
        .await
        .unwrap();

        // Park a continue_ on session #1: the free-running adapter responds to
        // `continue` and withholds the stop until `release_tx` fires, so phase
        // 2 parks with the `active` lock released.
        let mgr2 = mgr.clone();
        let cont = tokio::spawn(async move {
            let signal = AbortSignal::new();
            mgr2.continue_(1, &signal, Duration::from_secs(5)).await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;

        // While continue_ is parked, simulate a launch/attach swapping in a
        // FRESH session with a different id and its own live event receivers.
        // Take session #1 out of `active` without dropping it so its
        // client/senders stay alive and the parked continue wakes on the real
        // stop (not a disconnect) — the race the fix targets.
        let (stopped_tx_new, stopped_rx_new) = mpsc::unbounded_channel::<StoppedEventBody>();
        let (_output_tx, output_rx) = mpsc::unbounded_channel();
        let (_terminated_tx, terminated_rx) = mpsc::unbounded_channel();
        let (_exited_tx, exited_rx) = mpsc::unbounded_channel();
        let replacement = DapSession {
            id: mgr.next_id(), // "dap-2", differs from the parked session's "dap-1"
            client: client_with_fake_adapter(),
            status: SessionStatus::Running,
            breakpoints: HashMap::new(),
            function_breakpoints: Vec::new(),
            output: String::new(),
            output_truncated: false,
            exit_code: None,
            events: EventReceivers {
                stopped: stopped_rx_new,
                output: output_rx,
                terminated: terminated_rx,
                exited: exited_rx,
            },
            cached_threads: Vec::new(),
            cached_frames: Vec::new(),
            cached_variables: Vec::new(),
            last_stopped_thread_id: None,
            languages: Vec::new(),
            stop_wait_in_flight: false,
        };
        let _parked_session = {
            let mut active = mgr.active.lock().await;
            let old = active.take();
            *active = Some(replacement);
            old
        };

        // Drive the parked continue_ to completion by releasing session #1's
        // stop. After the fix it bails on the id mismatch; before the fix it
        // clobbers the replacement first. The return value is not the point —
        // the invariant checked below is.
        release_tx.send(()).unwrap();
        let _ = cont.await;

        // The replacement session must be completely untouched.
        let mut active = mgr.active.lock().await;
        let session = active.as_mut().expect("replacement session still active");
        assert_eq!(session.id, "dap-2", "replacement id must be unchanged");
        assert!(
            !session.stop_wait_in_flight,
            "replacement's stop_wait_in_flight must stay clear"
        );
        assert_eq!(
            session.status,
            SessionStatus::Running,
            "replacement status must be unchanged"
        );
        let stop_body: StoppedEventBody = serde_json::from_value(serde_json::json!({
            "reason": "breakpoint",
            "threadId": 1
        }))
        .unwrap();
        let send_ok = stopped_tx_new.send(stop_body).is_ok();
        assert!(
            send_ok,
            "replacement's stopped receiver was dropped/clobbered by phase 3"
        );
        assert!(
            session.events.stopped.try_recv().is_ok(),
            "replacement's live receivers must still receive an event \
             (phase 3 must not have swapped in the old client's channels)"
        );
    }

    /// Launch handshake, then `continue` (respond, withhold the stop until
    /// `release` fires), then a `next` step (respond → stopped(step)). Lets a
    /// test cancel the parked continue and verify the session recovers once
    /// the adapter finally stops.
    fn client_with_cancel_recovery_adapter(
        release: tokio::sync::oneshot::Receiver<()>,
    ) -> DapClient {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, server_write) = tokio::io::split(server_side);
        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, _read_task) = DapRpc::new(client_reader, client_write);
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(server_read);
            let mut writer = server_write;

            // initialize
            let frame = decode_frame(&mut reader).await.unwrap();
            let msg: Value = serde_json::from_slice(&frame).unwrap();
            let seq = msg["seq"].as_u64().unwrap();
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "response", "seq": 1, "request_seq": seq, "success": true,
                    "command": "initialize",
                    "body": { "supportsConfigurationDoneRequest": true }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

            // launch → stopped(entry, 1)
            let frame = decode_frame(&mut reader).await.unwrap();
            let msg: Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(msg["command"], "launch");
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "event", "seq": 2, "event": "stopped",
                    "body": { "reason": "entry", "threadId": 1 }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

            // configurationDone
            let frame = decode_frame(&mut reader).await.unwrap();
            let msg: Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(msg["command"], "configurationDone");

            // continue → respond, withhold the stop until released.
            let frame = decode_frame(&mut reader).await.unwrap();
            let msg: Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(msg["command"], "continue");
            let seq = msg["seq"].as_u64().unwrap();
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "response", "seq": 3, "request_seq": seq, "success": true,
                    "command": "continue", "body": { "allThreadsContinued": true }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

            let _ = release.await;
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "event", "seq": 4, "event": "stopped",
                    "body": { "reason": "breakpoint", "threadId": 1 }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

            // A later step must still work: next → respond → stopped(step).
            let frame = decode_frame(&mut reader).await.unwrap();
            let msg: Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(msg["command"], "next");
            let seq = msg["seq"].as_u64().unwrap();
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "response", "seq": 5, "request_seq": seq, "success": true,
                    "command": "next"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "event", "seq": 6, "event": "stopped",
                    "body": { "reason": "step", "threadId": 1 }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

            // Hold the connection open.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        DapClient::from_rpc(rpc, "fake-adapter")
    }

    /// The agent tool executor drops the tool future on cancel. Dropping a
    /// parked `continue_` must not destroy the live event receivers or leave
    /// `stop_wait_in_flight` set — the detached stop-wait must still restore
    /// state when the adapter eventually stops, and a later step must work.
    #[tokio::test]
    async fn cancelled_continue_leaves_session_usable() {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mgr = std::sync::Arc::new(DapSessionManager::new());
        let signal = AbortSignal::new();
        let client = client_with_cancel_recovery_adapter(release_rx);

        mgr.launch_with_client(
            "fake-adapter",
            "/tmp",
            Some("p"),
            None,
            &[],
            Some(true),
            None,
            &signal,
            client,
            Duration::from_secs(5),
            vec![],
        )
        .await
        .unwrap();

        // Park a continue, then drop its future mid-park (what the tool
        // executor's cancel race does).
        let mgr2 = mgr.clone();
        let cont = tokio::spawn(async move {
            let signal = AbortSignal::new();
            mgr2.continue_(1, &signal, Duration::from_secs(5)).await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        cont.abort();
        let _ = cont.await;

        // Adapter finally reports the stop the continue induced.
        release_tx.send(()).unwrap();

        // The detached stop-wait must consume it, restore the receivers, and
        // clear stop_wait_in_flight. Poll until the status flips.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let summary = mgr.active_summary().await.expect("session still active");
            if summary.status == SessionStatus::Stopped {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "stop never recorded after cancelled continue; session wedged"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // A later step must succeed — not deflected by a stuck
        // stop_wait_in_flight, not parked on dead receivers.
        let summary = mgr
            .step_over(1, &signal, Duration::from_secs(3))
            .await
            .expect("step after a cancelled continue must succeed");
        assert_eq!(summary.stop_reason.as_deref(), Some("step"));
    }

    /// Launch handshake, then queues a never-reported stopped event
    /// (breakpoint on thread 5). If a pause request arrives anyway (the buggy
    /// path), respond success but send no event so the wait times out instead
    /// of hanging.
    fn client_with_queued_stop_adapter() -> DapClient {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, server_write) = tokio::io::split(server_side);
        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, _read_task) = DapRpc::new(client_reader, client_write);
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(server_read);
            let mut writer = server_write;

            // initialize
            let frame = decode_frame(&mut reader).await.unwrap();
            let msg: Value = serde_json::from_slice(&frame).unwrap();
            let seq = msg["seq"].as_u64().unwrap();
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "response", "seq": 1, "request_seq": seq, "success": true,
                    "command": "initialize",
                    "body": { "supportsConfigurationDoneRequest": true }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

            // launch → stopped(entry, 1)
            let frame = decode_frame(&mut reader).await.unwrap();
            let msg: Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(msg["command"], "launch");
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "event", "seq": 2, "event": "stopped",
                    "body": { "reason": "entry", "threadId": 1 }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

            // configurationDone
            let frame = decode_frame(&mut reader).await.unwrap();
            let msg: Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(msg["command"], "configurationDone");

            // A genuine stop nobody waited for (e.g. breakpoint hit after a
            // timed-out continue). It sits queued in the stopped channel.
            encode_frame(
                &mut writer,
                &serde_json::to_vec(&serde_json::json!({
                    "type": "event", "seq": 3, "event": "stopped",
                    "body": { "reason": "breakpoint", "threadId": 5 }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

            // Buggy path only: answer a pause but never send a stop, so the
            // wait times out rather than hanging the test.
            if let Ok(frame) = decode_frame(&mut reader).await {
                let msg: Value = serde_json::from_slice(&frame).unwrap();
                if msg["command"] == "pause" {
                    let seq = msg["seq"].as_u64().unwrap();
                    let _ = encode_frame(
                        &mut writer,
                        &serde_json::to_vec(&serde_json::json!({
                            "type": "response", "seq": 4, "request_seq": seq,
                            "success": true, "command": "pause"
                        }))
                        .unwrap(),
                    )
                    .await;
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        DapClient::from_rpc(rpc, "fake-adapter")
    }

    /// A stop queued with no waiter (status still Running) must not be eaten
    /// by pause's stale-stop drain: pausing an already-stopped program yields
    /// no new event, so the drained stop IS the pause result.
    #[tokio::test]
    async fn pause_returns_a_drained_never_reported_stop() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();
        let client = client_with_queued_stop_adapter();

        let summary = mgr
            .launch_with_client(
                "fake-adapter",
                "/tmp",
                Some("p"),
                None,
                &[],
                Some(true),
                None,
                &signal,
                client,
                Duration::from_secs(5),
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(summary.stop_reason.as_deref(), Some("entry"));

        // Let the never-reported stop land in the channel.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let summary = mgr
            .pause(1, Duration::from_secs(2))
            .await
            .expect("pause must report the drained stop, not time out");
        assert_eq!(summary.status, SessionStatus::Stopped);
        assert_eq!(summary.stop_reason.as_deref(), Some("breakpoint"));
        assert_eq!(summary.thread_id, Some(5));
    }
} // mod tests
