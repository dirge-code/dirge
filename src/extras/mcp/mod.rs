pub mod client;
pub mod config;
pub mod tool;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tool::McpTool;

use crate::agent::agent_loop::types::InjectionScanMode;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

pub struct McpClientManager {
    /// Connection state per server, by name. Each `Arc<SharedConnection>`
    /// is the SINGLE owner of its peer + RunningService. Cloned into every
    /// `McpTool` from that server so manual `/mcp reconnect` AND tool-side
    /// auto-reconnect share the same swap target (M-R1 + M-R4 fix).
    connections: HashMap<String, Arc<client::SharedConnection>>,
    /// Per-server reconnect serializer + generation counter. Cloned into
    /// every `McpTool` from that server so concurrent failures dedup
    /// across the whole agent — and survive `collect_tools` being
    /// called multiple times during a session (M-R2 fix).
    reconnect_locks: HashMap<String, Arc<Mutex<u64>>>,
    /// Original configs retained so a disconnected server can be
    /// reconnected later via [`reconnect`] (manual `/mcp reconnect`) OR
    /// the tool-side auto-reconnect path (audit H15).
    configs: HashMap<String, config::McpServerConfig>,
    /// Cached tool definitions per server (dirge-fn8h). `collect_tools` runs
    /// on every `build_agent`, and build_agent is awaited inline at ~9 UI-loop
    /// sites; without this, each rebuild re-ran a `list_tools` network
    /// round-trip per server and froze the loop. Populated on first fetch,
    /// invalidated on manual [`reconnect`]. A server's advertised tool set is
    /// stable for its process lifetime, so this stays correct across the
    /// tool-side auto-reconnect (same server → same tools). `std::sync::Mutex`
    /// because the critical sections are sync (no await held).
    tool_cache: std::sync::Mutex<HashMap<String, Vec<rmcp::model::Tool>>>,
    /// Names of servers whose initial `connect` failed (GH #541). The
    /// manager still drops the live connection, but recording the name
    /// lets the info panel surface it as broken (`○`) instead of
    /// silently omitting it — mirroring how LSP shows broken servers.
    failed: Vec<String>,
}

