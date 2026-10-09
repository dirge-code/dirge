//! `dirge.harness/mcp-call`: addon code reaching the MCP servers dirge is
//! connected to, over the same connections dirge's own MCP tools use.
//!
//! Before a call leaves dirge it is refused when the permission checker
//! handed to [`publish`] denies `mcp_tool:<server>:<tool>` (a `deny` rule or
//! the active prompt's `deny_tools`), or when a path argument lies outside
//! the working directory and the server's config does not set
//! `allow_external_paths`. Nothing asks the user: a call the rules would ask
//! about runs. The result comes back as the server sent it, without the size
//! cap, injection scan or auto-reconnect of the model's MCP tool calls.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

use super::port::McpGateway;
use crate::extras::mcp::McpClientManager;
use crate::extras::mcp::client::SharedConnection;
use crate::extras::mcp::tool::first_external_path;
use crate::permission::checker::PermCheck;
use crate::permission::engine::types::Effect;
#[allow(unused_imports)]
use crate::sync_util::LockExt;

/// Longest an addon waits for one MCP call.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// A connected server as addon code reaches it.
#[derive(Clone)]
struct Server {
    name: String,
    connection: Arc<SharedConnection>,
    allow_external_paths: bool,
}

/// What [`publish`] made reachable: the servers, and the permission checker
/// calls to them answer to.
#[derive(Clone, Default)]
struct Published {
    servers: Vec<Server>,
    permission: Option<PermCheck>,
}

type Shared = Arc<Mutex<Published>>;

static LIVE: OnceLock<Shared> = OnceLock::new();

fn live() -> Shared {
    LIVE.get_or_init(Shared::default).clone()
}

/// Make `manager`'s connections reachable from addon code, with calls
/// checked against `permission` (`None`: unchecked, as dirge's own tools
/// are without a checker). Called wherever a manager finishes connecting; a
/// reconnect swaps the peer inside the shared connection, so it needs no
/// republish.
pub fn publish(manager: &McpClientManager, permission: Option<PermCheck>) {
    let servers = manager
        .connections_snapshot()
        .into_iter()
        .map(|(name, connection)| Server {
            allow_external_paths: manager.allows_external_paths(&name),
            name,
            connection,
        })
        .collect();
    *live().lock_ignore_poison() = Published {
        servers,
        permission,
    };
    readiness().send_replace(Readiness::Published);
}

/// Where the MCP servers addon code reaches stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    /// Nothing is connecting: whatever is published is all there will be.
    Idle,
    /// A connect is under way and will [`publish`] when it finishes.
    Connecting,
    /// [`publish`] ran.
    Published,
}

static READINESS: OnceLock<tokio::sync::watch::Sender<Readiness>> = OnceLock::new();

fn readiness() -> &'static tokio::sync::watch::Sender<Readiness> {
    READINESS.get_or_init(|| tokio::sync::watch::channel(Readiness::Idle).0)
}

/// Record that the configured servers are connecting in the background, so
/// [`connected`] waits for them to be published.
pub fn expect() {
    readiness().send_if_modified(|state| {
        let idle = *state == Readiness::Idle;
        if idle {
            *state = Readiness::Connecting;
        }
        idle
    });
}

/// The names of the servers addon code reaches, once a connect under way
/// has published them or `wait` has passed, whichever comes first.
pub async fn connected(wait: Duration) -> Vec<String> {
    settled(readiness(), wait).await;
    published_names(&live())
}

/// Returns once `state` is not [`Readiness::Connecting`], or after `wait`.
async fn settled(state: &tokio::sync::watch::Sender<Readiness>, wait: Duration) {
    let mut state = state.subscribe();
    let _ = tokio::time::timeout(wait, state.wait_for(|s| *s != Readiness::Connecting)).await;
}

fn published_names(published: &Shared) -> Vec<String> {
    published
        .lock_ignore_poison()
        .servers
        .iter()
        .map(|s| s.name.clone())
        .collect()
}

/// Why the rules refuse `tool` on `server` outright, or `None`. Reads deny
/// rules and the active prompt's `deny_tools` without asking or recording
/// anything, so a call the rules would ask about is not refused.
fn denial(permission: &PermCheck, server: &str, tool: &str) -> Option<String> {
    let qualified = format!("mcp_tool:{server}:{tool}");
    let checker = permission.lock_ignore_poison();
    if checker.any_prompt_denied(&[tool, &qualified, "mcp_tool"]) {
        return Some(format!(
            "MCP tool {server}::{tool} is denied by the active prompt's `deny_tools`"
        ));
    }
    let decision = checker.peek("mcp_tool", &qualified, false);
    (decision.effect == Effect::Deny)
        .then(|| format!("MCP tool {server}::{tool} is denied: {}", decision.reason()))
}

