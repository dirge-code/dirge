//! `dirge mcp` — run dirge itself as an MCP server so another agent
//! (e.g. Claude Code) can delegate implementation tasks to dirge and
//! review them: the caller does high-level planning/architecture and
//! hands the implementation details to dirge.
//!
//! The server speaks MCP over stdio and keeps a **persistent per-project
//! session**: each `delegate` runs against the same on-disk dirge session
//! (so dirge accumulates context across tasks), and `new_session` rotates
//! to a fresh one when the caller moves to a new task/thread. The current
//! session id is remembered in `<project>/.dirge/mcp_current_session.json`
//! so it survives a server restart.
//!
//! v1 executes each delegation by spawning `dirge -p --session <id>
//! --accept-all --output-format json <task>` as a child process — robust
//! and isolated. (A warm in-process executor that keeps the LSP/MCP/
//! semantic managers hot across delegations is a planned follow-up; it
//! swaps only the executor, not this MCP API.)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::ServiceExt;
use rmcp::handler::server::ServerHandler;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, InitializeResult, ServerCapabilities, ServerInfo,
};
use rmcp::transport::stdio;
use rmcp::{schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

/// Default per-delegation turn cap so a `delegate` call always returns
/// within bounds (the caller can continue with another delegation).
const DEFAULT_MAX_TURNS: u32 = 30;

#[derive(Clone)]
pub struct DirgeMcp {
    state: Arc<Mutex<State>>,
    // Read by the `#[tool_handler]`-generated `call_tool`/`list_tools`,
    // which the dead-code pass can't see through the macro — verified live
    // (the server routes all four tools). Silence the false positive.
    #[allow(dead_code)]
    tool_router: ToolRouter<DirgeMcp>,
}

struct State {
    /// The project the server operates in (its cwd at launch).
    project_dir: PathBuf,
    /// Path to the dirge binary (for spawning `-p` children).
    exe: PathBuf,
    /// Current session id — what `delegate` resumes/extends.
    session_id: String,
    /// Optional human label for the current session.
    label: Option<String>,
    /// Model override for delegated work (`--model`), if any.
    model: Option<String>,
    /// Sandbox mode for delegated bash (`--sandbox`), if any.
    sandbox: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DelegateArgs {
    /// The implementation task for dirge to carry out, in plain language,
    /// with any constraints (target files, what to avoid, how to verify).
    /// dirge works in the project, can edit files and run commands, and
    /// returns a summary of what it did.
    task: String,
    /// Start a fresh session before running — use this when moving to a
    /// new task/thread so dirge isn't anchored to the previous context.
    #[serde(default)]
    new_session: bool,
    /// Optional label for the new session (only when `new_session`).
    #[serde(default)]
    session_label: Option<String>,
    /// Cap on dirge's internal turns for this delegation (default 30).
    /// If hit, status is `max_turns` and you can continue with another
    /// `delegate` in the same session.
    #[serde(default)]
    max_turns: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct NewSessionArgs {
    /// Optional human label for the new session.
    #[serde(default)]
    label: Option<String>,
}

#[tool_handler]
impl ServerHandler for DirgeMcp {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("dirge", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "dirge is a coding agent you delegate implementation work to. Call `delegate` \
                 with a task — dirge edits files and runs commands in this project on a \
                 persistent session, then returns a summary plus the files it changed for you \
                 to review. Call `delegate` again in the same session to ask for a fix (it keeps \
                 the context). Set new_session=true (or call `new_session`) when moving to a new \
                 task/thread so it isn't anchored to the old one. Use `session_info` / \
                 `list_sessions` for orientation.",
            )
    }
}

#[tool_router]
impl DirgeMcp {
    fn new(state: State) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Delegate an implementation task to dirge. dirge works in the project \
            (editing files, running commands) on its persistent session and returns a summary \
            plus the list of files it changed, so you can review the result and either accept \
            it, ask for a fix (call delegate again — same session keeps the context), or move \
            on. Set new_session=true when starting a new task/thread."
    )]
    async fn delegate(
        &self,
        Parameters(args): Parameters<DelegateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let max_turns = args.max_turns.unwrap_or(DEFAULT_MAX_TURNS);
        // Snapshot the session id + run params under the lock, optionally
        // rotating first; release the lock before the (long) child run.
        let (exe, project_dir, session_id, model, sandbox) = {
            let mut st = self.state.lock().await;
            if args.new_session {
                st.session_id = new_session_id();
                st.label = args.session_label.clone();
                if let Err(e) = persist_pointer(&st.project_dir, &st.session_id, &st.label) {
                    return Ok(tool_err(format!("failed to persist new session: {e}")));
                }
            }
            (
                st.exe.clone(),
                st.project_dir.clone(),
                st.session_id.clone(),
                st.model.clone(),
                st.sandbox.clone(),
            )
        };

        let before = git_status_set(&project_dir);
        let run = run_delegation(
            &exe,
            &project_dir,
            &session_id,
            &args.task,
            max_turns,
            model.as_deref(),
            sandbox.as_deref(),
        )
        .await;
        let after = git_status_set(&project_dir);
        let files_changed = changed_paths(&before, &after);

        match run {
            Ok(env) => {
                let result = json!({
                    "session_id": session_id,
                    "status": env.status,
                    "summary": env.summary,
                    "files_changed": files_changed,
                    "turns": env.turns,
                    "duration_ms": env.duration_ms,
                });
                Ok(tool_json(&result))
            }
            Err(e) => Ok(tool_err(format!("delegation failed to run: {e}"))),
        }
    }

    #[tool(
        description = "Start a fresh dirge session (new task/thread) without immediately \
            delegating. Returns the new session id; subsequent delegate calls use it."
    )]
    async fn new_session(
        &self,
        Parameters(args): Parameters<NewSessionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut st = self.state.lock().await;
        st.session_id = new_session_id();
        st.label = args.label.clone();
        if let Err(e) = persist_pointer(&st.project_dir, &st.session_id, &st.label) {
            return Ok(tool_err(format!("failed to persist new session: {e}")));
        }
        Ok(tool_json(&json!({
            "session_id": st.session_id,
            "label": st.label,
        })))
    }

    #[tool(
        description = "Info about the current dirge session: its id, label, the project dir, \
            message count, last activity, and the model dirge uses for delegated work."
    )]
    async fn session_info(&self) -> Result<CallToolResult, ErrorData> {
        let st = self.state.lock().await;
        let (message_count, last_active) =
            match crate::session::storage::load_session(&st.session_id) {
                Ok(s) => (s.messages.len(), Some(s.updated_at.to_string())),
                Err(_) => (0, None),
            };
        Ok(tool_json(&json!({
            "session_id": st.session_id,
            "label": st.label,
            "project_dir": st.project_dir.to_string_lossy(),
            "message_count": message_count,
            "last_active": last_active,
            "model": st.model,
            "sandbox": st.sandbox,
        })))
    }

    #[tool(
        description = "List recent dirge sessions in this project (id, last activity, and a \
            one-line preview) so you can resume a past task thread by passing its id."
    )]
    async fn list_sessions(&self) -> Result<CallToolResult, ErrorData> {
        let sessions = crate::session::storage::find_recent_sessions(20).unwrap_or_default();
        let list: Vec<_> = sessions
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "last_active": s.updated_at,
                    "messages": s.messages.len(),
                    "preview": crate::ui::events::session_preview(s, 80),
                })
            })
            .collect();
        Ok(tool_json(&json!({ "sessions": list })))
    }
}

