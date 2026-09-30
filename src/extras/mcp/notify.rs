//! Server-to-client MCP notifications: progress and logging.
//!
//! Two layers:
//! - pure mapping ([`ProgressCoalescer`], [`format_progress`], [`route_log`])
//!   with no I/O, unit-tested directly;
//! - the rmcp boundary ([`McpClientHandler`]) that feeds notifications
//!   through the pure layer into a [`ClientEventSink`] port. Production
//!   uses [`UiSink`] (UI notification channel + tracing); tests use a
//!   recording sink.
//!
//! Progress tokens are allocated per `tools/call` by [`ProgressRegistry`],
//! which remembers token -> tool name so progress lines can name the tool.

// Logging notifications are SEP-2577-deprecated in rmcp 3.x but still sent
// by servers in the wild.
#![allow(deprecated)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    ClientInfo, LoggingLevel, LoggingMessageNotificationParam, ProgressNotificationParam,
    ProgressToken,
};
use rmcp::service::{NotificationContext, RoleClient};

use crate::ui::notifications::Notification;

/// Minimum spacing between two progress lines for the same token.
pub const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(500);
/// Upper bound for one rendered notification line (chars).
const MAX_LINE_CHARS: usize = 512;
/// Coalescer entries older than this are pruned once the map grows.
const COALESCE_TTL: Duration = Duration::from_secs(120);
const COALESCE_PRUNE_AT: usize = 256;

// ---------------------------------------------------------------------------
// Progress token registry
// ---------------------------------------------------------------------------

/// Tracks in-flight tool calls per server so progress lines can name the
/// tool.
///
/// rmcp 3.1.1 assigns `_meta.progressToken` itself (a per-peer counter,
/// unique per request) and overwrites any caller-supplied token; the
/// high-level `call_tool` does not expose the assigned token. So the
/// token cannot be mapped to a tool before sending. Instead: when exactly
/// one call is in flight on a server, its progress is attributed to that
/// tool; with several concurrent calls the line stays generic.
#[derive(Default)]
pub struct ProgressRegistry {
    next: AtomicU64,
    inflight: Mutex<HashMap<String, Vec<(u64, String)>>>,
}

impl ProgressRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a call to `tool` on `server`; it stays in flight until the
    /// returned guard drops.
    pub fn issue(self: &Arc<Self>, server: &str, tool: &str) -> InflightGuard {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(server.to_string())
            .or_default()
            .push((id, tool.to_string()));
        InflightGuard {
            registry: Arc::clone(self),
            server: server.to_string(),
            id,
        }
    }

    /// The tool in flight on `server`, if exactly one is.
    pub fn tool_for(&self, server: &str) -> Option<String> {
        let map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(server).map(Vec::as_slice) {
            Some([(_, tool)]) => Some(tool.clone()),
            _ => None,
        }
    }

    fn release(&self, server: &str, id: u64) {
        let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = map.get_mut(server) {
            v.retain(|(i, _)| *i != id);
            if v.is_empty() {
                map.remove(server);
            }
        }
    }
}

/// Keeps one call registered as in flight.
pub struct InflightGuard {
    registry: Arc<ProgressRegistry>,
    server: String,
    id: u64,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.registry.release(&self.server, self.id);
    }
}

/// Process-wide registry shared by every connection; survives reconnects.
pub fn global_registry() -> Arc<ProgressRegistry> {
    static GLOBAL: LazyLock<Arc<ProgressRegistry>> =
        LazyLock::new(|| Arc::new(ProgressRegistry::new()));
    Arc::clone(&GLOBAL)
}

// ---------------------------------------------------------------------------
// Pure mapping
// ---------------------------------------------------------------------------

/// Rate limiter for progress lines, keyed by progress token. The first
/// update and the final one (progress >= total) always pass; updates in
/// between pass at most once per `min_interval`.
pub struct ProgressCoalescer {
    min_interval: Duration,
    last: HashMap<ProgressToken, Instant>,
}

impl ProgressCoalescer {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last: HashMap::new(),
        }
    }

    pub fn admit(&mut self, token: &ProgressToken, is_final: bool, now: Instant) -> bool {
        if self.last.len() >= COALESCE_PRUNE_AT {
            self.last
                .retain(|_, t| now.saturating_duration_since(*t) < COALESCE_TTL);
        }
        if is_final {
            self.last.remove(token);
            return true;
        }
        match self.last.get(token) {
            Some(prev) if now.saturating_duration_since(*prev) < self.min_interval => false,
            _ => {
                self.last.insert(token.clone(), now);
                true
            }
        }
    }
}

