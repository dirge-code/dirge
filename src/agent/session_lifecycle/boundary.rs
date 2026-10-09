//! Announcing the lifecycle: every call into a [`SessionLifecycle`] runs on
//! a blocking thread of the agent runtime and is bounded; what does not
//! answer in time is logged, never raised into the session.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::collect::{self, McpServersFn};
use super::domain::{EndCause, LifecycleHook, StartFacts};
use super::policy::{self, StartLedger};
use super::port::SessionLifecycle;
use crate::agent::agent_loop::hooks::{OpenRunFn, RunOpening};
use crate::runtime::{NoAnswer, blocking_within};
#[allow(unused_imports)]
use crate::sync_util::LockExt;

/// How long each step may take before the session goes on without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    /// For the configured MCP servers to connect before a start.
    pub mcp_wait: Duration,
    /// For the listeners to answer a start.
    pub start: Duration,
    /// For the listeners to finish with an end.
    pub end: Duration,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            mcp_wait: Duration::from_secs(10),
            start: Duration::from_secs(30),
            end: Duration::from_secs(10),
        }
    }
}

/// Announces session starts and ends through one [`SessionLifecycle`].
#[derive(Clone)]
#[cfg_attr(not(feature = "addons"), allow(dead_code))]
pub struct Lifecycle {
    port: Arc<dyn SessionLifecycle>,
    ledger: Arc<Mutex<StartLedger>>,
    mcp_servers: McpServersFn,
    budgets: Budgets,
}

impl Lifecycle {
    #[cfg_attr(not(feature = "addons"), allow(dead_code))]
    pub fn new(
        port: Arc<dyn SessionLifecycle>,
        ledger: Arc<Mutex<StartLedger>>,
        mcp_servers: McpServersFn,
        budgets: Budgets,
    ) -> Self {
        Self {
            port,
            ledger,
            mcp_servers,
            budgets,
        }
    }

    /// Announces the start of the session `facts` describe the first time a
    /// run of it opens in this process; the reminder its answers make.
    pub async fn start(&self, facts: StartFacts) -> Option<String> {
        if !self.port.listens(LifecycleHook::Start)
            || !self
                .ledger
                .lock_ignore_poison()
                .admit(facts.session_id.as_deref())
        {
            return None;
        }
        let began = std::time::Instant::now();
        let servers = (self.mcp_servers)(self.budgets.mcp_wait).await;
        let event = policy::start_event(facts, servers);
        let session_id = event.session_id.clone();
        let port = self.port.clone();
        let texts = blocking_within(self.budgets.start, move || port.session_start(&event)).await;
        let texts = answered("session-start", texts).unwrap_or_default();
        tracing::info!(
            target: "dirge::session",
            hook = "session-start",
            session_id = session_id.as_deref().unwrap_or("-"),
            answers = texts.len(),
            took_ms = began.elapsed().as_millis() as u64,
            "session lifecycle hook ran"
        );
        policy::start_reminder(&texts)
    }

    /// Announces the end of the running session, ended from `cause`, when
    /// its start was announced.
    pub async fn end(&self, cause: EndCause) {
        let Some(session_id) = self.ledger.lock_ignore_poison().close() else {
            return;
        };
        if !self.port.listens(LifecycleHook::End) {
            return;
        }
        let began = std::time::Instant::now();
        let event = policy::end_event(session_id.as_deref(), collect::cwd(), cause);
        let reason = event.reason.key();
        let port = self.port.clone();
        let done = blocking_within(self.budgets.end, move || port.session_end(&event)).await;
        if answered("session-end", done).is_some() {
            tracing::info!(
                target: "dirge::session",
                hook = "session-end",
                session_id = session_id.as_deref().unwrap_or("-"),
                reason,
                took_ms = began.elapsed().as_millis() as u64,
                "session lifecycle hook ran"
            );
        }
    }

    /// Ends the running session, then runs `teardown`, so the listeners can
    /// still reach what the teardown closes.
    #[cfg(test)]
    pub async fn end_then<T>(&self, cause: EndCause, teardown: impl Future<Output = T>) -> T {
        self.end(cause).await;
        teardown.await
    }

    /// The step that opens a run of the session `facts` describe: the
    /// start's reminder joins the run's first turn.
    pub fn open_run(self, facts: StartFacts) -> OpenRunFn {
        let lifecycle = Arc::new(self);
        Arc::new(move |opening: RunOpening| {
            let (lifecycle, facts) = (lifecycle.clone(), facts.clone());
            Box::pin(async move {
                let reminder = lifecycle.start(facts).await;
                policy::with_reminder(opening, reminder)
            })
        })
    }
}

/// `answer`, or `None` with why it is missing logged.
fn answered<T>(hook: &str, answer: Result<T, NoAnswer>) -> Option<T> {
    answer
        .map_err(|why| {
            tracing::warn!(target: "dirge::session", hook, %why, "session lifecycle hook skipped");
        })
        .ok()
}

static INSTALLED: std::sync::OnceLock<Lifecycle> = std::sync::OnceLock::new();

/// Make `lifecycle` the one this process announces through. The first
/// install wins.
#[cfg_attr(not(feature = "addons"), allow(dead_code))]
pub fn install(lifecycle: Lifecycle) {
    let _ = INSTALLED.set(lifecycle);
}

/// The lifecycle of this process, once one is installed.
pub fn installed() -> Option<Lifecycle> {
    INSTALLED.get().cloned()
}

/// Announces the end of the running session, ended from `cause`, through
/// the installed lifecycle.
pub async fn end(cause: EndCause) {
    if let Some(lifecycle) = installed() {
        lifecycle.end(cause).await;
    }
}