/// Why the outside-cwd guard refuses `args` for `tool` on `server`, or
/// `None`: the first path argument outside the working directory, unless
/// the server allows external paths.
fn external_path(
    permission: &PermCheck,
    server: &str,
    allow_external_paths: bool,
    tool: &str,
    args: &Value,
) -> Option<String> {
    let path = first_external_path(permission, args.as_object()?, allow_external_paths)?;
    Some(format!(
        "MCP tool {server}::{tool} refused: path {path:?} is outside the working directory; \
         set `allow_external_paths: true` on the `{server}` server config to permit it"
    ))
}

/// The published connections, driven on the runtime dirge started on.
pub struct LiveMcp {
    handle: tokio::runtime::Handle,
    published: Shared,
}

impl LiveMcp {
    /// The gateway for the runtime this is called on; `None` outside one.
    pub fn current() -> Option<Self> {
        tokio::runtime::Handle::try_current()
            .ok()
            .map(|handle| Self {
                handle,
                published: live(),
            })
    }

    fn snapshot(&self) -> Published {
        self.published.lock_ignore_poison().clone()
    }
}

impl McpGateway for LiveMcp {
    fn servers(&self) -> Vec<String> {
        published_names(&self.published)
    }

    fn call(&self, server: &str, tool: &str, args: &Value) -> Result<Value, String> {
        // The addon isolate is a plain thread. Anywhere else, blocking here
        // would park a runtime worker the call itself needs.
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err("mcp-call must not run on an async runtime thread".to_string());
        }
        let Published {
            servers,
            permission,
        } = self.snapshot();
        if let Some(reason) = permission.as_ref().and_then(|p| denial(p, server, tool)) {
            return Err(reason);
        }
        let target = servers
            .into_iter()
            .find(|s| s.name == server)
            .ok_or_else(|| format!("no MCP server named {server} is connected"))?;
        if let Some(reason) = permission
            .as_ref()
            .and_then(|p| external_path(p, server, target.allow_external_paths, tool, args))
        {
            return Err(reason);
        }
        let mut params = rmcp::model::CallToolRequestParams::new(tool.to_string());
        if let Some(object) = args.as_object() {
            params = params.with_arguments(object.clone());
        }
        let conn = target.connection;
        let began = std::time::Instant::now();
        let answer = self.handle.block_on(async move {
            let peer = conn.current_peer().await;
            tokio::time::timeout(CALL_TIMEOUT, peer.call_tool(params)).await
        });
        let answer = match answer {
            Ok(Ok(result)) => serde_json::to_value(result).map_err(|e| e.to_string()),
            Ok(Err(e)) => Err(format!("MCP tool error ({server}::{tool}): {e}")),
            Err(_) => Err(format!(
                "MCP tool {server}::{tool} timed out after {}s",
                CALL_TIMEOUT.as_secs()
            )),
        };
        let tool_error = answer
            .as_ref()
            .is_ok_and(|v| v.get("isError") == Some(&Value::Bool(true)));
        tracing::debug!(
            target: "dirge::addon",
            server,
            tool,
            took_ms = began.elapsed().as_millis() as u64,
            error = answer.as_ref().err().map(String::as_str),
            tool_error,
            "mcp-call answered"
        );
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{Action, OpSpec, PermissionConfig, RuleConfig, SecurityMode};
    use serde_json::json;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    /// A checker rooted at `/tmp` that denies `mcp_tool:db:drop_table` and
    /// leaves every other MCP tool at its default, ask.
    fn checker() -> PermCheck {
        let config = PermissionConfig {
            rules: vec![RuleConfig {
                op: OpSpec::Mcp,
                pattern: "mcp_tool:db:drop_table".to_string(),
                effect: Action::Deny,
                tool: None,
            }],
            ..Default::default()
        };
        Arc::new(Mutex::new(PermissionChecker::new(
            &config,
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/tmp")),
        )))
    }