/// True when a progress update reports completion.
pub fn is_final_progress(progress: f64, total: Option<f64>) -> bool {
    matches!(total, Some(t) if t > 0.0 && progress >= t)
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.1}")
    }
}

fn clean(s: &str) -> String {
    let s = crate::ui::ansi::strip_controls(s, crate::ui::ansi::StripPolicy::STRICT).to_string();
    if s.chars().count() > MAX_LINE_CHARS {
        let mut t: String = s.chars().take(MAX_LINE_CHARS).collect();
        t.push('…');
        t
    } else {
        s
    }
}

/// Render one progress line: `<tool>: 3/10 (30%) message`.
pub fn format_progress(
    tool: Option<&str>,
    progress: f64,
    total: Option<f64>,
    message: Option<&str>,
) -> String {
    let mut out = format!("{}: ", tool.unwrap_or("tool"));
    match total {
        Some(t) if t > 0.0 => {
            let pct = ((progress / t) * 100.0).clamp(0.0, 100.0);
            out.push_str(&format!("{}/{} ({pct:.0}%)", fmt_num(progress), fmt_num(t)));
        }
        _ => out.push_str(&fmt_num(progress)),
    }
    if let Some(m) = message.filter(|m| !m.is_empty()) {
        out.push(' ');
        out.push_str(m);
    }
    clean(&out)
}

/// Where a server log message goes.
#[derive(Debug, Clone)]
pub enum LogRoute {
    Notify(Notification),
    Debug(String),
}

/// Render a logging payload as one line.
fn log_text(logger: Option<&str>, data: &serde_json::Value) -> String {
    let body = match data {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match logger {
        Some(l) if !l.is_empty() => clean(&format!("{l}: {body}")),
        _ => clean(&body),
    }
}

/// Map a server log message: warning and above reach the UI, the rest
/// goes to the debug log.
pub fn route_log(
    server: &str,
    level: LoggingLevel,
    logger: Option<&str>,
    data: &serde_json::Value,
) -> LogRoute {
    let line = format!("[mcp:{server}] {}", log_text(logger, data));
    match level {
        LoggingLevel::Debug | LoggingLevel::Info | LoggingLevel::Notice => LogRoute::Debug(line),
        LoggingLevel::Warning => LogRoute::Notify(Notification::Warn(line)),
        LoggingLevel::Error
        | LoggingLevel::Critical
        | LoggingLevel::Alert
        | LoggingLevel::Emergency => LogRoute::Notify(Notification::Error(line)),
    }
}

// ---------------------------------------------------------------------------
// Sink port
// ---------------------------------------------------------------------------

/// Output port for client-side events.
pub trait ClientEventSink: Send + Sync + 'static {
    fn notify(&self, notif: Notification);
    fn debug(&self, line: String);
}

/// Production sink: UI notification channel + tracing debug.
pub struct UiSink;

impl ClientEventSink for UiSink {
    fn notify(&self, notif: Notification) {
        crate::ui::notifications::notify_send(notif);
    }
    fn debug(&self, line: String) {
        tracing::debug!(target: "dirge::mcp", "{line}");
    }
}

// ---------------------------------------------------------------------------
// rmcp boundary
// ---------------------------------------------------------------------------

/// rmcp client handler for one server connection.
pub struct McpClientHandler {
    server: String,
    registry: Arc<ProgressRegistry>,
    coalescer: Mutex<ProgressCoalescer>,
    sink: Arc<dyn ClientEventSink>,
}

impl McpClientHandler {
    pub fn new(
        server: impl Into<String>,
        registry: Arc<ProgressRegistry>,
        sink: Arc<dyn ClientEventSink>,
    ) -> Self {
        Self {
            server: server.into(),
            registry,
            coalescer: Mutex::new(ProgressCoalescer::new(PROGRESS_MIN_INTERVAL)),
            sink,
        }
    }

    /// Handler wired to the global registry and the UI sink.
    pub fn for_server(server: &str) -> Self {
        Self::new(server, global_registry(), Arc::new(UiSink))
    }

    /// Synchronous core of `on_progress`.
    pub fn handle_progress(&self, params: &ProgressNotificationParam) {
        let is_final = is_final_progress(params.progress, params.total);
        let admitted = self
            .coalescer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .admit(&params.progress_token, is_final, Instant::now());
        if !admitted {
            return;
        }
        let tool = self.registry.tool_for(&self.server);
        let line = format_progress(
            tool.as_deref(),
            params.progress,
            params.total,
            params.message.as_deref(),
        );
        self.sink.notify(Notification::McpLog {
            server: self.server.clone(),
            line,
        });
    }