/// The slice of the headless `--output-format json` envelope we surface.
struct Envelope {
    status: String,
    summary: String,
    turns: u64,
    duration_ms: u64,
}

/// SIGKILL a whole process group on drop. Mirrors
/// [`crate::dap::client::DapProcessGuard`]: the delegated child is put in its
/// own session (`setsid`, pgid == pid) so `kill(-pgid)` reaps it and its
/// descendants when the delegation future is dropped (dirge-6iwq).
#[cfg(unix)]
struct ProcessGroupGuard {
    pgid: u32,
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        // Never signal group 0 (dirge's OWN group — suicide) or 1 (everything
        // signalable). setsid() makes the child a leader with pgid == pid
        // (> 1), but guard defensively against a 0/1 slipping in.
        if self.pgid <= 1 {
            return;
        }
        // SAFETY: kill(2) with a negative pid targets the process group.
        unsafe {
            let _ = libc::kill(-(self.pgid as libc::pid_t), libc::SIGKILL);
        }
    }
}

/// Run `cmd` isolated in its own process group with `kill_on_drop`, collecting
/// its output. If the returned future is dropped before completion — the MCP
/// client cancels the `delegate` tool call, or the server is shutting down
/// mid-delegation — the guard SIGKILLs the whole group so the spawned,
/// auto-approving dirge child can't keep running unsupervised (dirge-6iwq).
async fn run_child_killable(
    mut cmd: tokio::process::Command,
) -> std::io::Result<std::process::Output> {
    cmd.kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // Own session/process group so the guard's kill(-pgid) reaps the child and
    // its (non-setsid) descendants, and so a runaway child can't grab dirge's
    // controlling terminal.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    // `child` stays a live local (not moved into `wait_with_output`) so drop
    // order on cancellation is deterministic: `_pg_guard` is declared right
    // after `child`, and locals drop in reverse declaration order, so the
    // group SIGKILL fires while the leader is still an un-reaped zombie pinning
    // the pgid; `child` then drops and `kill_on_drop` reaps the leader.
    let mut child = cmd.spawn()?;
    #[cfg(unix)]
    let _pg_guard = child.id().map(|pid| ProcessGroupGuard { pgid: pid });

    // Drain both pipes concurrently with the wait so a child that fills a pipe
    // buffer can't deadlock (mirrors what `wait_with_output` does internally).
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    use tokio::io::AsyncReadExt;
    let read_out = async {
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_end(&mut stdout_buf).await;
        }
    };
    let read_err = async {
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut stderr_buf).await;
        }
    };
    let (status, (), ()) = tokio::join!(child.wait(), read_out, read_err);

    Ok(std::process::Output {
        status: status?,
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

/// Spawn `dirge -p --session <id> --accept-all --output-format json <task>`
/// in the project and parse its result envelope.
async fn run_delegation(
    exe: &Path,
    project_dir: &Path,
    session_id: &str,
    task: &str,
    max_turns: u32,
    model: Option<&str>,
    sandbox: Option<&str>,
) -> anyhow::Result<Envelope> {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.current_dir(project_dir)
        .arg("--print")
        .arg("--accept-all")
        .arg("--session")
        .arg(session_id)
        .arg("--output-format")
        .arg("json")
        .arg("--max-agent-turns")
        .arg(max_turns.to_string());
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    if let Some(s) = sandbox {
        cmd.arg("--sandbox").arg(s);
    }
    // `--` so a task that begins with `-` isn't parsed as a flag.
    cmd.arg("--").arg(task);
    cmd.stdin(std::process::Stdio::null());

    // Cancel-safe: if this future is dropped (client cancel / server shutdown),
    // the child's process group is SIGKILLed rather than left running (dirge-6iwq).
    let out = run_child_killable(cmd)
        .await
        .map_err(|e| anyhow::anyhow!("spawn dirge: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // `--output-format json` prints a single result object. Be tolerant of
    // any stray leading lines by taking the last non-empty line that parses.
    let env_val = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "dirge produced no JSON result (exit {}). stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )
        })?;
    Ok(Envelope {
        status: env_val
            .get("subtype")
            .and_then(|v| v.as_str())
            .unwrap_or("error")
            .to_string(),
        summary: env_val
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        turns: env_val
            .get("num_turns")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        duration_ms: env_val
            .get("duration_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

/// `git status --porcelain` as a set of `"XY path"` lines. Empty (not an
/// error) when the project isn't a git repo or git is unavailable.
/// Signature for the path under each `git status --porcelain` entry:
/// `path -> (size, mtime-nanos)`. Empty when the dir isn't a git repo.
///
/// We key on a content signature, not just the porcelain line, so a file
/// that was ALREADY dirty before a delegation (e.g. created untracked by
/// an earlier delegation in the same session) and is edited again is still
/// attributed — a plain porcelain-line diff misses that, since `?? path`
/// is identical before and after.
fn git_status_set(dir: &Path) -> std::collections::HashMap<String, (u64, i128)> {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["status", "--porcelain"])
        .output();
    let mut map = std::collections::HashMap::new();
    if let Ok(o) = out
        && o.status.success()
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let path = porcelain_path(line);
            if path.is_empty() {
                continue;
            }
            let sig = std::fs::metadata(dir.join(&path))
                .ok()
                .map(|m| (m.len(), mtime_nanos(&m)))
                .unwrap_or((0, 0));
            map.insert(path, sig);
        }
    }
    map
}

/// The path from a porcelain line: strip the 3-char `XY ` status prefix,
/// and for a rename/copy (`old -> new`) attribute the change to `new`.
fn porcelain_path(line: &str) -> String {
    let p = line.get(3..).unwrap_or("").trim();
    match p.find(" -> ") {
        Some(i) => p[i + 4..].to_string(),
        None => p.to_string(),
    }
}

fn mtime_nanos(m: &std::fs::Metadata) -> i128 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Paths dirge touched during the run: dirty afterward AND either newly
/// dirty or with a changed `(size, mtime)` signature vs before — so edits
/// to an already-dirty file are caught, while the caller's pre-existing
/// untouched changes are not falsely attributed.
fn changed_paths(
    before: &std::collections::HashMap<String, (u64, i128)>,
    after: &std::collections::HashMap<String, (u64, i128)>,
) -> Vec<String> {
    let mut v: Vec<String> = after
        .iter()
        .filter(|(path, sig)| before.get(*path) != Some(*sig))
        .map(|(path, _)| path.clone())
        .collect();
    v.sort();
    v.dedup();
    v
}

fn new_session_id() -> String {
    format!("mcp-{}", crate::agent::runner::uuid_v4_simple())
}

fn pointer_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".dirge").join("mcp_current_session.json")
}

fn persist_pointer(project_dir: &Path, id: &str, label: &Option<String>) -> anyhow::Result<()> {
    let path = pointer_path(project_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, json!({ "id": id, "label": label }).to_string())?;
    Ok(())
}

/// Load the remembered session pointer, or mint + persist a new one.
fn load_or_create_pointer(project_dir: &Path) -> anyhow::Result<(String, Option<String>)> {
    let path = pointer_path(project_dir);
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(id) = v.get("id").and_then(|x| x.as_str())
    {
        let label = v
            .get("label")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        return Ok((id.to_string(), label));
    }
    let id = new_session_id();
    persist_pointer(project_dir, &id, &None)?;
    Ok((id, None))
}

fn tool_json(value: &serde_json::Value) -> CallToolResult {
    let body = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    CallToolResult::success(vec![Content::text(body)])
}

fn tool_err(msg: String) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg)])
}