    fn gateway(rt: &tokio::runtime::Runtime, permission: Option<PermCheck>) -> LiveMcp {
        LiveMcp {
            handle: rt.handle().clone(),
            published: Arc::new(Mutex::new(Published {
                servers: Vec::new(),
                permission,
            })),
        }
    }

    #[test]
    fn a_deny_rule_refuses_and_an_ask_rule_does_not() {
        let perm = checker();
        let reason = denial(&perm, "db", "drop_table").expect("denied");
        assert!(reason.contains("db::drop_table is denied"), "{reason}");
        assert_eq!(denial(&perm, "db", "select"), None);
    }

    #[test]
    fn the_prompts_deny_tools_refuse_by_bare_or_qualified_name() {
        let perm = checker();
        perm.lock_ignore_poison()
            .set_prompt_deny_tools(vec!["select".to_string()]);
        let reason = denial(&perm, "db", "select").expect("denied");
        assert!(reason.contains("deny_tools"), "{reason}");
        perm.lock_ignore_poison()
            .set_prompt_deny_tools(vec!["mcp_tool:fs:write_file".to_string()]);
        assert!(denial(&perm, "fs", "write_file").is_some());
        assert_eq!(denial(&perm, "fs", "read_file"), None);
    }

    #[test]
    fn a_check_records_no_retry_pressure() {
        let perm = checker();
        for _ in 0..20 {
            assert_eq!(denial(&perm, "db", "select"), None);
        }
        let decision = perm
            .lock_ignore_poison()
            .peek("mcp_tool", "mcp_tool:db:select", false);
        assert_eq!(decision.effect, Effect::Ask);
    }

    #[test]
    fn paths_outside_the_working_directory_are_refused_unless_the_server_allows_them() {
        let perm = checker();
        let outside = json!({"path": "/etc/passwd"});
        let reason = external_path(&perm, "fs", false, "read_file", &outside).expect("refused");
        assert!(reason.contains("/etc/passwd"), "{reason}");
        assert!(reason.contains("allow_external_paths"), "{reason}");
        assert_eq!(
            external_path(&perm, "fs", true, "read_file", &outside),
            None
        );
        let inside = json!({"path": "notes.txt"});
        assert_eq!(
            external_path(&perm, "fs", false, "read_file", &inside),
            None
        );
    }

    #[test]
    fn a_denied_call_is_refused_before_the_server_is_looked_up() {
        let rt = runtime();
        let gw = gateway(&rt, Some(checker()));
        let answers = std::thread::spawn(move || {
            [
                gw.call("db", "drop_table", &json!({})),
                gw.call("db", "select", &json!({})),
            ]
        })
        .join()
        .unwrap();
        let denied = answers[0].as_ref().unwrap_err();
        assert!(denied.contains("denied"), "{denied}");
        let passed = answers[1].as_ref().unwrap_err();
        assert!(passed.contains("no MCP server named db"), "{passed}");
    }

    #[test]
    fn unknown_servers_are_an_error_not_a_hang() {
        let rt = runtime();
        let gw = gateway(&rt, None);
        let err = std::thread::spawn(move || gw.call("nowhere", "t", &json!({})))
            .join()
            .unwrap()
            .unwrap_err();
        assert!(err.contains("nowhere"), "{err}");
    }

    #[tokio::test]
    async fn nothing_connecting_is_not_waited_for() {
        let (state, _) = tokio::sync::watch::channel(Readiness::Idle);
        let started = std::time::Instant::now();
        settled(&state, Duration::from_secs(5)).await;
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_connect_under_way_is_waited_for_until_it_publishes() {
        let (state, _) = tokio::sync::watch::channel(Readiness::Connecting);
        let state = Arc::new(state);
        let publisher = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            publisher.send_replace(Readiness::Published);
        });
        let started = std::time::Instant::now();
        settled(&state, Duration::from_secs(5)).await;
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(40), "{took:?}");
        assert!(took < Duration::from_secs(2), "{took:?}");
    }

    #[tokio::test]
    async fn a_connect_that_never_publishes_is_waited_for_at_most_the_budget() {
        let (state, _) = tokio::sync::watch::channel(Readiness::Connecting);
        let started = std::time::Instant::now();
        settled(&state, Duration::from_millis(50)).await;
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn calls_from_a_runtime_thread_are_refused() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(async {
            LiveMcp::current()
                .expect("inside a runtime")
                .call("any", "t", &json!({}))
                .unwrap_err()
        });
        assert!(err.contains("async runtime"), "{err}");
    }
}