    /// Synchronous core of `on_logging_message`.
    pub fn handle_log(&self, params: &LoggingMessageNotificationParam) {
        match route_log(
            &self.server,
            params.level,
            params.logger.as_deref(),
            &params.data,
        ) {
            LogRoute::Notify(n) => self.sink.notify(n),
            LogRoute::Debug(line) => self.sink.debug(line),
        }
    }
}

impl ClientHandler for McpClientHandler {
    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.handle_progress(&params);
    }

    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.handle_log(&params);
    }

    fn get_info(&self) -> ClientInfo {
        // Same client info the unit handler `()` advertised.
        ClientInfo::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recording sink port for tests.
    #[derive(Default)]
    pub(super) struct RecordingSink {
        pub notes: Mutex<Vec<Notification>>,
        pub debugs: Mutex<Vec<String>>,
    }

    impl ClientEventSink for RecordingSink {
        fn notify(&self, notif: Notification) {
            self.notes.lock().unwrap().push(notif);
        }
        fn debug(&self, line: String) {
            self.debugs.lock().unwrap().push(line);
        }
    }

    impl RecordingSink {
        pub(super) fn lines(&self) -> Vec<String> {
            self.notes
                .lock()
                .unwrap()
                .iter()
                .map(|n| match n {
                    Notification::McpLog { server, line } => format!("{server}|{line}"),
                    Notification::Warn(s) => format!("WARN|{s}"),
                    Notification::Error(s) => format!("ERR|{s}"),
                    Notification::Info(s) => format!("INFO|{s}"),
                })
                .collect()
        }
    }

    use rmcp::model::NumberOrString;

    fn tok(n: i64) -> ProgressToken {
        ProgressToken(NumberOrString::Number(n))
    }

    #[test]
    fn registry_names_the_single_inflight_tool_per_server() {
        let reg = Arc::new(ProgressRegistry::new());
        let a = reg.issue("s1", "alpha");
        let _other = reg.issue("s2", "gamma");
        assert_eq!(reg.tool_for("s1").as_deref(), Some("alpha"));
        let b = reg.issue("s1", "beta");
        // Ambiguous while two calls share the server.
        assert_eq!(reg.tool_for("s1"), None);
        drop(a);
        assert_eq!(reg.tool_for("s1").as_deref(), Some("beta"));
        drop(b);
        assert_eq!(reg.tool_for("s1"), None);
        assert_eq!(reg.tool_for("s2").as_deref(), Some("gamma"));
    }

    #[test]
    fn coalescer_rate_limits_per_token() {
        let mut c = ProgressCoalescer::new(Duration::from_millis(500));
        let t0 = Instant::now();
        assert!(c.admit(&tok(1), false, t0));
        assert!(!c.admit(&tok(1), false, t0 + Duration::from_millis(100)));
        // A different token is independent.
        assert!(c.admit(&tok(2), false, t0 + Duration::from_millis(100)));
        assert!(c.admit(&tok(1), false, t0 + Duration::from_millis(600)));
    }

    #[test]
    fn coalescer_always_admits_final() {
        let mut c = ProgressCoalescer::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(c.admit(&tok(1), false, t0));
        assert!(c.admit(&tok(1), true, t0));
    }

    #[test]
    fn final_detection() {
        assert!(is_final_progress(10.0, Some(10.0)));
        assert!(!is_final_progress(9.0, Some(10.0)));
        assert!(!is_final_progress(10.0, None));
        assert!(!is_final_progress(0.0, Some(0.0)));
    }

    #[test]
    fn progress_line_formats() {
        assert_eq!(
            format_progress(Some("build"), 3.0, Some(10.0), Some("compiling")),
            "build: 3/10 (30%) compiling"
        );
        assert_eq!(format_progress(None, 2.5, None, None), "tool: 2.5");
        let line = format_progress(Some("x"), 1.0, None, Some("\x1b[31mred"));
        assert!(!line.contains('\x1b'), "controls stripped: {line:?}");
    }

    #[test]
    fn log_routing_by_level() {
        let data = serde_json::json!("disk almost full");
        assert!(matches!(
            route_log("srv", LoggingLevel::Info, None, &data),
            LogRoute::Debug(ref l) if l == "[mcp:srv] disk almost full"
        ));
        assert!(matches!(
            route_log("srv", LoggingLevel::Warning, Some("fs"), &data),
            LogRoute::Notify(Notification::Warn(ref l)) if l == "[mcp:srv] fs: disk almost full"
        ));
        assert!(matches!(
            route_log("srv", LoggingLevel::Critical, None, &serde_json::json!({"a": 1})),
            LogRoute::Notify(Notification::Error(ref l)) if l == "[mcp:srv] {\"a\":1}"
        ));
    }

    #[test]
    fn handler_routes_progress_with_tool_name_and_coalesces() {
        let reg = Arc::new(ProgressRegistry::new());
        let sink = Arc::new(RecordingSink::default());
        let h = McpClientHandler::new("srv", Arc::clone(&reg), sink.clone());
        let _guard = reg.issue("srv", "index");
        let t = tok(7);
        h.handle_progress(&ProgressNotificationParam::new(t.clone(), 1.0).with_total(4.0));
        h.handle_progress(&ProgressNotificationParam::new(t.clone(), 2.0).with_total(4.0));
        h.handle_progress(&ProgressNotificationParam::new(t.clone(), 4.0).with_total(4.0));
        assert_eq!(
            sink.lines(),
            vec!["srv|index: 1/4 (25%)", "srv|index: 4/4 (100%)"]
        );
    }

    #[test]
    fn handler_routes_logs() {
        let sink = Arc::new(RecordingSink::default());
        let h = McpClientHandler::new("srv", Arc::new(ProgressRegistry::new()), sink.clone());
        h.handle_log(&LoggingMessageNotificationParam::new(
            LoggingLevel::Debug,
            serde_json::json!("quiet"),
        ));
        h.handle_log(&LoggingMessageNotificationParam::new(
            LoggingLevel::Error,
            serde_json::json!("loud"),
        ));
        assert_eq!(sink.lines(), vec!["ERR|[mcp:srv] loud"]);
        assert_eq!(*sink.debugs.lock().unwrap(), vec!["[mcp:srv] quiet"]);
    }

    /// End to end over an in-process duplex pipe: a scripted JSON-RPC
    /// server answers `initialize`, then on `tools/call` echoes progress
    /// for the request's `_meta.progressToken` plus a warning log before
    /// replying. The handler must name the tool and surface the warning.
    #[tokio::test]
    async fn duplex_progress_and_logging_reach_the_sink() {
        use rmcp::model::CallToolRequestParams;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(server_io);
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
                let id = msg.get("id").cloned();
                let reply = match msg["method"].as_str() {
                    Some("initialize") => vec![serde_json::json!({
                        "jsonrpc": "2.0", "id": id, "result": {
                            "protocolVersion": msg["params"]["protocolVersion"],
                            "capabilities": {"tools": {}, "logging": {}},
                            "serverInfo": {"name": "scripted", "version": "0"}
                        }
                    })],
                    Some("tools/call") => {
                        let token = msg["params"]["_meta"]["progressToken"].clone();
                        vec![
                            serde_json::json!({"jsonrpc": "2.0",
                                "method": "notifications/progress",
                                "params": {"progressToken": token, "progress": 2, "total": 2}}),
                            serde_json::json!({"jsonrpc": "2.0",
                                "method": "notifications/message",
                                "params": {"level": "warning", "data": "slow disk"}}),
                            serde_json::json!({"jsonrpc": "2.0", "id": id,
                                "result": {"content": [], "isError": false}}),
                        ]
                    }
                    _ => vec![],
                };
                for m in reply {
                    let mut s = m.to_string();
                    s.push('\n');
                    w.write_all(s.as_bytes()).await.unwrap();
                }
                w.flush().await.unwrap();
            }
        });

        let reg = Arc::new(ProgressRegistry::new());
        let sink = Arc::new(RecordingSink::default());
        let handler = McpClientHandler::new("srv", Arc::clone(&reg), sink.clone());
        let rs = rmcp::service::serve_client(handler, client_io)
            .await
            .expect("initialize");
        let guard = reg.issue("srv", "reindex");
        let params = CallToolRequestParams::new("reindex");
        rs.peer().call_tool(params).await.expect("call_tool");
        drop(guard);

        let deadline = Instant::now() + Duration::from_secs(5);
        while sink.lines().len() < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut got = sink.lines();
        got.sort();
        assert_eq!(
            got,
            vec!["WARN|[mcp:srv] slow disk", "srv|reindex: 2/2 (100%)"]
        );
        drop(rs);
        server.abort();
    }
}