/// Bound on a per-server `list_tools` round-trip (dirge-fn8h). A wedged server
/// must not hang the UI loop; mirrors the 2s git-subprocess cap's intent with
/// extra headroom since caching means this only gates the FIRST fetch per
/// server (and post-reconnect retries).
const LIST_TOOLS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl McpClientManager {
    pub async fn connect_all(configs: &HashMap<String, config::McpServerConfig>) -> Self {
        // Connect to every server CONCURRENTLY. This loop used to await
        // each `client::connect` in turn, so startup paid the SUM of every
        // server's spin-up before the first frame could draw — and each
        // can be seconds (an `npx -y <pkg>` cold start) with a 10s init
        // timeout on top. Running them together means startup waits only
        // for the SLOWEST server instead of all of them in series, which
        // is the dominant contributor to time-to-first-frame (dirge-lvag).
        //
        // `join_all` preserves input order, and the result-handling below
        // stays a sequential pass, so log/insert order and the
        // skip-failed-server behaviour are unchanged — only the network
        // waits overlap.
        let connect_results = futures::future::join_all(configs.iter().map(|(name, cfg)| {
            let name = name.clone();
            async move {
                let result = client::connect(name.clone(), cfg).await;
                (name, result)
            }
        }))
        .await;

        let mut connections = HashMap::new();
        let mut reconnect_locks = HashMap::new();
        let mut failed = Vec::new();
        for (name, result) in connect_results {
            match result {
                Ok(conn) => {
                    tracing::info!("Connected to MCP server '{}'", name);
                    connections.insert(name.clone(), conn);
                    reconnect_locks.insert(name, Arc::new(Mutex::new(0u64)));
                }
                Err(e) => {
                    // Record the name so the info panel can surface a
                    // failed server as broken (GH #541) instead of
                    // silently omitting it. The live connection is still
                    // dropped — reconnect keeps working from `configs`.
                    failed.push(name.clone());
                    // ALSO emit to stderr so users running without
                    // RUST_LOG / --verbose see that an MCP server
                    // failed to register. Without this, configured
                    // tools just silently never appear and the user
                    // has no idea why.
                    tracing::warn!("Failed to connect to MCP server '{}': {e}", name);
                    eprintln!(
                        "warning: MCP server '{}' failed to connect: {}; its tools won't be available this session",
                        name, e,
                    );
                }
            }
        }
        Self {
            connections,
            reconnect_locks,
            configs: configs.clone(),
            tool_cache: std::sync::Mutex::new(HashMap::new()),
            failed,
        }
    }

    /// Reconnect a single MCP server by name using its original config.
    /// Updates the existing `SharedConnection` in place via `replace`,
    /// so every `McpTool` clone from that server picks up the new
    /// transport on its next call.
    ///
    /// Wired by `/mcp reconnect <name>` (UI slash) for the manual case.
    /// `McpTool` self-reconnects on its own via the same swap path
    /// on transport-class failures.
    #[allow(dead_code)]
    pub async fn reconnect(&mut self, name: &str) -> anyhow::Result<()> {
        let cfg = self.configs.get(name).cloned().ok_or_else(|| {
            anyhow::anyhow!("no config for MCP server '{name}' — was it registered at startup?")
        })?;
        let conn = self.connections.get(name).cloned();

        let (new_peer, new_rs, new_guard) = client::raw_connect(name, &cfg)
            .await
            .map_err(|e| anyhow::anyhow!("reconnect to '{name}' failed: {e}"))?;

        // A manual reconnect may target a server whose tool set changed
        // (config edit, server upgrade) — drop the cached definitions so the
        // next collect_tools re-fetches (dirge-fn8h).
        self.tool_cache.lock().unwrap().remove(name);

        if let Some(conn) = conn {
            // Swap into the existing shared container so previously-
            // handed-out McpTool clones see the new peer.
            conn.replace(new_peer, new_rs, new_guard).await;
        } else {
            // No prior connection (server failed to start originally).
            // Create a fresh shared container + start a fresh
            // reconnect lock.
            let conn = Arc::new(client::SharedConnection::new(
                name.to_string(),
                new_peer,
                new_rs,
                new_guard,
            ));
            self.connections.insert(name.to_string(), conn);
            self.reconnect_locks
                .entry(name.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(0u64)));
        }
        Ok(())
    }

    pub async fn collect_tools(
        &self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Vec<McpTool> {
        let mut all_tools = Vec::new();
        for (server_name, conn) in &self.connections {
            let cfg = self.configs.get(server_name).cloned().map(Arc::new);
            // Reconnect lock from the manager's persistent map. Cloning
            // the Arc bumps the refcount; every McpTool from this
            // server (across this AND any future collect_tools call)
            // shares one canonical lock + gen counter.
            let reconnect_lock = self
                .reconnect_locks
                .get(server_name)
                .cloned()
                .unwrap_or_else(|| Arc::new(Mutex::new(0u64)));
            let definitions = self
                .tools_for_server(server_name, LIST_TOOLS_TIMEOUT, || client::list_tools(conn))
                .await;
            for definition in definitions {
                all_tools.push(McpTool {
                    server_name: server_name.clone(),
                    definition,
                    connection: Arc::clone(conn),
                    config: cfg.clone(),
                    reconnect_lock: reconnect_lock.clone(),
                    permission: permission.clone(),
                    ask_tx: ask_tx.clone(),
                    injection_scan_mode: InjectionScanMode::default(),
                });
            }
        }
        all_tools
    }

    /// Tool definitions for one server: cache hit if present, else fetch via
    /// `fetch` bounded by `timeout` and cache the result (dirge-fn8h). A failed
    /// or timed-out fetch yields no tools and is NOT cached, so the next
    /// rebuild retries. Generic over the fetcher so the cache + timeout logic
    /// is unit-testable without a live MCP peer.
    async fn tools_for_server<F, Fut, E>(
        &self,
        server: &str,
        timeout: std::time::Duration,
        fetch: F,
    ) -> Vec<rmcp::model::Tool>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<rmcp::model::Tool>, E>>,
        E: std::fmt::Display,
    {
        if let Some(cached) = self.cached_tools(server) {
            return cached;
        }
        match tokio::time::timeout(timeout, fetch()).await {
            Ok(Ok(tools)) => {
                self.store_tools(server, tools.clone());
                tools
            }
            Ok(Err(e)) => {
                tracing::warn!("Failed to list tools from MCP server '{}': {e}", server);
                eprintln!(
                    "warning: MCP server '{}' connected but list_tools failed: {}; \
                     its tools won't be available this session",
                    server, e,
                );
                Vec::new()
            }
            Err(_) => {
                tracing::warn!(
                    "Timed out listing tools from MCP server '{}' after {:?}",
                    server,
                    timeout,
                );
                eprintln!(
                    "warning: MCP server '{}' did not respond to list_tools within {:?}; \
                     its tools won't be available this turn",
                    server, timeout,
                );
                Vec::new()
            }
        }
    }

    /// Cached tool definitions for a server, if any (dirge-fn8h).
    fn cached_tools(&self, server: &str) -> Option<Vec<rmcp::model::Tool>> {
        self.tool_cache.lock().unwrap().get(server).cloned()
    }

    /// Cache a server's tool definitions for reuse by later rebuilds.
    fn store_tools(&self, server: &str, tools: Vec<rmcp::model::Tool>) {
        self.tool_cache
            .lock()
            .unwrap()
            .insert(server.to_string(), tools);
    }

    /// Snapshot the current set of (server_name, shared_connection)
    /// pairs. Cheap — clones an `Arc` per server. Used by the
    /// `/mcp` slash command and the info panel to enumerate the
    /// live connections without holding any lock across the await
    /// points that follow (e.g. `list_tools`).
    pub fn connections_snapshot(&self) -> Vec<(String, Arc<client::SharedConnection>)> {
        self.connections
            .iter()
            .map(|(name, conn)| (name.clone(), Arc::clone(conn)))
            .collect()
    }

    /// Names of servers whose initial `connect` failed (GH #541). The
    /// info panel renders these as broken (`○`) alongside healthy ones,
    /// so a misconfigured or timed-out server is visible instead of
    /// silently omitted.
    pub fn failed_servers(&self) -> Vec<String> {
        self.failed.clone()
    }

    pub async fn shutdown(self) {
        for (name, conn) in self.connections {
            conn.shutdown().await;
            tracing::debug!("Disconnected from MCP server '{}'", name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn bogus_server() -> config::McpServerConfig {
        // A command that can't spawn → `connect` returns Err fast (no
        // 10s timeout), letting us exercise connect_all without a real
        // MCP server.
        config::McpServerConfig::Command {
            command: "dirge-nonexistent-mcp-binary".to_string(),
            args: vec![],
            env: HashMap::new(),
            allow_external_paths: false,
        }
    }

    /// dirge-lvag: parallelizing connect_all must preserve the
    /// skip-failed-server contract — a server that fails to connect is
    /// dropped (no live connection) but its config is retained so it can
    /// still be `/mcp reconnect`-ed, and the other servers are unaffected.
    #[tokio::test]
    async fn connect_all_skips_failed_servers_and_retains_configs() {
        let mut configs = HashMap::new();
        configs.insert("bogus-a".to_string(), bogus_server());
        configs.insert("bogus-b".to_string(), bogus_server());

        let mgr = McpClientManager::connect_all(&configs).await;

        // Both failed → no live connections, but every config is kept
        // (the manager is the source of truth for later reconnects).
        assert_eq!(mgr.connections.len(), 0, "failed servers must not register");
        assert_eq!(mgr.reconnect_locks.len(), 0);
        assert_eq!(mgr.configs.len(), 2, "configs retained for /mcp reconnect");
        assert!(mgr.connections_snapshot().is_empty());
    }

    /// GH #541: a server that fails to connect must still be VISIBLE to
    /// the user. Previously the only signal was a one-line stderr warning,
    /// and the info panel (which reads only live connections) rendered
    /// `· (none)` — so a misconfigured / timed-out MCP server was
    /// completely invisible. The manager now records the names of servers
    /// that failed their initial connect so the panel can surface them as
    /// broken (parity with how LSP shows broken servers).
    #[tokio::test]
    async fn connect_all_records_failed_server_names() {
        let mut configs = HashMap::new();
        configs.insert("bogus-a".to_string(), bogus_server());
        configs.insert("bogus-b".to_string(), bogus_server());

        let mgr = McpClientManager::connect_all(&configs).await;

        let mut failed = mgr.failed_servers();
        failed.sort();
        assert_eq!(failed, vec!["bogus-a".to_string(), "bogus-b".to_string()]);
    }

    /// Empty config set → an empty, well-formed manager (no panic, no
    /// stray entries).
    #[tokio::test]
    async fn connect_all_empty_config_is_empty_manager() {
        let mgr = McpClientManager::connect_all(&HashMap::new()).await;
        assert!(mgr.connections.is_empty());
        assert!(mgr.configs.is_empty());
    }

    /// dirge-fn8h: build_agent rebuilds the agent at ~9 inline UI-loop sites,
    /// and each rebuild used to re-run `list_tools` per server (a network
    /// round-trip) — freezing the loop. The tool list is cached after the
    /// first fetch, so a second `tools_for_server` is served from cache and
    /// makes no network call.
    #[tokio::test]
    async fn tools_for_server_caches_after_first_fetch() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mgr = McpClientManager::connect_all(&HashMap::new()).await;
        let calls = std::sync::Arc::new(AtomicUsize::new(0));

        let c = calls.clone();
        let first = mgr
            .tools_for_server("srv", std::time::Duration::from_secs(1), || async {
                c.fetch_add(1, Ordering::SeqCst);
                Ok::<_, std::io::Error>(Vec::new())
            })
            .await;
        assert!(first.is_empty());

        let c = calls.clone();
        let _second = mgr
            .tools_for_server("srv", std::time::Duration::from_secs(1), || async {
                c.fetch_add(1, Ordering::SeqCst);
                Ok::<_, std::io::Error>(Vec::new())
            })
            .await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second collect must be served from cache, not re-fetched"
        );
        assert!(mgr.cached_tools("srv").is_some());
    }

    /// A wedged server must not hang the rebuild: `list_tools` is bounded by a
    /// timeout, after which the server contributes no tools (and the failure
    /// is NOT cached, so the next rebuild retries).
    #[tokio::test]
    async fn tools_for_server_times_out_to_empty_without_caching() {
        let mgr = McpClientManager::connect_all(&HashMap::new()).await;
        let tools = mgr
            .tools_for_server("slow", std::time::Duration::from_millis(20), || async {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok::<_, std::io::Error>(Vec::new())
            })
            .await;
        assert!(tools.is_empty(), "a timed-out fetch yields no tools");
        assert!(
            mgr.cached_tools("slow").is_none(),
            "a timeout must not be cached — the next rebuild should retry"
        );
    }
}