/// Entry point for `dirge mcp`. Runs the MCP server over stdio until the
/// transport closes (the client disconnects).
pub async fn serve(
    _cli: &crate::cli::Cli,
    _cfg: &crate::config::Config,
    model: Option<String>,
    sandbox: Option<String>,
) -> anyhow::Result<()> {
    let project_dir = std::env::current_dir()?;
    let exe = std::env::current_exe()?;
    let (session_id, label) = load_or_create_pointer(&project_dir)?;

    let server = DirgeMcp::new(State {
        project_dir,
        exe,
        session_id,
        label,
        model,
        sandbox,
    });

    let service = server
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP server failed to start: {e}"))?;
    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_paths_detects_new_and_modified_not_untouched() {
        use std::collections::HashMap;
        // src/a.rs dirty before with sig (10,1). After: a.rs unchanged,
        // b.rs newly dirty, c.rs already dirty but its signature changed
        // (further edited — the case the old line-diff missed).
        let before: HashMap<String, (u64, i128)> =
            [("src/a.rs".into(), (10, 1)), ("c.rs".into(), (5, 1))].into();
        let after: HashMap<String, (u64, i128)> = [
            ("src/a.rs".into(), (10, 1)), // untouched → not reported
            ("src/b.rs".into(), (3, 2)),  // new → reported
            ("c.rs".into(), (8, 9)),      // sig changed → reported
        ]
        .into();
        let changed = changed_paths(&before, &after);
        assert_eq!(changed, vec!["c.rs".to_string(), "src/b.rs".to_string()]);
    }

    #[test]
    fn porcelain_path_strips_status_and_handles_rename() {
        assert_eq!(porcelain_path("?? src/b.rs"), "src/b.rs");
        assert_eq!(porcelain_path(" M src/a.rs"), "src/a.rs");
        assert_eq!(porcelain_path("R  old.rs -> new.rs"), "new.rs");
    }

    #[test]
    fn new_session_id_is_prefixed() {
        let id = new_session_id();
        assert!(id.starts_with("mcp-"), "got {id}");
        assert!(id.len() > 4);
    }

    /// dirge-6iwq: when the delegation future is dropped (MCP client cancels
    /// the tool call, or the server shuts down mid-run), the spawned child AND
    /// its descendants must be SIGKILLed — otherwise an auto-approving dirge
    /// (and the tools it spawned) keeps running unsupervised. The direct child
    /// dies on `kill_on_drop` alone; this test checks a GRANDCHILD, which only
    /// the process-group guard can reap. The child backgrounds a `sleep`
    /// (grandchild, same process group) and records its pid, then `wait`s so
    /// the future stays pending.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_delegation_future_kills_the_child_group() {
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!(
            "dirge-mcp-kill-{}",
            crate::agent::runner::uuid_v4_simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let gpidfile = dir.join("grandchild_pid");

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!(
                "sleep 300 & echo $! > {}; wait",
                gpidfile.display()
            ))
            .stdin(std::process::Stdio::null());

        // Own the future in a Box so `drop(fut)` drops the future itself (and
        // runs the guard) — `tokio::pin!` would only drop a `Pin<&mut>` wrapper
        // and leave the real future (and child) alive until end of scope.
        let mut fut = Box::pin(run_child_killable(cmd));

        // Poll the future while waiting for the grandchild pid to appear; the
        // future must NOT complete (the child is `wait`ing on the grandchild).
        let gpid: i32 = loop {
            tokio::select! {
                _ = &mut fut => panic!("child should still be waiting, not done"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {
                    if let Ok(s) = std::fs::read_to_string(&gpidfile)
                        && let Ok(p) = s.trim().parse() {
                        break p;
                    }
                }
            }
        };

        // Cancel: dropping the future runs the process-group guard's SIGKILL.
        drop(fut);

        // The grandchild (`sleep 300`) shares the child's process group, so
        // only `kill(-pgid)` reaps it. Without the guard it would orphan to
        // init and keep running. Poll for its disappearance (init reaps it).
        let mut gone = false;
        for _ in 0..150 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            // SAFETY: signal 0 only probes for existence.
            if unsafe { libc::kill(gpid as libc::pid_t, 0) } != 0 {
                gone = true;
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "grandchild (pid {gpid}) must be killed on cancel");
    }

    #[test]
    fn pointer_round_trips_and_persists() {
        let dir = std::env::temp_dir().join(format!(
            "dirge-mcp-ptr-{}",
            crate::agent::runner::uuid_v4_simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // First call mints + persists a fresh pointer.
        let (id1, label1) = load_or_create_pointer(&dir).unwrap();
        assert!(id1.starts_with("mcp-"));
        assert_eq!(label1, None);

        // Second call returns the SAME id (remembered across "restarts").
        let (id2, _) = load_or_create_pointer(&dir).unwrap();
        assert_eq!(id1, id2);

        // An explicit persist with a label round-trips.
        persist_pointer(&dir, "mcp-fixed", &Some("auth".to_string())).unwrap();
        let (id3, label3) = load_or_create_pointer(&dir).unwrap();
        assert_eq!(id3, "mcp-fixed");
        assert_eq!(label3, Some("auth".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
