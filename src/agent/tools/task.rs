//! `task` — spawn a background **subagent** to carry out an independent piece
//! of work, tracked in the [`BackgroundStore`] with an abort registry;
//! `task_status` polls it. For long/parallel work the main loop shouldn't
//! block on.
//!
//! One of four similarly-named work-tracking surfaces — NOT the phased
//! `/plan` workflow, plan-**mode**, or the in-session `write_todo_list`. See
//! the canonical map in [`crate::agent::plan`].

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::agent_loop::tool::AbortSignal;
use crate::agent::tools::background::{BackgroundStore, TaskState};
use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::config::SubagentWriteIsolation;
use crate::provider::AnyModel;
use crate::sandbox::Sandbox;

#[cfg(feature = "git-worktree")]
type WriterWorktree = crate::extras::git_worktree::WorktreeInfo;

#[cfg(not(feature = "git-worktree"))]
#[allow(dead_code)]
#[derive(Clone)]
struct WriterWorktree {
    branch: String,
    worktree_path: std::path::PathBuf,
    main_repo_path: std::path::PathBuf,
}

fn writer_worktree_enabled(
    is_writer: bool,
    isolation: SubagentWriteIsolation,
    is_microvm: bool,
    sandbox_confines_writes: bool,
) -> Result<bool, &'static str> {
    if !is_writer || isolation == SubagentWriteIsolation::Serialize {
        return Ok(false);
    }
    if is_microvm {
        return match isolation {
            SubagentWriteIsolation::Worktree => {
                Err("worktree write isolation is unavailable in microVM sandbox mode")
            }
            SubagentWriteIsolation::Auto => Ok(false),
            SubagentWriteIsolation::Serialize => Ok(false),
        };
    }
    if !sandbox_confines_writes {
        return match isolation {
            SubagentWriteIsolation::Worktree => Err(
                "worktree write isolation requires a confining sandbox; run dirge with a Linux bwrap sandbox, or the writer's shell could escape the worktree",
            ),
            SubagentWriteIsolation::Auto => Ok(false),
            SubagentWriteIsolation::Serialize => unreachable!(),
        };
    }
    Ok(true)
}

/// Check whether the process's current directory (the parent checkout)
/// is dirty. Returns `Err` when we're not in a git repo at all — the
/// caller should treat that as "no uncommitted work to clobber."
fn current_repo_is_dirty() -> Result<bool, String> {
    let repo = std::env::current_dir()
        .map_err(|e| format!("failed to resolve current directory: {e}"))?
        .canonicalize()
        .map_err(|e| format!("failed to canonicalize repository: {e}"))?;
    #[cfg(feature = "git-worktree")]
    {
        crate::extras::git_worktree::repo_is_dirty(&repo)
    }
    #[cfg(not(feature = "git-worktree"))]
    {
        // No worktree isolation in this build, so no dirty-check machinery is
        // compiled in and there is no worktree to clobber. The caller treats
        // `Ok(false)` exactly like `Err(_)` — "no uncommitted work to lose".
        let _ = repo;
        Ok(false)
    }
}

#[cfg(feature = "git-worktree")]
fn create_writer_worktree(
    task_id: &str,
) -> Result<crate::extras::git_worktree::WorktreeInfo, String> {
    let repo = std::env::current_dir()
        .map_err(|e| format!("failed to resolve current directory: {e}"))?
        .canonicalize()
        .map_err(|e| format!("failed to canonicalize current repository: {e}"))?;
    if crate::extras::git_worktree::repo_is_dirty(&repo)? {
        return Err("current repository has uncommitted changes".to_string());
    }
    let parent = repo
        .parent()
        .ok_or_else(|| "current repository has no parent directory".to_string())?;
    let name = format!("dirge-task-{task_id}");
    crate::extras::git_worktree::create_at(&repo, parent, &name, &name)
}

/// dirge-ov2 Phase D: subagent chat-window event. Sent by `TaskTool`
/// when it spawns / completes a subagent so the UI loop can surface
/// the subagent's lifecycle as a chat-window (Ctrl-N/P/X to switch
/// to it via the multi-chat infrastructure landed in Phases A-C).
///
/// `id` is the subagent's task id (UUID for background tasks; a
/// freshly-generated UUID for foreground tasks). The UI loop keys
/// chat windows on this id so multiple concurrent subagents get
/// distinct windows.
///
/// First-pass design: prompt + final result are emitted; per-token
/// streaming isn't wired through. A follow-up will route the full
/// agent-loop event stream once `TaskTool` migrates from `btw_query`
/// (one-shot, tool-less) to a proper sub-runner with the parent's
/// tool set. Phase A-C laid the multi-chat infrastructure that
/// rewrite needs; Phase D ships visibility today.
#[derive(Debug, Clone)]
// dirge-781c: Reasoning / ToolCall / ToolResult variants are part of
// the streaming surface the chat-tab routes; production producers
// (`btw_query`-based foreground/background subagents) emit only
// Token + Complete + Failed + Aborted today. Sub-runner migration
// will fire the rest. The `Complete.result` field is also kept for
// the same reason — the UI handler currently reads only `id` (the
// Token event carries the text) but a future runner can populate
// it with the final assembled reply when a separate Token stream
// isn't used.
#[allow(dead_code)]
pub enum SubagentChatEvent {
    /// A new subagent is starting. UI loop creates a chat window
    /// named after a short truncation of the prompt and writes the
    /// prompt as the first line.
    Spawn {
        id: String,
        prompt: String,
        agent: Option<String>,
    },
    /// Subagent finished successfully. UI loop writes `result` to
    /// the matching chat window.
    Complete { id: String, result: String },
    /// Subagent errored or timed out. UI loop writes the failure
    /// reason in error color.
    Failed { id: String, error: String },
    /// dirge-781c: streaming assistant token from the subagent.
    /// Currently emitted as a single chunk when `btw_query` returns
    /// (one-shot model has no per-token stream); when the task tool
    /// migrates to a sub-runner this fires per chunk so the user can
    /// watch the reply build up in the subagent's chat slot.
    Token { id: String, text: String },
    /// dirge-781c: streaming reasoning text from the subagent.
    /// Renders dim to mirror the parent chat's reasoning style.
    Reasoning { id: String, text: String },
    /// dirge-781c: subagent emitted a tool call. `args_summary` is a
    /// short, human-readable rendering of the args (one-liner).
    ToolCall {
        id: String,
        tool_name: String,
        args_summary: String,
    },
    /// dirge-781c: subagent tool result. `output_summary` is a short
    /// human-readable preview (single line, truncated) so the tab
    /// shows progress without dumping multi-KB blobs.
    ToolResult {
        id: String,
        tool_name: String,
        output_summary: String,
    },
    /// dirge-781c: subagent was killed via `/kill` or Ctrl+K. UI
    /// writes `(aborted)` to the matching chat slot.
    Aborted { id: String },
}

/// dirge-02tn: subagent chat events are DISPLAY-ONLY — the subagent's
/// real result returns through the normal tool-result path, not this
/// channel. So the channel is BOUNDED and producers use `try_send`:
/// under a sustained UI stall the live chat view degrades (a few dropped
/// tokens/updates) but memory stays bounded and correctness is
/// unaffected. 1024 is generous — normal streaming never fills it.
pub const SUBAGENT_CHAT_CAP: usize = 1024;

pub type SubagentChatSender = mpsc::Sender<SubagentChatEvent>;

/// Receiver side of the subagent chat-event channel — exposed for
/// the UI loop's `tokio::select!` arm. Only consumed in main.rs +
/// ui/mod.rs; marked `dead_code`-allow because the producer side
/// (TaskTool) lives in this module and `cargo check` sees only the
/// definition site, not the cross-module consumer.
#[allow(dead_code)]
pub type SubagentChatReceiver = mpsc::Receiver<SubagentChatEvent>;

/// dirge-ov2 Phase D: process-global sender for subagent chat
/// events. Set once at interactive-session startup; every TaskTool
/// reads it lazily so the builder doesn't need to thread the
/// channel through 13 call sites.
///
/// A follow-up could replace this with proper threading through
/// `BuilderContext` — for now the global keeps the Phase D diff
/// small and the test path (no global set) behaves like pre-ov2.
static SUBAGENT_CHAT_SINK: std::sync::OnceLock<SubagentChatSender> = std::sync::OnceLock::new();

pub fn set_subagent_chat_sink(sink: SubagentChatSender) {
    // OnceLock — first writer wins. Re-set is a no-op (logged via
    // tracing for visibility but not fatal because tests / hot
    // reload may try to set twice).
    if SUBAGENT_CHAT_SINK.set(sink).is_err() {
        tracing::debug!("subagent chat sink already set; ignoring re-set");
    }
}

pub fn subagent_chat_sink() -> Option<SubagentChatSender> {
    SUBAGENT_CHAT_SINK.get().cloned()
}

/// dirge-781c: process-global registry mapping in-flight subagent ids
/// to their `AbortSignal`. Populated when a `TaskTool::call` spawns a
/// subagent; cleared on terminal events (complete / failed / aborted).
///
/// Used by `/kill <id-prefix>` and Ctrl+K to find a live subagent and
/// trigger its abort signal. The map is keyed on the FULL subagent id
/// (UUID for background, freshly-minted UUID for foreground). Prefix
/// resolution lives in `kill_subagent`.
static SUBAGENT_ABORT_REGISTRY: std::sync::OnceLock<Mutex<HashMap<String, AbortSignal>>> =
    std::sync::OnceLock::new();

fn abort_registry() -> &'static Mutex<HashMap<String, AbortSignal>> {
    SUBAGENT_ABORT_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a subagent's abort signal so `/kill` can find it later.
/// Idempotent — re-registering replaces the previous signal (which
/// shouldn't happen in practice since ids are fresh UUIDs).
pub fn register_subagent_abort(id: &str, signal: AbortSignal) {
    let mut map = abort_registry().lock_ignore_poison();
    map.insert(id.to_string(), signal);
}

/// Remove a subagent's abort entry. Called at terminal lifecycle
/// events so the registry doesn't accumulate stale ids.
pub fn unregister_subagent_abort(id: &str) {
    let mut map = abort_registry().lock_ignore_poison();
    map.remove(id);
}

/// Bridge a registered `AbortSignal` (driven by `/kill` / Ctrl+K) to a tooled
/// subagent's `AgentRunner`. The tool-less path polls the signal inline
/// (`tokio::select!` around `btw_query`); the tooled path drives a real
/// `AgentRunner`, so this watcher translates the registry's cancel flag into
/// the runner's cooperative `cancel_tx` + a hard `JoinHandle::abort()`. The
/// drain's `AbortRunnerOnDrop` is the drop-time safety net; this watcher is
/// the external-trigger path. Both are needed.
fn spawn_abort_watcher(
    abort: AbortSignal,
    handle: tokio::task::AbortHandle,
    cancel_tx: tokio::sync::mpsc::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if abort.is_cancelled() {
                let _ = cancel_tx.try_send(()); // cooperative: clean cancelled event
                handle.abort(); // hard kill at the next .await
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
}

struct SubagentCleanup {
    id: String,
    watcher: Option<tokio::task::JoinHandle<()>>,
    /// When set, `Drop` calls `notify_if_running` with a Failed state so a
    /// panic / early-return in the spawned closure doesn't leave the task
    /// stuck Running forever (which would stall the coordinator batch barrier).
    store: Option<BackgroundStore>,
}

impl SubagentCleanup {
    fn new(id: String, watcher: tokio::task::JoinHandle<()>) -> Self {
        Self {
            id,
            watcher: Some(watcher),
            store: None,
        }
    }

    fn with_store(
        id: String,
        watcher: tokio::task::JoinHandle<()>,
        store: BackgroundStore,
    ) -> Self {
        Self {
            id,
            watcher: Some(watcher),
            store: Some(store),
        }
    }

    /// Drop-guard for a spawned closure that has no abort-watcher.
    /// Only carries the store so `Drop` can call `notify_if_running`
    /// on a panic / early-return.
    fn from_store(id: String, store: BackgroundStore) -> Self {
        Self {
            id,
            watcher: None,
            store: Some(store),
        }
    }
}

impl Drop for SubagentCleanup {
    fn drop(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
        unregister_subagent_abort(&self.id);
        if let Some(store) = &self.store {
            store.notify_if_running(
                &self.id,
                TaskState::Failed("subagent exited without producing a result".into()),
            );
        }
    }
}

/// Read-only tool universe for a readonly tooled subagent. Verified against
/// the registration list in `build_loop_tools`. A readonly subagent can read
/// files, search, and browse the web — but never mutate, recurse, write
/// durable state, or attribute work to a session.
const SUBAGENT_READONLY_BASE: &[&str] = &[
    "read",
    "read_minified",
    "grep",
    "find_files",
    "glob",
    "list_dir",
    "repo_overview",
    "websearch",
    "webfetch",
];

/// Read-write tool universe for a readwrite tooled subagent: the readonly
/// base PLUS the write/bash family, so a subagent can edit the code tree and
/// run builds/tests directly. The leaky tools (durable state / session
/// attribution / recursion / interactive) are STILL stripped by
/// [`SUBAGENT_FORCED_EXCLUDES`] after this universe is chosen — readwrite can
/// edit the repo, not write agent state or attribute to a session. So the
/// dirge-mifq leakage gate holds for both tiers.
const SUBAGENT_READWRITE_BASE: &[&str] = &[
    // readonly universe
    "read",
    "read_minified",
    "grep",
    "find_files",
    "glob",
    "list_dir",
    "repo_overview",
    "websearch",
    "webfetch",
    // write family — edit the code tree + run builds/tests. `edit_minified`
    // is feature-gated under `semantic`; filter_loop_tools just no-matches it
    // when the feature is off, so listing it is harmless.
    "write",
    "edit",
    "edit_lines",
    "edit_minified",
    "apply_patch",
    "bash",
    "bash_output",
    "kill_shell",
];

/// Non-negotiable safety floor stripped from EVERY tooled subagent's allow-list,
/// regardless of tier or profile `allow`. Covers three classes:
///   - recursion (`task`/`task_status`) — no fan-out,
///   - durable writes (`memory`/`skill`/`spec`) — no side effects out of band,
///   - session-attribution (`write_todo_list`/`session_search`/`issue`/`graph`)
///     — the dirge-mifq leakage class, blocked until the session-id audit,
///   - interactive (`question`/`plan_enter`/`plan_exit`) — would block the
///     parent UI mid-turn.
///
/// Disjoint from `SUBAGENT_READONLY_BASE`, so a no-op for the readonly tier
/// today, but defense-in-depth for future tiers.
const SUBAGENT_FORCED_EXCLUDES: &[&str] = &[
    "task",
    "task_status",
    "memory",
    "skill",
    "spec",
    "write_todo_list",
    "session_search",
    "issue",
    "graph",
    "question",
    "plan_enter",
    "plan_exit",
];

/// Default per-subagent turn cap. A tooled subagent loops until it stops or
/// this many assistant turns elapse, whichever is first. Prevents a runaway
/// loop from burning the full background timeout. Per-profile overridable via
/// `subagent.max_turns`.
pub const SUBAGENT_DEFAULT_MAX_TURNS: usize = 25;

/// Default system prompt for a tooled subagent whose profile set no prompt
/// body. The tool-less path frames the task inside the prompt; the tooled
/// path has a real system-prompt slot, so the subagent identity lives here
/// and the task is the user message.
const SUBAGENT_DEFAULT_PREAMBLE: &str = "You are a subagent working on a specific subtask. Complete it thoroughly using the tools available to you. You cannot spawn further subagents.";

fn writer_preamble(
    route_preamble: Option<&str>,
    worktree: &std::path::Path,
    branch: &str,
) -> String {
    let mut preamble = route_preamble
        .unwrap_or(SUBAGENT_DEFAULT_PREAMBLE)
        .to_string();
    preamble.push_str(&format!(
        "\n\nYou are an isolated writer. Your root is {} on branch {}. Do not modify the parent checkout. Inspect git status, stage intended changes, make one descriptive commit, and report the commit hash, changed files, tests run, and unresolved issues.",
        worktree.display(),
        branch,
    ));
    preamble
}

/// Resolve a profile's subagent policy into the exact allow-list for the
/// tooled fork.
///
/// Returns `None` for a tool-less subagent (the unchanged `btw_query`
/// path). `Some` is the filtered tool set. Both tiers resolve:
/// `Readonly` uses [`SUBAGENT_READONLY_BASE`] (reads + search + web),
/// `ReadWrite` uses [`SUBAGENT_READWRITE_BASE`] (readonly + write/bash).
///
/// Tier invariant: the final set is intersected with the tier's universe, so
/// `allow` can never escalate past the tier (a readonly profile can't
/// `allow` its way to `edit`); `deny` narrows; [`SUBAGENT_FORCED_EXCLUDES`]
/// is then stripped as the mandatory floor, so EVEN readwrite can't reach a
/// session-attributing / durable-state / recursive / interactive tool.
pub fn resolve_subagent_allow(
    p: &crate::context::agent_defs::SubagentToolPolicy,
) -> Option<Vec<String>> {
    use crate::context::agent_defs::SubagentToolTier;
    use std::collections::BTreeSet;
    let universe: &[&str] = match p.tier {
        SubagentToolTier::Toolless => return None,
        SubagentToolTier::Readonly => SUBAGENT_READONLY_BASE,
        SubagentToolTier::ReadWrite => SUBAGENT_READWRITE_BASE,
    };
    let mut set: BTreeSet<String> = if p.allow.is_empty() {
        universe.iter().map(|s| s.to_ascii_lowercase()).collect()
    } else {
        p.allow
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .filter(|a| universe.iter().any(|u| u.eq_ignore_ascii_case(a)))
            .collect()
    };
    for d in &p.deny {
        set.remove(&d.to_ascii_lowercase());
    }
    for x in SUBAGENT_FORCED_EXCLUDES {
        set.remove(&x.to_ascii_lowercase());
    }
    Some(set.into_iter().collect())
}

/// Resolve a profile's [`SubagentMcpAccess`] against the live agent's set of
/// MCP tool names (issue #701). Returns the MCP tool names to ADD to the
/// subagent's allow-list on top of its tier universe.
///
/// The result is always a subset of `available`, so a profile can never use
/// `subagent_mcp` to name a non-MCP tool (a built-in like `bash`): the tier
/// cap on built-in tools stays honest. `Only` names not present in
/// `available` (typo / disconnected server) are dropped with a warning so a
/// misspelled name isn't a silent no-op. Case-insensitive against `available`.
pub fn resolve_mcp_selection(
    access: &crate::context::agent_defs::SubagentMcpAccess,
    available: &std::collections::HashSet<String>,
) -> Vec<String> {
    use crate::context::agent_defs::SubagentMcpAccess;
    match access {
        SubagentMcpAccess::None => Vec::new(),
        SubagentMcpAccess::All => available.iter().cloned().collect(),
        SubagentMcpAccess::Only(names) => names
            .iter()
            .filter_map(
                |n| match available.iter().find(|a| a.eq_ignore_ascii_case(n)) {
                    Some(a) => Some(a.clone()),
                    None => {
                        tracing::warn!(
                            target: "dirge::agents",
                            tool = %n,
                            "subagent_mcp names '{n}' but no such MCP tool is connected; ignoring"
                        );
                        None
                    }
                },
            )
            .collect(),
    }
}

/// Per-subagent turn cap, honoring a profile override or the default.
pub fn resolve_subagent_max_turns(p: &crate::context::agent_defs::SubagentToolPolicy) -> usize {
    p.max_turns.unwrap_or(SUBAGENT_DEFAULT_MAX_TURNS)
}

/// Per-subagent timeout honoring profile configuration, clamped to a bounded
/// interval so a typo cannot create either immediate failures or stuck tasks.
pub fn resolve_subagent_timeout(
    p: &crate::context::agent_defs::SubagentToolPolicy,
) -> std::time::Duration {
    const DEFAULT_SECS: u64 = 600;
    const MIN_SECS: u64 = 30;
    const MAX_SECS: u64 = 3600;
    let raw = p.timeout_secs.unwrap_or(DEFAULT_SECS);
    let secs = raw.clamp(MIN_SECS, MAX_SECS);
    if secs != raw {
        tracing::warn!(
            timeout_secs = raw,
            resolved_timeout_secs = secs,
            "subagent timeout is outside the supported range; clamping"
        );
    }
    std::time::Duration::from_secs(secs)
}

/// dirge-ykeu Phase 4: a pre-resolved subagent routing for one agent profile.
/// Built once at startup (in `main`, where the client + config + registry are
/// all available) so `TaskTool` needs neither the client nor the config to
/// route a `task(agent="<name>")` call to a profile's model + system prompt.
#[derive(Clone)]
pub struct SubagentRoute {
    /// Model to run the subagent on. `None` → use the default subagent model
    /// (the profile didn't pin a model).
    pub model: Option<AnyModel>,
    /// System prompt override for the subagent. `None` → default preamble.
    pub preamble: Option<String>,
    /// `None` → tool-less subagent (the unchanged `btw_query` one-shot path).
    /// `Some` → the exact tool allow-list for a tooled fork; the subagent runs
    /// a real filtered agent loop with this set (readonly base minus the
    /// mandatory floor, narrowed by any profile `deny`).
    pub tool_allow: Option<Vec<String>>,
    /// Per-subagent turn cap. Honors a profile `subagent.max_turns` override
    /// else [`SUBAGENT_DEFAULT_MAX_TURNS`]. Only consumed on the tooled path.
    pub max_turns: usize,
    pub timeout: std::time::Duration,
    pub tier: crate::context::agent_defs::SubagentToolTier,
    /// MCP tools this profile grants its subagent on top of the tier universe
    /// (#701). Resolved against the live agent's MCP-tool set at fork time
    /// (not here) because MCP servers connect after routes are built. Ignored
    /// on the tool-less path (no loop to attach tools to).
    pub mcp: crate::context::agent_defs::SubagentMcpAccess,
}

#[derive(Debug, Clone)]
pub struct CoordinatorProfile {
    pub name: String,
    pub description: Option<String>,
    pub tier: crate::context::agent_defs::SubagentToolTier,
}

#[derive(Debug, Clone, Default)]
pub struct CoordinatorProfiles {
    pub readonly: Vec<CoordinatorProfile>,
    pub readwrite: Vec<CoordinatorProfile>,
}

pub fn resolve_coordinator_profiles(
    agent_defs: &crate::context::agent_defs::AgentRegistry,
) -> CoordinatorProfiles {
    let mut profiles = CoordinatorProfiles::default();
    for definition in agent_defs.iter() {
        let profile = CoordinatorProfile {
            name: definition.name.clone(),
            description: definition.description.clone(),
            tier: definition.subagent.tier.clone(),
        };
        match &profile.tier {
            crate::context::agent_defs::SubagentToolTier::Readonly => {
                profiles.readonly.push(profile)
            }
            crate::context::agent_defs::SubagentToolTier::ReadWrite => {
                profiles.readwrite.push(profile)
            }
            crate::context::agent_defs::SubagentToolTier::Toolless => {}
        }
    }
    profiles
}

/// Process-global map of profile name → routing. Set once at interactive
/// startup; unset on tool-less / test paths (where `task(agent=…)` simply
/// reports the feature isn't available). Mirrors the `set_subagent_chat_sink`
/// process-global pattern to keep the diff off every `build_agent` call site.
static SUBAGENT_ROUTES: std::sync::OnceLock<HashMap<String, SubagentRoute>> =
    std::sync::OnceLock::new();

/// Install the subagent routing table. First writer wins (a no-op re-set is
/// logged, not fatal, matching the chat-sink global).
pub fn set_subagent_routes(routes: HashMap<String, SubagentRoute>) {
    if SUBAGENT_ROUTES.set(routes).is_err() {
        tracing::debug!("subagent routes already set; ignoring re-set");
    }
}

/// Look up a profile's routing by name. `None` distinguishes "no routing table
/// installed" from "name not in table" only at the call site (both are errors
/// for an explicit `agent=` arg).
fn subagent_route(name: &str) -> Option<SubagentRoute> {
    SUBAGENT_ROUTES.get()?.get(name).cloned()
}

/// Whether any routing table is installed (profiles feature active).
fn subagent_routes_available() -> bool {
    SUBAGENT_ROUTES.get().is_some_and(|m| !m.is_empty())
}

/// Sorted profile names, for the `task` tool definition's `agent` enum.
fn subagent_route_names() -> Vec<String> {
    let Some(map) = SUBAGENT_ROUTES.get() else {
        return Vec::new();
    };
    let mut names: Vec<String> = map.keys().cloned().collect();
    names.sort();
    names
}

/// Outcome of a `/kill <id-prefix>` resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillOutcome {
    /// No in-flight subagent matched the prefix.
    NotFound,
    /// Multiple in-flight subagents matched the prefix — ambiguous;
    /// the caller should ask the user to supply more characters.
    /// Carries the matching full ids so the UI can list them.
    Ambiguous(Vec<String>),
    /// Exactly one match — its `AbortSignal::cancel()` was triggered.
    /// Carries the full id so the UI can echo back what got killed.
    Killed(String),
}

/// Resolve `id_prefix` against the in-flight subagent registry and,
/// when it matches exactly one entry, fire the abort signal.
///
/// Resolution rules:
///   - Empty prefix → `NotFound` (refuse to kill blindly).
///   - Exact match on a full id → kill that one even if other ids
///     also start with the same string.
///   - One id starts with the prefix → kill it.
///   - Multiple ids start with the prefix → `Ambiguous` (no-op).
///   - Zero matches → `NotFound`.
pub fn kill_subagent(id_prefix: &str) -> KillOutcome {
    let trimmed = id_prefix.trim();
    if trimmed.is_empty() {
        return KillOutcome::NotFound;
    }
    let map = abort_registry().lock_ignore_poison();
    // Exact match wins outright.
    if let Some(sig) = map.get(trimmed) {
        sig.cancel();
        return KillOutcome::Killed(trimmed.to_string());
    }
    let matches: Vec<String> = map
        .keys()
        .filter(|k| k.starts_with(trimmed))
        .cloned()
        .collect();
    match matches.len() {
        0 => KillOutcome::NotFound,
        1 => {
            let id = matches.into_iter().next().unwrap();
            if let Some(sig) = map.get(&id) {
                sig.cancel();
            }
            KillOutcome::Killed(id)
        }
        _ => KillOutcome::Ambiguous(matches),
    }
}

/// Snapshot of currently-registered in-flight subagent ids. Used by
/// the UI to drive Ctrl+K (resolve the focused-tab's id back to a
/// full registry key) without exposing the mutex.
#[allow(dead_code)]
pub fn registered_subagent_ids() -> Vec<String> {
    let map = abort_registry().lock_ignore_poison();
    map.keys().cloned().collect()
}

/// Test-only helper: clear the abort registry between cases. Tests
/// run in parallel by default; without this they'd leak ids across
/// test invocations and corrupt prefix-resolution assertions.
#[cfg(test)]
pub fn clear_abort_registry_for_test() {
    let mut map = abort_registry().lock_ignore_poison();
    map.clear();
}

/// Truncate a string to one line of at most `max` chars, collapsing whitespace.
fn one_line(s: &str, max: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(max).collect();
        format!("{truncated}…")
    }
}

/// One-line summary of a tool-call's JSON args for the chat-window ticker.
fn summarize_json_args(args: &serde_json::Value, max: usize) -> String {
    let raw = match args {
        serde_json::Value::Null => String::new(),
        // Prefer a compact `key=val` rendering for object args (the common case).
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}={}", value_brief(v)))
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    };
    one_line(&raw, max)
}

/// Brief rendering of a JSON value for the args summary (strings unquoted,
/// arrays/objects shown by length).
fn value_brief(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Array(a) => format!("[{}]", a.len()),
        serde_json::Value::Object(o) => format!("{{+{}}}", o.len()),
    }
}

/// dirge-cnx3: leads a payload whose subagent was stopped by the turn cap
/// rather than finishing. The parent renders subagent results under a
/// `completed:` heading (`background.rs::format_notifications`), so without an
/// in-band marker a cut-off run is indistinguishable from a finished one.
pub(crate) const SUBAGENT_TRUNCATED_MARKER: &str = "[dirge] SUBAGENT STOPPED EARLY — turn cap reached. The text below is its \
     last narration, NOT a final answer. Its work is incomplete: re-dispatch \
     the remainder or verify the claims yourself before relying on them.";

/// dirge-vxmi: leads a payload assembled from the accumulated token stream
/// because the run produced no final assistant text. That stream is every
/// turn's running narration, not a report.
pub(crate) const SUBAGENT_TRACE_FALLBACK_MARKER: &str = "[dirge] SUBAGENT PRODUCED NO FINAL ANSWER — the text below is its \
     turn-by-turn narration, not a report. Treat any conclusion in it as \
     unverified.";

/// Prefix the appropriate marker (if any) onto a subagent's payload.
///
/// The turn-cap marker wins when both conditions hold: a run that blew its
/// budget is the more actionable fact, and "no final answer" is the expected
/// consequence of being cut off mid-turn rather than an independent signal.
fn mark_subagent_payload(
    body: String,
    turn_cap_notice: Option<String>,
    no_final_text: bool,
) -> String {
    let marker = match (&turn_cap_notice, no_final_text) {
        (Some(_), _) => SUBAGENT_TRUNCATED_MARKER,
        (None, true) => SUBAGENT_TRACE_FALLBACK_MARKER,
        (None, false) => return body,
    };
    let mut out = String::with_capacity(marker.len() + body.len() + 128);
    out.push_str(marker);
    if let Some(notice) = turn_cap_notice {
        out.push('\n');
        out.push_str(&notice);
    }
    out.push_str("\n\n");
    out.push_str(&body);
    out
}

/// Drain a tooled subagent's `AgentRunner` to completion, relaying each
/// event into the subagent chat-window channel. This is the first live
/// producer for the dormant `SubagentChatEvent::{Token,Reasoning,ToolCall,
/// ToolResult,Complete,Failed}` variants (dirge-781c). Mirrors
/// `plan::runtime::collect_runner_text` but emits per-event instead of
/// silently consuming.
///
/// Returns the final assistant text (`Done.response`, falling back to the
/// accumulated token stream) or the first error. `AbortRunnerOnDrop` is held
/// for the drain so a cancelled/early-returning caller actually kills the fork.
///
/// dirge-cnx3 / dirge-vxmi: the payload is MARKED when it isn't a real answer.
/// A subagent's terminal state is not visible in `Done` alone:
///
///   - Turn cap: `run.rs` emits the max-turns notice as a `SystemNotice` and
///     appends it as a USER message, so `AgentEnd`'s last ASSISTANT message —
///     and hence `Done.response` — is the in-flight tool-call turn's preamble.
///     A bare "Now let me check the next file" then reaches the parent under a
///     `completed:` heading and reconciles as finished work.
///   - No final text: an empty `Done.response` falls back to the accumulated
///     token stream, which is every turn's narration rather than a report.
///
/// Both cases get a leading marker so the dispatching agent can tell a
/// finished subagent from a cut-off one. A clean answer is returned verbatim.
async fn drain_subagent_runner(
    runner: crate::agent::runner::AgentRunner,
    id: &str,
    emit: impl Fn(SubagentChatEvent),
) -> Result<String, String> {
    use crate::agent::runner::AbortRunnerOnDrop;
    use crate::event::AgentEvent;

    let crate::agent::runner::AgentRunner {
        event_rx,
        task,
        cancel_tx,
        ..
    } = runner;
    let _guard = AbortRunnerOnDrop { task, cancel_tx };
    let mut rx = event_rx;
    let mut text = String::new();
    // The max-turns notice, if the run hit its cap. Captured here because
    // `SystemNotice` is otherwise UI-only — nothing downstream of the fork
    // ever sees it, so without this the cutoff is invisible to the parent.
    let mut turn_cap_notice: Option<String> = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::SystemNotice { content } => {
                if content.starts_with(crate::agent::agent_loop::run::MAX_TURNS_NOTICE_PREFIX) {
                    turn_cap_notice = Some(content.to_string());
                }
            }
            AgentEvent::Token(t) => {
                text.push_str(&t);
            }
            AgentEvent::Reasoning(t) => emit(SubagentChatEvent::Reasoning {
                id: id.to_string(),
                text: t.to_string(),
            }),
            AgentEvent::ToolCall { name, args, .. } => emit(SubagentChatEvent::ToolCall {
                id: id.to_string(),
                tool_name: name.to_string(),
                args_summary: summarize_json_args(&args, 120),
            }),
            AgentEvent::ToolResult { output, .. } => emit(SubagentChatEvent::ToolResult {
                id: id.to_string(),
                tool_name: String::new(), // AgentEvent carries no name here
                output_summary: one_line(&output, 120),
            }),
            AgentEvent::Done { response, .. } => {
                let no_final_text = response.is_empty();
                let body = if no_final_text {
                    std::mem::take(&mut text)
                } else {
                    response.to_string()
                };
                let response = mark_subagent_payload(body, turn_cap_notice.take(), no_final_text);
                emit(SubagentChatEvent::Token {
                    id: id.to_string(),
                    text: response.clone(),
                });
                emit(SubagentChatEvent::Complete {
                    id: id.to_string(),
                    result: response.clone(),
                });
                return Ok(response);
            }
            AgentEvent::Error(msg) => return Err(msg.to_string()),
            AgentEvent::ContextOverflow { error, .. } => return Err(error.to_string()),
            _ => {}
        }
    }
    Err("subagent runner ended without Done".to_string())
}

pub struct TaskTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    model: AnyModel,
    bg_store: BackgroundStore,
    sandbox: Sandbox,
    write_isolation: SubagentWriteIsolation,
    /// dirge-ov2: send-side of the subagent-chat-event channel.
    /// `Option` so `--no-tools` paths / tests can omit the UI sink
    /// without forcing every TaskTool builder to manufacture one.
    chat_sink: Option<SubagentChatSender>,
}

impl TaskTool {
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        model: AnyModel,
        bg_store: BackgroundStore,
        sandbox: Sandbox,
        write_isolation: SubagentWriteIsolation,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            model,
            bg_store,
            sandbox,
            write_isolation,
            chat_sink: None,
        }
    }

    /// dirge-ov2: wire the subagent-chat-event sender. Called by the
    /// agent builder when constructing the TaskTool for an
    /// interactive session. Headless / test paths skip this so the
    /// tool behaves identically to the pre-ov2 implementation.
    ///
    /// Currently unused in production — the process-global sink
    /// (set via `set_subagent_chat_sink`) is the wired path. Kept
    /// for tests + future per-instance overrides.
    #[allow(dead_code)]
    pub fn with_chat_sink(mut self, sink: SubagentChatSender) -> Self {
        self.chat_sink = Some(sink);
        self
    }

    /// dirge-ov2 helper: fire-and-forget a chat event. Prefers the
    /// instance-bound sink (set via `with_chat_sink`); falls back
    /// to the process-global sink set at UI-loop startup. If
    /// neither is installed (headless / tests) the event is
    /// silently discarded — never block the subagent or fail the
    /// tool call on UI plumbing trouble.
    fn emit_chat(&self, event: SubagentChatEvent) {
        if let Some(sink) = &self.chat_sink {
            let _ = sink.try_send(event);
            return;
        }
        if let Some(sink) = subagent_chat_sink() {
            let _ = sink.try_send(event);
        }
    }

    /// Tooled subagent path (v1: readonly tier). Forks a filtered runner off
    /// the live agent (its tool registry, optionally the profile's model) and
    /// drains it, relaying each event into the subagent chat window — the
    /// first live producer for the dormant `ToolCall`/`ToolResult`/`Reasoning`
    /// variants. `background` selects detached (returns a task_id) vs inline
    /// (blocks the parent's tool call), mirroring the tool-less path's shape.
    #[allow(clippy::too_many_arguments)]
    async fn run_tooled(
        &self,
        route_model: Option<AnyModel>,
        route_preamble: Option<String>,
        allowed: Vec<String>,
        mcp: crate::context::agent_defs::SubagentMcpAccess,
        max_turns: usize,
        timeout: std::time::Duration,
        background: bool,
        is_writer: bool,
        retry_of: Option<String>,
        prompt: String,
        agent_name: Option<String>,
    ) -> Result<String, ToolError> {
        let agent = crate::provider::current_agent().ok_or_else(|| {
            ToolError::Msg(
                "tooled subagents require a live interactive agent; this path (headless/test) \
                 does not support them. Use a tool-less profile or omit `agent`."
                    .into(),
            )
        })?;

        if background {
            let running = self.bg_store.running_count();
            let cap = BackgroundStore::max_concurrent();
            if running >= cap {
                return Err(ToolError::Msg(format!(
                    "background subagent cap reached ({}/{} in flight). Wait for one to finish \
                     (use task_status) or run inline (background=false). Capping prevents fan-out \
                     from burning the API budget.",
                    running, cap,
                )));
            }
            let task_id = Uuid::new_v4().to_string();
            let coordinator_dispatch = self.bg_store.coordinator_strategy().is_some();
            let attempts_isolated_writer = coordinator_dispatch
                && writer_worktree_enabled(
                    is_writer,
                    self.write_isolation,
                    self.sandbox.is_microvm(),
                    self.sandbox.confines_writes(),
                )
                .map_err(|error| ToolError::Msg(error.to_string()))?;
            if coordinator_dispatch {
                self.bg_store
                    .insert_coordinator_dispatch(
                        task_id.clone(),
                        prompt.clone(),
                        is_writer,
                        attempts_isolated_writer,
                        retry_of.as_deref(),
                    )
                    .map_err(ToolError::Msg)?;
            } else {
                self.bg_store.insert_for_dispatch(task_id.clone());
            }

            // Coordinator ReadWrite agents may be rooted in a dedicated
            // worktree. Auto preserves serialization when any prerequisite is
            // unavailable; explicit Worktree reports that failure.
            let rooted_worktree: Option<(WriterWorktree, std::path::PathBuf, String)> =
                if attempts_isolated_writer {
                    #[cfg(feature = "git-worktree")]
                    match create_writer_worktree(&task_id) {
                        Ok(info) => {
                            let main_git_dir = info.main_repo_path.join(".git");
                            let base_commit = match crate::extras::git_worktree::head_commit(
                                &info.main_repo_path,
                            ) {
                                Ok(commit) => commit,
                                Err(error) => {
                                    let _ = crate::extras::git_worktree::remove_worktree_if_clean(
                                        &info,
                                    );
                                    self.bg_store
                                        .notify(&task_id, TaskState::Failed(error.clone()));
                                    return Err(ToolError::Msg(error));
                                }
                            };
                            if let Err(error) = self.bg_store.set_coordinator_dispatch_worktree(
                                &task_id,
                                info.branch.clone(),
                                info.worktree_path.display().to_string(),
                            ) {
                                let _ =
                                    crate::extras::git_worktree::remove_worktree_if_clean(&info);
                                self.bg_store
                                    .notify(&task_id, TaskState::Failed(error.clone()));
                                return Err(ToolError::Msg(error));
                            }
                            self.bg_store
                                .register_writer_worktree(task_id.clone(), info.clone());
                            Some((info, main_git_dir, base_commit))
                        }
                        Err(_) if self.write_isolation == SubagentWriteIsolation::Auto => {
                            tracing::warn!(
                                task_id = %task_id,
                                "worktree writer isolation unavailable; falling back to serialized parent checkout"
                            );
                            if let Err(error) = self
                                .bg_store
                                .set_coordinator_dispatch_isolated(&task_id, false)
                            {
                                self.bg_store
                                    .notify(&task_id, TaskState::Failed(error.clone()));
                                return Err(ToolError::Msg(error));
                            }
                            None
                        }
                        Err(error) => {
                            self.bg_store
                                .notify(&task_id, TaskState::Failed(error.clone()));
                            return Err(ToolError::Msg(error));
                        }
                    }
                    #[cfg(not(feature = "git-worktree"))]
                    {
                        if self.write_isolation == SubagentWriteIsolation::Auto {
                            tracing::warn!(
                                task_id = %task_id,
                                "git-worktree support is unavailable; falling back to serialized parent checkout"
                            );
                            if let Err(error) = self
                                .bg_store
                                .set_coordinator_dispatch_isolated(&task_id, false)
                            {
                                self.bg_store
                                    .notify(&task_id, TaskState::Failed(error.clone()));
                                return Err(ToolError::Msg(error));
                            }
                            None
                        } else {
                            let error =
                                "worktree write isolation requires the git-worktree feature"
                                    .to_string();
                            self.bg_store
                                .notify(&task_id, TaskState::Failed(error.clone()));
                            return Err(ToolError::Msg(error));
                        }
                    }
                } else {
                    None
                };
            // A read-write subagent without worktree isolation runs in the
            // parent checkout. Refuse to do this when the parent is dirty —
            // the writer's edits and `git add -A` + `git commit` would
            // clobber uncommitted work (data loss). This fires for BOTH the
            // attempts_isolated_writer=false path AND the auto-fallback after
            // a failed worktree creation.
            if is_writer && rooted_worktree.is_none() {
                match current_repo_is_dirty() {
                    Ok(true) => {
                        let error = "cannot run a read-write subagent in a dirty parent \
                                     checkout without worktree isolation — commit or \
                                     stash your changes, or run under a Linux bwrap \
                                     sandbox for isolated worktrees"
                            .to_string();
                        self.bg_store
                            .notify(&task_id, TaskState::Failed(error.clone()));
                        return Err(ToolError::Msg(error));
                    }
                    Ok(false) => { /* clean — safe to proceed */ }
                    Err(_) => { /* not a git repo — no uncommitted work to clobber */ }
                }
            }
            self.bg_store.notify_started(&task_id);
            self.emit_chat(SubagentChatEvent::Spawn {
                id: task_id.clone(),
                prompt: prompt.clone(),
                agent: agent_name.clone(),
            });
            let abort = AbortSignal::new();
            register_subagent_abort(&task_id, abort.clone());

            let store = self.bg_store.clone();
            let chat_sink = self.chat_sink.clone();
            let tid_for_task = task_id.clone();
            let preamble_for_task = route_preamble.clone();
            let allowed_for_task = allowed.clone();
            let mcp_for_task = mcp.clone();
            let model_for_task = route_model.clone();
            let abort_for_task = abort.clone();
            let permission_for_task = self.permission.clone();
            let ask_tx_for_task = self.ask_tx.clone();
            let sandbox_for_task = self.sandbox.clone();
            let rooted_worktree_for_task = rooted_worktree.clone();

            let route_timeout = timeout;
            let store_for_task = store.clone();
            let handle = tokio::spawn(async move {
                let child_sid = format!("sub-{}", crate::agent::runner::uuid_v4_simple());
                let system_prompt = if let Some((info, _, _)) = rooted_worktree_for_task.as_ref() {
                    writer_preamble(
                        preamble_for_task.as_deref(),
                        &info.worktree_path,
                        &info.branch,
                    )
                } else {
                    preamble_for_task
                        .as_deref()
                        .unwrap_or(SUBAGENT_DEFAULT_PREAMBLE)
                        .to_string()
                };
                #[cfg_attr(not(feature = "git-worktree"), allow(unused_variables))]
                let runner = if let Some((info, main_git_dir, base_commit)) =
                    rooted_worktree_for_task.as_ref()
                {
                    let worktree = info.worktree_path.clone();
                    let root = match crate::agent::tools::ToolRoot::new(&worktree) {
                        Ok(root) => root,
                        Err(error) => {
                            let state = TaskState::Failed(error.to_string());
                            #[cfg(feature = "git-worktree")]
                            {
                                let commits = crate::extras::git_worktree::worktree_commits_since(
                                    info,
                                    base_commit,
                                )
                                .unwrap_or_default();
                                let dirty = crate::extras::git_worktree::worktree_is_dirty(info)
                                    .unwrap_or(true);
                                let retained = dirty || !commits.is_empty();
                                if !retained {
                                    let _ =
                                        crate::extras::git_worktree::remove_worktree_if_clean(info);
                                }
                                store_for_task.set_coordinator_dispatch_worktree_outcome(
                                    &tid_for_task,
                                    commits,
                                    dirty,
                                    retained,
                                );
                                store_for_task.unregister_writer_worktree(&tid_for_task);
                            }
                            store_for_task.notify(&tid_for_task, state);
                            return;
                        }
                    };
                    // Isolated worktree writers build a fresh tool registry
                    // rooted at the worktree; MCP tools aren't rooted there,
                    // so `subagent_mcp` (#701) is not wired into this path in
                    // v1 — it applies to the shared-checkout fork below.
                    let tools = crate::agent::builder::build_rooted_writer_tools(
                        root,
                        permission_for_task,
                        ask_tx_for_task,
                        sandbox_for_task,
                        crate::sandbox::SandboxExecutionRoot {
                            worktree,
                            main_git_dir: main_git_dir.clone(),
                        },
                    )
                    .await;
                    agent.spawn_subagent_runner_with_tools(
                        prompt.clone(),
                        system_prompt,
                        tools,
                        &child_sid,
                        max_turns,
                        model_for_task.as_ref(),
                    )
                } else {
                    agent.spawn_subagent_runner(
                        prompt.clone(),
                        system_prompt,
                        &allowed_for_task,
                        &mcp_for_task,
                        &child_sid,
                        max_turns,
                        model_for_task.as_ref(),
                    )
                };
                let abort_watcher = spawn_abort_watcher(
                    abort_for_task.clone(),
                    runner.task.abort_handle(),
                    runner.cancel_tx.clone(),
                );
                let _cleanup = SubagentCleanup::with_store(
                    tid_for_task.clone(),
                    abort_watcher,
                    store_for_task.clone(),
                );
                let chat_sink_drain = chat_sink.clone();
                let emit = move |ev: SubagentChatEvent| {
                    if let Some(sink) = &chat_sink_drain {
                        let _ = sink.try_send(ev);
                    } else if let Some(sink) = subagent_chat_sink() {
                        let _ = sink.try_send(ev);
                    }
                };
                let drained = drain_subagent_runner(runner, &tid_for_task, emit);
                let outer = tokio::time::timeout(route_timeout, drained).await;
                let aborted = abort_for_task.is_cancelled();
                let (state, chat_event) = match outer {
                    Ok(Ok(text)) => (TaskState::Completed(text), None),
                    Ok(Err(e)) => {
                        let aborted_msg = "aborted by user".to_string();
                        if aborted {
                            (
                                TaskState::Cancelled(aborted_msg.clone()),
                                Some(SubagentChatEvent::Aborted {
                                    id: tid_for_task.clone(),
                                }),
                            )
                        } else {
                            (
                                TaskState::Failed(e.clone()),
                                Some(SubagentChatEvent::Failed {
                                    id: tid_for_task.clone(),
                                    error: e,
                                }),
                            )
                        }
                    }
                    Err(_) => {
                        let msg = format!("subagent timed out after {}s", route_timeout.as_secs());
                        (
                            TaskState::Failed(msg.clone()),
                            Some(SubagentChatEvent::Failed {
                                id: tid_for_task.clone(),
                                error: msg,
                            }),
                        )
                    }
                };
                if let Some(chat_event) = chat_event {
                    if let Some(sink) = chat_sink {
                        let _ = sink.try_send(chat_event);
                    } else if let Some(sink) = subagent_chat_sink() {
                        let _ = sink.try_send(chat_event);
                    }
                }
                #[cfg(feature = "git-worktree")]
                if let Some((info, _, base_commit)) = rooted_worktree_for_task.as_ref() {
                    let commits =
                        crate::extras::git_worktree::worktree_commits_since(info, base_commit)
                            .unwrap_or_default();
                    let dirty =
                        crate::extras::git_worktree::worktree_is_dirty(info).unwrap_or(true);
                    let retained = dirty || !commits.is_empty();
                    if !retained {
                        let _ = crate::extras::git_worktree::remove_worktree_if_clean(info);
                    }
                    store_for_task.set_coordinator_dispatch_worktree_outcome(
                        &tid_for_task,
                        commits,
                        dirty,
                        retained,
                    );
                    store_for_task.unregister_writer_worktree(&tid_for_task);
                }
                store_for_task.notify(&tid_for_task, state);
            });
            store.attach_handle(&task_id, handle);

            Ok(format!(
                "background task started — task_id: {}\n\nThe subagent runs in the background. \
                 Completion will be delivered automatically as a <system-reminder> at the start \
                 of your next turn. Do NOT poll task_status or sleep waiting — continue with \
                 other work.",
                task_id
            ))
        } else {
            let task_id = Uuid::new_v4().to_string();
            self.emit_chat(SubagentChatEvent::Spawn {
                id: task_id.clone(),
                prompt: prompt.clone(),
                agent: agent_name.clone(),
            });
            let abort = AbortSignal::new();
            register_subagent_abort(&task_id, abort.clone());

            let child_sid = format!("sub-{}", crate::agent::runner::uuid_v4_simple());
            let system_prompt = route_preamble
                .as_deref()
                .unwrap_or(SUBAGENT_DEFAULT_PREAMBLE)
                .to_string();
            let runner = agent.spawn_subagent_runner(
                prompt.clone(),
                system_prompt,
                &allowed,
                &mcp,
                &child_sid,
                max_turns,
                route_model.as_ref(),
            );
            let abort_watcher = spawn_abort_watcher(
                abort.clone(),
                runner.task.abort_handle(),
                runner.cancel_tx.clone(),
            );

            let _cleanup = SubagentCleanup::new(task_id.clone(), abort_watcher);

            let result = tokio::time::timeout(
                timeout,
                drain_subagent_runner(runner, &task_id, |ev| self.emit_chat(ev)),
            )
            .await;
            let aborted = abort.is_cancelled();
            match result {
                Ok(Ok(text)) => {
                    let outcome =
                        crate::agent::tools::output_relay::relay_if_large("task", text, "");
                    Ok(outcome.text)
                }
                Ok(Err(e)) => {
                    if aborted {
                        self.emit_chat(SubagentChatEvent::Aborted { id: task_id });
                        Err(ToolError::Msg("Subagent aborted by user".to_string()))
                    } else {
                        self.emit_chat(SubagentChatEvent::Failed {
                            id: task_id,
                            error: e.clone(),
                        });
                        Err(ToolError::Msg(format!("Subagent error: {e}")))
                    }
                }
                Err(_) => {
                    let message = format!("Subagent timed out after {}s", timeout.as_secs());
                    self.emit_chat(SubagentChatEvent::Failed {
                        id: task_id,
                        error: message.clone(),
                    });
                    Err(ToolError::Msg(message))
                }
            }
        }
    }
}

#[derive(Deserialize)]
pub struct Args {
    pub prompt: String,
    #[serde(default)]
    pub background: Option<bool>,
    /// dirge-ykeu Phase 4: optional agent-profile name. When set, the subagent
    /// runs on that profile's model + system prompt (defined in `.dirge/agents/`
    /// or `config.json` `agents`). Omit for the default subagent.
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub retry_of: Option<String>,
}

fn normalize_retry_of(retry_of: Option<String>) -> Option<String> {
    retry_of
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

impl Tool for TaskTool {
    const NAME: &'static str = "task";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        let mut description = "Spawn a subagent to handle a specific subtask. The subagent runs as a one-shot query (no tools) and returns its result inline. Use for research, analysis, or planning subtasks that don't require file access. Set background=true to run asynchronously — completion is delivered to you automatically as a <system-reminder> at the start of your next turn. Do NOT poll task_status in a loop or sleep waiting; continue with other work."
            .to_string();

        let mut properties = serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Task description for the subagent"
                },
                "background": {
                    "type": "boolean",
                    "description": "Run asynchronously (default: false). When true, returns a task_id immediately. The result is delivered automatically as a <system-reminder> on your next turn — do NOT poll task_status."
                },
                "retry_of": {
                    "type": "string",
                    "description": "Coordinator-only: retry one failed background task once."
                }
            },
            "required": ["prompt"]
        });

        // dirge-ykeu Phase 4: advertise the `agent` param only when profiles
        // are installed, with the available names as an enum so the model
        // can't invent one.
        let names = subagent_route_names();
        if !names.is_empty() {
            description.push_str(&format!(
                " Optionally set agent=<name> to run the subagent under a defined profile (its own model + system prompt). Available profiles: {}.",
                names.join(", ")
            ));
            if let Some(props) = properties
                .get_mut("properties")
                .and_then(|p| p.as_object_mut())
            {
                props.insert(
                    "agent".to_string(),
                    serde_json::json!({
                        "type": "string",
                        "enum": names,
                        "description": "Agent profile to run this subagent under (model + system prompt). Omit for the default subagent."
                    }),
                );
            }

            // If any profile opted its subagent into tools, tell the model so
            // it can pick a tooled profile for work that needs repo access.
            if SUBAGENT_ROUTES
                .get()
                .is_some_and(|m| m.values().any(|r| r.tool_allow.is_some()))
            {
                let has_write = SUBAGENT_ROUTES.get().is_some_and(|m| {
                    m.values().any(|r| {
                        r.tool_allow
                            .as_ref()
                            .is_some_and(|a| a.iter().any(|t| t == "write" || t == "edit"))
                    })
                });
                if has_write {
                    description.push_str(
                        " Some profiles enable a read-write tooled subagent (read, grep, glob, \
                         edit, write, bash — no recursion, no session-scoped tools) that can \
                         investigate AND edit the repo directly; pick such a profile for \
                         implementation subtasks. Others are read-only.",
                    );
                } else {
                    description.push_str(
                        " Some profiles enable a read-only tooled subagent (read, grep, glob, \
                         list_dir, etc. — no mutation, no recursion) that can investigate the \
                         repo directly; pick such a profile for research/exploration subtasks.",
                    );
                }
            }
        }

        ToolDefinition {
            name: "task".to_string(),
            description,
            parameters: properties,
        }
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "task", &args.prompt).await?;

        // dirge-ykeu Phase 4: resolve an optional agent profile to a model +
        // system-prompt override. An explicit `agent=` that can't be resolved
        // is an error (don't silently fall back to the default subagent —
        // the model asked for a specific persona). `tool_allow` is `Some`
        // when the profile opted its subagent into tools (v1: readonly tier);
        // that selects the tooled fork below instead of the tool-less one-shot.
        let (route_model, route_preamble, tool_allow, max_turns, timeout, tier, mcp) = match args
            .agent
            .as_deref()
        {
            None => (
                None,
                None,
                None,
                SUBAGENT_DEFAULT_MAX_TURNS,
                std::time::Duration::from_secs(600),
                crate::context::agent_defs::SubagentToolTier::Toolless,
                crate::context::agent_defs::SubagentMcpAccess::None,
            ),
            Some(name) => {
                if !subagent_routes_available() {
                    return Err(ToolError::Msg(format!(
                        "agent profile '{}' requested but no profiles are defined. Add .dirge/agents/<name>.md or a config.json \"agents\" entry, or omit `agent`.",
                        name
                    )));
                }
                match subagent_route(name) {
                    Some(r) => (
                        r.model,
                        r.preamble,
                        r.tool_allow,
                        r.max_turns,
                        r.timeout,
                        r.tier,
                        r.mcp,
                    ),
                    None => {
                        return Err(ToolError::Msg(format!(
                            "unknown agent profile '{}'. Available: {}.",
                            name,
                            subagent_route_names().join(", ")
                        )));
                    }
                }
            }
        };

        let background = args.background.unwrap_or(false);
        let retry_of = normalize_retry_of(args.retry_of);
        let is_writer = matches!(
            tier,
            crate::context::agent_defs::SubagentToolTier::ReadWrite
        );
        if retry_of.is_some() && !background {
            return Err(ToolError::Msg("retry_of requires background=true".into()));
        }
        if self.bg_store.coordinator_strategy().is_some() && !background && is_writer {
            return Err(ToolError::Msg(
                "coordinator writer subagents require background=true so writer isolation can be enforced"
                    .into(),
            ));
        }
        if self.bg_store.coordinator_strategy()
            == Some(crate::config::SubagentDispatchStrategy::Full)
            && !background
            && !matches!(tier, crate::context::agent_defs::SubagentToolTier::Toolless)
        {
            return Err(ToolError::Msg(
                "full coordinator mode requires tier-routed subagents to use background=true"
                    .into(),
            ));
        }

        // Tooled subagent (v1 readonly tier): fork a filtered runner off the
        // live agent. Everything below this branch is the unchanged tool-less
        // `btw_query` path, so the dirge-mifq regression stays green.
        if let Some(allowed) = tool_allow {
            return self
                .run_tooled(
                    route_model,
                    route_preamble,
                    allowed,
                    mcp,
                    max_turns,
                    timeout,
                    background,
                    is_writer,
                    retry_of,
                    args.prompt,
                    args.agent,
                )
                .await;
        }

        // The profile's model (when it pinned one) else the default subagent.
        let model = route_model.unwrap_or_else(|| self.model.clone());

        let background = args.background.unwrap_or(false);

        if background {
            // Audit M2: refuse new background spawns past the
            // concurrency cap. The agent gets a clear refusal it
            // can act on (wait for an existing task to finish, then
            // retry) rather than fanning out unbounded.
            let running = self.bg_store.running_count();
            let cap = BackgroundStore::max_concurrent();
            if running >= cap {
                return Err(ToolError::Msg(format!(
                    "background subagent cap reached ({}/{} in flight). Wait for one to finish (use task_status) or run inline (background=false). Capping prevents fan-out from burning the API budget.",
                    running, cap,
                )));
            }
            let task_id = Uuid::new_v4().to_string();
            if self.bg_store.coordinator_strategy().is_some() {
                self.bg_store
                    .insert_coordinator_dispatch(
                        task_id.clone(),
                        args.prompt.clone(),
                        is_writer,
                        false,
                        retry_of.as_deref(),
                    )
                    .map_err(ToolError::Msg)?;
            } else {
                self.bg_store.insert_for_dispatch(task_id.clone());
            }
            self.bg_store.notify_started(&task_id);

            // dirge-ov2 Phase D: announce the subagent so the UI
            // loop creates a chat window for it.
            self.emit_chat(SubagentChatEvent::Spawn {
                id: task_id.clone(),
                prompt: args.prompt.clone(),
                agent: args.agent.clone(),
            });

            // dirge-781c: per-subagent AbortSignal so `/kill <id>` or
            // Ctrl+K on the focused tab can interrupt it. Registered
            // in the process-global registry; cleared on terminal
            // event below.
            let abort = AbortSignal::new();
            register_subagent_abort(&task_id, abort.clone());

            let prompt = args.prompt;
            let store = self.bg_store.clone();
            let tid = task_id.clone();
            let chat_sink = self.chat_sink.clone();
            let abort_for_task = abort.clone();
            let preamble_for_task = route_preamble.clone();

            let route_timeout = timeout;
            let store_for_task = store.clone();
            let tid_for_task = tid.clone();
            let handle = tokio::spawn(async move {
                let _cleanup =
                    SubagentCleanup::from_store(tid_for_task.clone(), store_for_task.clone());
                let fut = model.btw_query_with(
                    format!(
                        "You are a subagent working on a specific subtask. Complete it thoroughly.\n\nTask: {}",
                        prompt
                    ),
                    preamble_for_task.as_deref(),
                );
                // dirge-781c: race btw_query against the abort signal.
                // `btw_query` is one-shot so we can't propagate the
                // signal into the provider; instead we poll the flag
                // and bail out of the await at the next tick.
                let abort_check = abort_for_task.clone();
                let raced = async {
                    tokio::pin!(fut);
                    loop {
                        tokio::select! {
                            r = &mut fut => break Ok::<_, ()>(r),
                            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                                if abort_check.is_cancelled() {
                                    break Err(());
                                }
                            }
                        }
                    }
                };
                let outer = tokio::time::timeout(route_timeout, raced).await;
                let (state, chat_event) = match outer {
                    Ok(Ok(Ok(text))) => (
                        TaskState::Completed(text.clone()),
                        SubagentChatEvent::Token {
                            id: tid_for_task.clone(),
                            text: text.clone(),
                        },
                    ),
                    Ok(Ok(Err(e))) => {
                        let msg = e.to_string();
                        (
                            TaskState::Failed(msg.clone()),
                            SubagentChatEvent::Failed {
                                id: tid_for_task.clone(),
                                error: msg,
                            },
                        )
                    }
                    Ok(Err(())) => {
                        // dirge-781c: aborted via /kill.
                        let msg = "aborted by user".to_string();
                        (
                            TaskState::Cancelled(msg.clone()),
                            SubagentChatEvent::Aborted {
                                id: tid_for_task.clone(),
                            },
                        )
                    }
                    Err(_) => {
                        let msg = format!("subagent timed out after {}s", route_timeout.as_secs(),);
                        (
                            TaskState::Failed(msg.clone()),
                            SubagentChatEvent::Failed {
                                id: tid_for_task.clone(),
                                error: msg,
                            },
                        )
                    }
                };
                // dirge-781c: emit the streaming Token first (if any),
                // then the terminal Complete. Lets the UI render the
                // payload through the same Token-handling code path
                // that a real per-token stream would use.
                let final_event = match &chat_event {
                    SubagentChatEvent::Token { id, text } => {
                        if let Some(sink) = &chat_sink {
                            let _ = sink.try_send(chat_event.clone());
                        } else if let Some(sink) = subagent_chat_sink() {
                            let _ = sink.try_send(chat_event.clone());
                        }
                        SubagentChatEvent::Complete {
                            id: id.clone(),
                            result: text.clone(),
                        }
                    }
                    _ => chat_event.clone(),
                };
                if let Some(sink) = chat_sink {
                    let _ = sink.try_send(final_event);
                } else if let Some(sink) = subagent_chat_sink() {
                    let _ = sink.try_send(final_event);
                }
                unregister_subagent_abort(&tid_for_task);
                store_for_task.notify(&tid_for_task, state);
            });
            // Register the handle so `BackgroundStore::cancel_all` (called
            // on session swap) can abort the subagent and free its
            // provider connection. Without this the task survived the
            // parent's session change and kept consuming API budget.
            store.attach_handle(&tid, handle);

            Ok(format!(
                "background task started — task_id: {}\n\nThe subagent runs in the background. Completion will be delivered automatically as a <system-reminder> at the start of your next turn. Do NOT poll task_status or sleep waiting — continue with other work.",
                task_id
            ))
        } else {
            // dirge-ov2 Phase D: foreground subagent. Emit Spawn /
            // Complete / Failed so the UI surfaces the call as a
            // chat window (Ctrl-N/P/X to view). Foreground tasks
            // still block the parent agent's tool call; the chat
            // window populates with prompt + final result.
            let task_id = Uuid::new_v4().to_string();
            self.emit_chat(SubagentChatEvent::Spawn {
                id: task_id.clone(),
                prompt: args.prompt.clone(),
                agent: args.agent.clone(),
            });
            // dirge-781c: register an AbortSignal so `/kill` / Ctrl+K
            // can interrupt the foreground subagent. Registered for
            // the duration of the btw_query call and removed on
            // every exit path.
            let abort = AbortSignal::new();
            register_subagent_abort(&task_id, abort.clone());

            let fut = model.btw_query_with(
                format!(
                    "You are a subagent working on a specific subtask. Complete it thoroughly.\n\nTask: {}",
                    args.prompt
                ),
                route_preamble.as_deref(),
            );
            let abort_check = abort.clone();
            let raced = async {
                tokio::pin!(fut);
                loop {
                    tokio::select! {
                        r = &mut fut => break Ok::<_, ()>(r),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                            if abort_check.is_cancelled() {
                                break Err(());
                            }
                        }
                    }
                }
            };
            let result = raced.await;
            unregister_subagent_abort(&task_id);
            match result {
                Ok(Ok(text)) => {
                    // dirge-nmv5: the chat window always gets the FULL
                    // text so the user sees the complete subagent
                    // answer in its Ctrl-N/P window. The parent agent
                    // sees the relayed text — verbatim when small,
                    // a head/tail summary plus a `read`-tool hint
                    // when large (full payload at
                    // `~/.dirge/transient/<pid>/task-<ts>.txt`).
                    // Replaces the prior "drop everything past 3000
                    // chars" behavior that silently lost subagent
                    // output on the background path.
                    self.emit_chat(SubagentChatEvent::Token {
                        id: task_id.clone(),
                        text: text.clone(),
                    });
                    self.emit_chat(SubagentChatEvent::Complete {
                        id: task_id,
                        result: text.clone(),
                    });
                    let outcome =
                        crate::agent::tools::output_relay::relay_if_large("task", text, "");
                    Ok(outcome.text)
                }
                Ok(Err(e)) => {
                    let msg = e.to_string();
                    self.emit_chat(SubagentChatEvent::Failed {
                        id: task_id,
                        error: msg.clone(),
                    });
                    Err(ToolError::Msg(format!("Subagent error: {}", msg)))
                }
                Err(()) => {
                    // dirge-781c: aborted via /kill or Ctrl+K. The
                    // parent agent sees an `aborted` error so the
                    // cancellation is visible in its loop, NOT a
                    // silent "subagent finished" with no payload.
                    self.emit_chat(SubagentChatEvent::Aborted { id: task_id });
                    Err(ToolError::Msg("Subagent aborted by user".to_string()))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::background::BackgroundStore;
    use crate::provider::AnyModel;
    use rig::client::CompletionClient;
    use rig::providers::openrouter;

    #[test]
    fn blank_retry_id_is_treated_as_absent() {
        assert_eq!(normalize_retry_of(Some("".to_string())), None);
        assert_eq!(normalize_retry_of(Some("   ".to_string())), None);
        assert_eq!(
            normalize_retry_of(Some(" task-id ".to_string())),
            Some("task-id".to_string())
        );
    }

    fn mock_tool() -> TaskTool {
        // The model is never invoked in these tests — they exercise the
        // definition surface only.
        let client = openrouter::Client::builder()
            .api_key("test-key")
            .http_client(crate::provider::compressing_http::CompressingHttpClient::default())
            .build()
            .unwrap();
        let model = client.completion_model("anthropic/claude-sonnet-4.5");
        TaskTool::new(
            None,
            None,
            AnyModel::OpenRouter(model),
            BackgroundStore::new(),
            Sandbox::new(crate::sandbox::SandboxMode::Off),
            SubagentWriteIsolation::Serialize,
        )
    }

    #[test]
    fn isolated_writer_preamble_requires_commit_and_report() {
        let preamble = writer_preamble(
            None,
            std::path::Path::new("/tmp/dirge-task-writer"),
            "dirge-task-writer",
        );
        assert!(preamble.contains("stage intended changes"));
        assert!(preamble.contains("one descriptive commit"));
        assert!(preamble.contains("commit hash"));
        assert!(preamble.contains("/tmp/dirge-task-writer"));
    }

    #[test]
    fn writer_worktree_policy_respects_mode_and_sandbox() {
        // Non-writer: always false regardless of isolation/sandbox.
        assert_eq!(
            writer_worktree_enabled(false, SubagentWriteIsolation::Worktree, false, false),
            Ok(false)
        );
        // Serialize: always false.
        assert_eq!(
            writer_worktree_enabled(true, SubagentWriteIsolation::Serialize, false, false),
            Ok(false)
        );
        // Writer + Auto + confining sandbox → worktree allowed.
        assert_eq!(
            writer_worktree_enabled(true, SubagentWriteIsolation::Auto, false, true),
            Ok(true)
        );
        // Writer + Auto + no confining sandbox (Off) → worktree disabled,
        // fall back to serialized parent (safe when parent is clean).
        assert_eq!(
            writer_worktree_enabled(true, SubagentWriteIsolation::Auto, false, false),
            Ok(false)
        );
        // Writer + Worktree + no confining sandbox → error (false isolation).
        assert!(
            writer_worktree_enabled(true, SubagentWriteIsolation::Worktree, false, false).is_err()
        );
        // Writer + Worktree + confining sandbox → allowed.
        assert_eq!(
            writer_worktree_enabled(true, SubagentWriteIsolation::Worktree, false, true),
            Ok(true)
        );
        // MicroVM: Auto → false, Worktree → error (unchanged).
        assert_eq!(
            writer_worktree_enabled(true, SubagentWriteIsolation::Auto, true, true),
            Ok(false)
        );
        assert!(
            writer_worktree_enabled(true, SubagentWriteIsolation::Worktree, true, true).is_err()
        );
    }

    #[test]
    fn model_provider_name_follows_model_variant() {
        use rig::client::CompletionClient;

        let openai = rig::providers::openai::CompletionsClient::builder()
            .http_client(crate::provider::compressing_http::CompressingHttpClient::default())
            .api_key("test-key")
            .build()
            .unwrap()
            .completion_model("gpt-test");
        let anthropic = rig::providers::anthropic::Client::builder()
            .api_key("test-key")
            .http_client(crate::provider::compressing_http::CompressingHttpClient::default())
            .build()
            .unwrap()
            .completion_model("claude-test");

        assert_eq!(AnyModel::OpenAI(openai).provider_name(), "openai");
        assert_eq!(AnyModel::Anthropic(anthropic).provider_name(), "anthropic");
    }

    #[tokio::test]
    async fn cancel_all_cleans_tooled_subagent_registry_and_watcher() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let _guard = registry_test_lock().await;
        clear_abort_registry_for_test();
        let id = "cancel-all-tooled";
        register_subagent_abort(id, AbortSignal::new());

        struct MarkDropped(Arc<AtomicBool>);
        impl Drop for MarkDropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let watcher_dropped = Arc::new(AtomicBool::new(false));
        let dropped = watcher_dropped.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let outer = tokio::spawn(async move {
            let watcher = tokio::spawn(async move {
                let _mark = MarkDropped(dropped);
                std::future::pending::<()>().await;
            });
            let _cleanup = SubagentCleanup::new(id.to_string(), watcher);
            let _ = ready_tx.send(());
            std::future::pending::<()>().await;
        });

        ready_rx.await.unwrap();
        let store = BackgroundStore::new();
        store.insert(id.to_string());
        store.attach_handle(id, outer);
        store.cancel_all();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while registered_subagent_ids()
                .iter()
                .any(|task_id| task_id == id)
                || !watcher_dropped.load(Ordering::SeqCst)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancel_all must drop cleanup and abort watcher");
    }

    #[tokio::test]
    async fn abort_watcher_can_be_stopped_after_runner_completion() {
        let runner_task = tokio::spawn(std::future::pending::<()>());
        let (cancel_tx, _cancel_rx) = tokio::sync::mpsc::channel(1);
        let watcher =
            spawn_abort_watcher(AbortSignal::new(), runner_task.abort_handle(), cancel_tx);

        watcher.abort();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), watcher)
            .await
            .expect("aborted watcher must terminate");
        assert!(result.unwrap_err().is_cancelled());
        runner_task.abort();
    }

    // Regression: the task tool description must tell the agent that
    // background=true delivers completion automatically and instruct it
    // NOT to poll task_status. The previous text told the agent to "use
    // task_status to poll for the result", which produced wasteful loops.
    #[tokio::test]
    async fn definition_steers_agent_away_from_polling() {
        let tool = mock_tool();
        let def = tool.definition(String::new()).await;
        let desc = def.description.to_lowercase();
        assert!(
            desc.contains("system-reminder") || desc.contains("automatically"),
            "task description must reference automatic notification: {}",
            def.description
        );
        assert!(
            desc.contains("do not poll") || desc.contains("not poll"),
            "task description must explicitly discourage polling: {}",
            def.description
        );
    }

    /// dirge-mifq — The DEFAULT subagent path (no `agent=`, or a profile that
    /// doesn't opt into tools) goes through `AnyModel::btw_query`, which builds
    /// a fresh rig agent from the model alone with NO tools attached. This pins
    /// that invariant: the one-shot `btw_query_with` must stay tool-less.
    ///
    /// The TOOLED subagent path (readonly OR readwrite tier, opt-in via a
    /// profile's `subagent.tools`) is NOT covered by the `btw_query` assertion
    /// below — instead it closes the dirge-mifq leakage class by construction:
    /// `resolve_subagent_allow` intersects the allow-list with the tier's
    /// universe and strips `SUBAGENT_FORCED_EXCLUDES`
    /// (session_search/memory/skill/issue/graph/todo/... + recursion + interactive),
    /// and the tooled fork runs under a FRESH child session id. So a tooled
    /// subagent literally cannot reach a session-attributing tool — and this
    /// holds for BOTH tiers: a readwrite subagent can edit the repo, but it
    /// still can't write durable agent state or attribute to a session. This
    /// test asserts that for both tiers below.
    ///
    /// `TaskTool` itself still holds no tool registry / session_id / agent
    /// handle — the tooled fork reaches the live agent through the
    /// process-global `provider::current_agent()`, not a captured field.
    #[test]
    fn subagent_path_is_stateless_no_session_search_leakage() {
        // The fields a TaskTool legitimately holds today. Anything
        // beyond this set is a red flag for subagent-tool leakage.
        let _expected_fields = ["permission", "ask_tx", "model", "bg_store", "chat_sink"];

        // Construct a TaskTool — if a future field is required,
        // this won't compile until the new field is provided.
        let _tool: TaskTool = mock_tool();

        // The tool-less path lives in provider/dispatch.rs. The build
        // inside `btw_query_with` (which `btw_query` delegates to) is
        // `AgentBuilder::new(m).preamble(...).build()` with no `.tool(...)`
        // calls — verify by source inspection that none has crept in.
        let provider_src = include_str!("../../provider/dispatch.rs");
        let btw_idx = provider_src
            .find("pub async fn btw_query_with")
            .expect("btw_query_with must exist in provider/dispatch.rs");
        let btw_end = provider_src[btw_idx..]
            .find("\n    }\n")
            .map(|i| btw_idx + i)
            .unwrap_or(provider_src.len());
        let btw_body = &provider_src[btw_idx..btw_end];
        assert!(
            !btw_body.contains(".tool("),
            "btw_query_with must not attach tools to the subagent — that would \
             require auditing session_id propagation per dirge-mifq. \
             Source snippet:\n{btw_body}"
        );
        assert!(
            !btw_body.contains(".tools("),
            "btw_query_with must not attach tools to the subagent — that would \
             require auditing session_id propagation per dirge-mifq."
        );

        // Tooled path: the resolved allow-list must never contain a session-
        // attributing / recursive / interactive tool, even if a profile tried
        // to `allow` one. This is the dirge-mifq gate for the tooled shape —
        // and it MUST hold for BOTH tiers (readonly AND readwrite). The forced
        // floor is tier-independent by design.
        let leaky = [
            "session_search",
            "memory",
            "skill",
            "issue",
            "graph",
            "write_todo_list",
            "spec",
            "task",
            "task_status",
            "question",
            "plan_enter",
            "plan_exit",
        ];
        use crate::context::agent_defs::{SubagentToolPolicy, SubagentToolTier};
        for tier in [SubagentToolTier::Readonly, SubagentToolTier::ReadWrite] {
            let policy = SubagentToolPolicy {
                tier: tier.clone(),
                allow: leaky.iter().map(|s| s.to_string()).collect(), // try to smuggle
                deny: Vec::new(),
                max_turns: None,
                timeout_secs: None,
                mcp: crate::context::agent_defs::SubagentMcpAccess::None,
            };
            let resolved = resolve_subagent_allow(&policy)
                .unwrap_or_else(|| panic!("{tier:?} should yield a tool set"));
            for l in leaky {
                assert!(
                    !resolved.iter().any(|t| t == l),
                    "{tier:?} subagent allow-list must exclude {l:?} (dirge-mifq); got {resolved:?}"
                );
            }
        }
    }

    // --- resolve_subagent_allow: the readonly allow-list contract (v1) ---

    #[test]
    fn resolve_readonly_base_is_expected_universe() {
        use crate::context::agent_defs::{SubagentToolPolicy, SubagentToolTier};
        let p = SubagentToolPolicy {
            tier: SubagentToolTier::Readonly,
            ..Default::default()
        };
        let mut got = resolve_subagent_allow(&p).unwrap();
        got.sort();
        let mut want: Vec<String> = SUBAGENT_READONLY_BASE
            .iter()
            .map(|s| s.to_string())
            .collect();
        want.sort();
        assert_eq!(got, want, "readonly base must match the verified universe");
    }

    #[test]
    fn non_empty_allow_narrows_readonly_base() {
        use crate::context::agent_defs::{SubagentToolPolicy, SubagentToolTier};
        let p = SubagentToolPolicy {
            tier: SubagentToolTier::Readonly,
            allow: vec!["read".into(), "grep".into()],
            ..Default::default()
        };
        assert_eq!(
            resolve_subagent_allow(&p).unwrap(),
            vec!["grep".to_string(), "read".to_string()]
        );
    }

    #[test]
    fn resolve_toolless_returns_none() {
        use crate::context::agent_defs::{SubagentToolPolicy, SubagentToolTier};
        let p = SubagentToolPolicy {
            tier: SubagentToolTier::Toolless,
            ..Default::default()
        };
        assert_eq!(resolve_subagent_allow(&p), None);
    }

    #[test]
    fn resolve_readwrite_includes_writes_and_excludes_leaky() {
        use crate::context::agent_defs::{SubagentToolPolicy, SubagentToolTier};
        // readwrite = readonly universe + the write/bash family. A readwrite
        // subagent can edit the code tree and run builds/tests directly.
        let p = SubagentToolPolicy {
            tier: SubagentToolTier::ReadWrite,
            ..Default::default()
        };
        let got = resolve_subagent_allow(&p).expect("readwrite yields a tool set");
        // write/bash family present
        for w in ["write", "edit", "edit_lines", "apply_patch", "bash"] {
            assert!(
                got.iter().any(|t| t == w),
                "readwrite must include {w}: {got:?}"
            );
        }
        // readonly base still present
        assert!(got.iter().any(|t| t == "read"));
        assert!(got.iter().any(|t| t == "grep"));
        // leaky tools STILL excluded regardless of tier (dirge-mifq) — readwrite
        // can edit the repo, not write durable agent state or attribute to a
        // session.
        for l in [
            "session_search",
            "memory",
            "skill",
            "issue",
            "graph",
            "write_todo_list",
            "spec",
            "task",
            "task_status",
            "question",
        ] {
            assert!(
                !got.iter().any(|t| t == l),
                "readwrite must still exclude {l:?} (dirge-mifq): {got:?}"
            );
        }
    }

    #[test]
    fn deny_narrows_readwrite_base() {
        use crate::context::agent_defs::{SubagentToolPolicy, SubagentToolTier};
        let p = SubagentToolPolicy {
            tier: SubagentToolTier::ReadWrite,
            deny: vec!["bash".into(), "edit".into()],
            ..Default::default()
        };
        let got = resolve_subagent_allow(&p).unwrap();
        assert!(!got.iter().any(|t| t == "bash"));
        assert!(!got.iter().any(|t| t == "edit"));
        // other write tools remain
        assert!(got.iter().any(|t| t == "write"));
        assert!(got.iter().any(|t| t == "apply_patch"));
    }

    #[test]
    fn deny_narrows_readonly_base() {
        use crate::context::agent_defs::{SubagentToolPolicy, SubagentToolTier};
        let p = SubagentToolPolicy {
            tier: SubagentToolTier::Readonly,
            deny: vec!["webfetch".into(), "grep".into()],
            ..Default::default()
        };
        let got = resolve_subagent_allow(&p).unwrap();
        assert!(!got.iter().any(|t| t == "webfetch"));
        assert!(!got.iter().any(|t| t == "grep"));
        // untouched base tools remain
        assert!(got.iter().any(|t| t == "read"));
        assert!(got.iter().any(|t| t == "glob"));
    }

    #[test]
    fn allow_cannot_escalate_past_readonly() {
        use crate::context::agent_defs::{SubagentToolPolicy, SubagentToolTier};
        // Asking for mutating tools on a readonly profile must be a no-op.
        let p = SubagentToolPolicy {
            tier: SubagentToolTier::Readonly,
            allow: vec!["bash".into(), "edit".into(), "write".into()],
            ..Default::default()
        };
        let got = resolve_subagent_allow(&p).unwrap();
        assert!(
            got.is_empty(),
            "invalid-only allow must narrow to no tools: {got:?}"
        );
    }

    #[test]
    fn resolve_mcp_selection_matches_issue_701() {
        use crate::context::agent_defs::SubagentMcpAccess;
        use std::collections::HashSet;
        let available: HashSet<String> = ["search_graph", "find_refs", "web_lookup"]
            .into_iter()
            .map(String::from)
            .collect();

        // None → nothing.
        assert!(resolve_mcp_selection(&SubagentMcpAccess::None, &available).is_empty());

        // All → every available MCP tool.
        let mut all = resolve_mcp_selection(&SubagentMcpAccess::All, &available);
        all.sort();
        assert_eq!(all, vec!["find_refs", "search_graph", "web_lookup"]);

        // Only → just the named tools, case-insensitively; unknown names dropped.
        let only = resolve_mcp_selection(
            &SubagentMcpAccess::Only(vec!["Search_Graph".into(), "nonexistent".into()]),
            &available,
        );
        assert_eq!(only, vec!["search_graph".to_string()]);

        // A built-in name in the Only list can NEVER be granted — it isn't in
        // the available MCP set, so the tier cap on built-ins stays honest.
        let escalate = resolve_mcp_selection(
            &SubagentMcpAccess::Only(vec!["bash".into(), "edit".into()]),
            &available,
        );
        assert!(
            escalate.is_empty(),
            "subagent_mcp must never grant a non-MCP (built-in) tool: {escalate:?}"
        );

        // All against an empty set (no MCP servers connected) → nothing.
        assert!(resolve_mcp_selection(&SubagentMcpAccess::All, &HashSet::new()).is_empty());
    }

    #[test]
    fn resolve_max_turns_honors_override_and_default() {
        use crate::context::agent_defs::{SubagentToolPolicy, SubagentToolTier};
        let none = SubagentToolPolicy {
            tier: SubagentToolTier::Readonly,
            ..Default::default()
        };
        assert_eq!(
            resolve_subagent_max_turns(&none),
            SUBAGENT_DEFAULT_MAX_TURNS
        );
        let over = SubagentToolPolicy {
            tier: SubagentToolTier::Readonly,
            max_turns: Some(7),
            ..Default::default()
        };
        assert_eq!(resolve_subagent_max_turns(&over), 7);
    }

    #[test]
    fn one_line_and_summarize_helpers_truncate() {
        assert_eq!(one_line("hello   world", 100), "hello world");
        let long = "a ".repeat(200);
        assert!(one_line(&long, 50).chars().count() <= 51); // 50 + ellipsis
        // Object-arg summary is iteration-order independent (serde_json Map
        // ordering is feature-dependent); assert on the set of key=val tokens.
        let args = serde_json::json!({"path": "/tmp/x", "n": 3});
        let got = summarize_json_args(&args, 120);
        let tokens: Vec<&str> = got.split(' ').collect();
        assert!(tokens.contains(&"path=/tmp/x"), "got: {got}");
        assert!(tokens.contains(&"n=3"), "got: {got}");
    }

    /// dirge-781c: `drain_subagent_runner` is the first live producer for the
    /// dormant `SubagentChatEvent::{Token,Reasoning,ToolCall,ToolResult}`
    /// variants. Feed a replay runner a Token → Reasoning → ToolCall →
    /// ToolResult → Done stream and assert each maps to the right chat event
    /// in order, with the final Done text returned. Mirrors the
    /// `runner_replaying` pattern in `plan::runtime::tests`.
    #[tokio::test]
    async fn drain_buffers_tokens_and_emits_response_once() {
        use crate::agent::runner::AgentRunner;
        use crate::event::{AgentEvent, ToolContent};
        use std::sync::{Arc, Mutex};
        use tokio::sync::mpsc;

        fn replay(events: Vec<AgentEvent>) -> AgentRunner {
            let (tx, event_rx) = mpsc::channel(events.len().max(1));
            for e in events {
                tx.try_send(e).expect("test channel sized to fit events");
            }
            drop(tx); // close → drain loop terminates at channel end
            let (interject_tx, _) = mpsc::channel(1);
            let (cancel_tx, _) = mpsc::channel(1);
            let task = tokio::spawn(async {});
            AgentRunner {
                event_rx,
                task,
                interject_tx,
                cancel_tx,
            }
        }

        let runner = replay(vec![
            AgentEvent::Token("hello ".into()),
            AgentEvent::Token("world".into()),
            AgentEvent::Reasoning("thinking".into()),
            AgentEvent::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                args: serde_json::json!({"path": "/tmp/x"}),
            },
            AgentEvent::ToolResult {
                id: "c1".into(),
                output: "file contents".into(),
                kind: ToolContent::Text,
            },
            AgentEvent::Done {
                response: "hello world".into(),
                tokens: 5,
                cost: 0.0,
            },
        ]);

        let collected: Arc<Mutex<Vec<SubagentChatEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = collected.clone();
        let text = drain_subagent_runner(runner, "sub-1", move |ev| {
            sink.lock().unwrap().push(ev);
        })
        .await
        .expect("drain returns the Done response text");

        assert_eq!(text, "hello world");
        let events = collected.lock().unwrap();
        use SubagentChatEvent::*;
        assert!(matches!(
            &events[0],
            Reasoning { id, text } if id == "sub-1" && text == "thinking"
        ));
        assert!(matches!(
            &events[1],
            ToolCall { id, tool_name, args_summary }
                if id == "sub-1" && tool_name == "read" && args_summary.contains("path=/tmp/x")
        ));
        assert!(matches!(
            &events[2],
            ToolResult { id, output_summary, .. }
                if id == "sub-1" && output_summary.contains("file contents")
        ));
        assert!(matches!(
            &events[3],
            Token { id, text } if id == "sub-1" && text == "hello world"
        ));
        assert!(matches!(
            &events[4],
            Complete { id, result } if id == "sub-1" && result == "hello world"
        ));
        assert_eq!(
            events.len(),
            5,
            "success must emit one buffered response then Complete: {events:?}"
        );
    }

    /// Replay helper shared by the truncation/fallback drain tests below.
    fn replay_runner(events: Vec<crate::event::AgentEvent>) -> crate::agent::runner::AgentRunner {
        use tokio::sync::mpsc;
        let (tx, event_rx) = mpsc::channel(events.len().max(1));
        for e in events {
            tx.try_send(e).expect("test channel sized to fit events");
        }
        drop(tx);
        let (interject_tx, _) = mpsc::channel(1);
        let (cancel_tx, _) = mpsc::channel(1);
        crate::agent::runner::AgentRunner {
            event_rx,
            task: tokio::spawn(async {}),
            interject_tx,
            cancel_tx,
        }
    }

    // dirge-cnx3: a subagent killed by the turn cap must NOT hand the parent a
    // bare narration string that reads as a finished answer. The loop emits the
    // max-turns notice as `AgentEvent::SystemNotice` and then an `AgentEnd`
    // whose last ASSISTANT message is the in-flight tool-call turn — so
    // `Done.response` is the preamble the model wrote before calling tools.
    // The drain must fold the notice into the payload.
    #[tokio::test]
    async fn drain_marks_payload_when_subagent_hits_turn_cap() {
        use crate::event::AgentEvent;
        let runner = replay_runner(vec![
            AgentEvent::Token("Now let me check the next file".into()),
            AgentEvent::SystemNotice {
                content: "[dirge] Max agent turns (25) reached. Stopping the run.".into(),
            },
            AgentEvent::Done {
                response: "Now let me check the next file".into(),
                tokens: 0,
                cost: 0.0,
            },
        ]);
        let text = drain_subagent_runner(runner, "sub-1", |_| {})
            .await
            .expect("drain returns the payload");
        assert!(
            text.starts_with(SUBAGENT_TRUNCATED_MARKER),
            "cut-off payload must lead with the truncation marker: {text:?}",
        );
        assert!(
            text.contains("Max agent turns (25) reached"),
            "the notice itself must survive so the parent knows the cap: {text:?}",
        );
        assert!(
            text.contains("Now let me check the next file"),
            "the agent's own trailing text must still be delivered: {text:?}",
        );
    }

    // An unrelated SystemNotice (or none at all) must leave a clean answer
    // untouched — the marker is for the turn-cap case only.
    #[tokio::test]
    async fn drain_leaves_clean_answer_unmarked() {
        use crate::event::AgentEvent;
        let runner = replay_runner(vec![
            AgentEvent::SystemNotice {
                content: "[dirge] compacted context".into(),
            },
            AgentEvent::Done {
                response: "## Findings\n1. real finding".into(),
                tokens: 0,
                cost: 0.0,
            },
        ]);
        let text = drain_subagent_runner(runner, "sub-1", |_| {})
            .await
            .expect("drain returns the payload");
        assert_eq!(
            text, "## Findings\n1. real finding",
            "a finished subagent's answer must round-trip verbatim",
        );
    }

    // dirge-vxmi: an empty `Done.response` falls back to the accumulated token
    // stream — every turn's narration, not an answer. Deliver it labelled so
    // the parent doesn't reconcile a trace as a findings report.
    #[tokio::test]
    async fn drain_labels_token_trace_fallback() {
        use crate::event::AgentEvent;
        let runner = replay_runner(vec![
            AgentEvent::Token("Let me look at the config.".into()),
            AgentEvent::Token("Now the handler.".into()),
            AgentEvent::Done {
                response: String::new().into(),
                tokens: 0,
                cost: 0.0,
            },
        ]);
        let text = drain_subagent_runner(runner, "sub-1", |_| {})
            .await
            .expect("drain returns the payload");
        assert!(
            text.starts_with(SUBAGENT_TRACE_FALLBACK_MARKER),
            "trace fallback must be labelled, not passed off as an answer: {text:?}",
        );
        assert!(
            text.contains("Let me look at the config.") && text.contains("Now the handler."),
            "the accumulated trace must still be delivered: {text:?}",
        );
    }

    // Both conditions at once (cap reached AND no final text): the parent gets
    // the turn-cap marker, which is the more actionable of the two.
    #[tokio::test]
    async fn drain_prefers_turn_cap_marker_over_trace_marker() {
        use crate::event::AgentEvent;
        let runner = replay_runner(vec![
            AgentEvent::Token("mid-work narration".into()),
            AgentEvent::SystemNotice {
                content: "[dirge] Max agent turns (25) reached. Stopping the run.".into(),
            },
            AgentEvent::Done {
                response: String::new().into(),
                tokens: 0,
                cost: 0.0,
            },
        ]);
        let text = drain_subagent_runner(runner, "sub-1", |_| {})
            .await
            .expect("drain returns the payload");
        assert!(text.starts_with(SUBAGENT_TRUNCATED_MARKER), "{text:?}");
        assert!(
            !text.contains(SUBAGENT_TRACE_FALLBACK_MARKER),
            "one marker, not two: {text:?}",
        );
        assert!(text.contains("mid-work narration"), "{text:?}");
    }

    // dirge-ykeu Phase 4: with a routing table installed, the `task` tool
    // advertises an `agent` enum and resolves/rejects profile names. NOTE:
    // SUBAGENT_ROUTES is a set-once OnceLock, so this is the only test in the
    // binary that installs it; other tests' assertions are substring/key
    // checks robust to the extra `agent` property it adds.
    #[tokio::test]
    async fn subagent_routing_advertises_and_validates() {
        let mut routes = HashMap::new();
        routes.insert(
            "reviewer".to_string(),
            SubagentRoute {
                model: None,
                preamble: Some("You are a reviewer.".to_string()),
                tool_allow: None,
                max_turns: SUBAGENT_DEFAULT_MAX_TURNS,
                timeout: std::time::Duration::from_secs(600),
                tier: crate::context::agent_defs::SubagentToolTier::Toolless,
                mcp: crate::context::agent_defs::SubagentMcpAccess::None,
            },
        );
        set_subagent_routes(routes);

        assert!(subagent_routes_available());
        assert_eq!(subagent_route_names(), vec!["reviewer".to_string()]);
        assert!(subagent_route("reviewer").is_some());
        assert!(subagent_route("ghost").is_none());

        let tool = mock_tool();
        let def = tool.definition(String::new()).await;
        assert!(
            def.description.contains("Available profiles: reviewer"),
            "definition must list installed profiles: {}",
            def.description
        );
        let props = def
            .parameters
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("properties present");
        let agent_prop = props.get("agent").expect("agent property advertised");
        assert_eq!(agent_prop["enum"][0], "reviewer");
    }

    #[tokio::test]
    async fn definition_advertises_background_field() {
        let tool = mock_tool();
        let def = tool.definition(String::new()).await;
        let props = def
            .parameters
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("properties present");
        assert!(props.contains_key("background"));
        let bg_desc = props["background"]["description"]
            .as_str()
            .unwrap()
            .to_lowercase();
        assert!(bg_desc.contains("automatically") || bg_desc.contains("system-reminder"));
        assert!(bg_desc.contains("do not poll") || bg_desc.contains("not poll"));
    }

    // dirge-nmv5: short subagent answers (under the 8 KiB / 200-line
    // budget) must be returned verbatim to the parent agent — no
    // summary, no relay file, no truncation. The relay is keyed on
    // the "task" tool name so this exercises exactly the same path
    // `TaskTool::call` runs.
    #[test]
    fn task_short_output_returned_verbatim() {
        let short = "subagent: 42 is the answer.\n".to_string();
        let outcome = crate::agent::tools::output_relay::relay_if_large("task", short.clone(), "");
        assert!(
            outcome.relayed_to.is_none(),
            "short output must not trigger the disk relay",
        );
        assert_eq!(
            outcome.text, short,
            "short subagent output must round-trip unchanged to the parent",
        );
    }

    // dirge-nmv5: large subagent answers must NOT silently truncate.
    // The full text is written to `~/.dirge/transient/<pid>/task-<ts>.txt`
    // and the parent agent receives a head/tail summary plus a
    // `read`-tool hint pointing at the transient file. This guards
    // against regressing to the prior "drop everything past 3000
    // chars" behavior that lost subagent output.
    #[test]
    fn task_large_output_relayed_to_disk_with_summary() {
        let _relay_dir = crate::agent::tools::output_relay::TransientDirGuard::new();
        // 64 KiB payload — well past the default 8 KiB inline budget.
        let huge: String = "subagent line\n".repeat(5_000);
        let original_len = huge.len();
        let outcome = crate::agent::tools::output_relay::relay_if_large("task", huge, "");

        // Full payload landed on disk and is readable.
        let path = outcome
            .relayed_to
            .as_ref()
            .expect("large output must trigger the disk relay");
        assert!(path.exists(), "relayed file must exist at {path:?}");
        let written = std::fs::read_to_string(path).expect("read relayed file");
        assert_eq!(
            written.len(),
            original_len,
            "the FULL original payload must be on disk (not the truncated head)",
        );

        // Parent agent gets a much-smaller summary plus the recovery
        // hint — no silent truncation.
        let summary = &outcome.text;
        assert!(
            summary.len() < original_len,
            "summary should be much smaller than the original payload",
        );
        assert!(
            summary.contains("`read`"),
            "summary must mention the `read` tool so the agent can recover the full payload: {summary}",
        );
        assert!(
            summary.contains(
                &crate::agent::tools::output_relay::transient_base()
                    .display()
                    .to_string()
            ),
            "summary must reference the transient path: {summary}",
        );

        // Cleanup.
        let _ = std::fs::remove_file(path);
    }

    // dirge-781c: registry-backed kill resolution. These tests use a
    // serial mutex to ensure they don't trample each other's
    // registry state when run in parallel (cargo's default).
    async fn registry_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    /// `/kill` against an empty registry or a never-spawned prefix
    /// must be a NoOp — never panic, never cancel anything.
    #[tokio::test]
    async fn kill_unknown_id_no_op() {
        let _guard = registry_test_lock().await;
        clear_abort_registry_for_test();
        // Empty registry, any prefix → NotFound.
        assert_eq!(kill_subagent("abc"), KillOutcome::NotFound);
        assert_eq!(kill_subagent(""), KillOutcome::NotFound);
        // Populated registry, prefix doesn't match anything.
        let sig = AbortSignal::new();
        register_subagent_abort("aaa-1111", sig.clone());
        assert_eq!(kill_subagent("zzz"), KillOutcome::NotFound);
        assert!(
            !sig.is_cancelled(),
            "unmatched kill must NOT cancel the surviving subagent",
        );
        unregister_subagent_abort("aaa-1111");
    }

    /// `/kill <prefix>` with exactly one matching id resolves to
    /// `Killed(full_id)` and fires the abort signal.
    #[tokio::test]
    async fn kill_resolves_by_prefix_unique_match() {
        let _guard = registry_test_lock().await;
        clear_abort_registry_for_test();
        let sig_a = AbortSignal::new();
        let sig_b = AbortSignal::new();
        register_subagent_abort("aa11-deadbeef", sig_a.clone());
        register_subagent_abort("bb22-cafef00d", sig_b.clone());

        // Unique 2-char prefix → kill exactly that one.
        match kill_subagent("aa") {
            KillOutcome::Killed(id) => assert_eq!(id, "aa11-deadbeef"),
            other => panic!("expected Killed; got {:?}", other),
        }
        assert!(sig_a.is_cancelled(), "matched signal must be cancelled");
        assert!(!sig_b.is_cancelled(), "unmatched signal must survive");

        // Ambiguous prefix (registering a second `aa…` id) → Ambiguous.
        let sig_a2 = AbortSignal::new();
        register_subagent_abort("aa99-othertask", sig_a2.clone());
        // sig_a already cancelled from previous step; check ambiguity
        // returns BOTH matching ids.
        match kill_subagent("aa") {
            KillOutcome::Ambiguous(ids) => {
                assert_eq!(ids.len(), 2);
                assert!(ids.iter().any(|i| i == "aa11-deadbeef"));
                assert!(ids.iter().any(|i| i == "aa99-othertask"));
            }
            other => panic!("expected Ambiguous; got {:?}", other),
        }
        assert!(
            !sig_a2.is_cancelled(),
            "ambiguous kill must NOT cancel any signal",
        );

        // Exact-id match wins over prefix collision: passing the
        // FULL id of one entry kills exactly that one even though
        // it's a prefix of itself.
        clear_abort_registry_for_test();
        let s1 = AbortSignal::new();
        let s2 = AbortSignal::new();
        register_subagent_abort("abc", s1.clone());
        register_subagent_abort("abcdef", s2.clone());
        match kill_subagent("abc") {
            KillOutcome::Killed(id) => assert_eq!(id, "abc"),
            other => panic!("expected exact-match Killed; got {:?}", other),
        }
        assert!(s1.is_cancelled());
        assert!(!s2.is_cancelled());

        clear_abort_registry_for_test();
    }

    /// `subagent_complete_after_kill_returns_aborted_result`: when
    /// `/kill` fires while the subagent's `btw_query` future is
    /// awaiting, the task tool emits an `Aborted` chat event and
    /// returns a `ToolError` containing "aborted" so the parent
    /// agent's tool-result block reflects the cancellation.
    ///
    /// The test exercises the racer directly because `btw_query`
    /// requires a real provider. The racer is the same code path
    /// the production `call()` runs.
    #[tokio::test]
    async fn subagent_complete_after_kill_returns_aborted_result() {
        let _guard = registry_test_lock().await;
        clear_abort_registry_for_test();
        let tid = "t-abort-1";
        let abort = AbortSignal::new();
        register_subagent_abort(tid, abort.clone());

        // Simulate a long-running btw_query future that never
        // returns. The select! racer polls the abort signal every
        // 100ms; cancelling here should make it bail out within ~200ms.
        let abort_check = abort.clone();
        let fut = async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok::<String, anyhow::Error>("never-arrives".to_string())
        };
        let raced = async {
            tokio::pin!(fut);
            loop {
                tokio::select! {
                    r = &mut fut => break Ok::<_, ()>(r),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                        if abort_check.is_cancelled() {
                            break Err(());
                        }
                    }
                }
            }
        };

        // Fire /kill in parallel; the racer should observe it on
        // its next 50ms poll.
        let killer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(75)).await;
            assert!(matches!(kill_subagent("t-abort"), KillOutcome::Killed(_)));
        });

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), raced)
            .await
            .expect("racer must exit before the 2s test timeout");
        killer.await.unwrap();

        match result {
            Err(()) => { /* expected — aborted */ }
            Ok(other) => panic!("expected abort; got Ok({:?})", other),
        }
        unregister_subagent_abort(tid);
        clear_abort_registry_for_test();
    }

    /// dirge-781c: Token / Reasoning / ToolCall / ToolResult /
    /// Aborted events round-trip through the chat sink — the UI
    /// receiver sees them with the same id payload the producer
    /// sent. Guards the variant additions against accidental
    /// dirge-02tn: the chat channel is BOUNDED — once full, `try_send`
    /// drops (returns Err) rather than growing memory without bound.
    /// Events are display-only, so a dropped event under a UI stall only
    /// degrades the live view, never the subagent's result.
    #[test]
    fn subagent_chat_channel_is_bounded_and_drops_on_overflow() {
        let (tx, _rx) = mpsc::channel::<SubagentChatEvent>(SUBAGENT_CHAT_CAP);
        // Fill to capacity without draining (_rx kept alive so the channel
        // stays open — otherwise try_send would fail Closed, not Full).
        for i in 0..SUBAGENT_CHAT_CAP {
            tx.try_send(SubagentChatEvent::Token {
                id: "x".into(),
                text: format!("{i}"),
            })
            .expect("sends within capacity succeed");
        }
        let overflow = tx.try_send(SubagentChatEvent::Token {
            id: "x".into(),
            text: "overflow".into(),
        });
        assert!(
            overflow.is_err(),
            "channel must be bounded — an over-capacity try_send drops"
        );
    }

    /// silent drops when the dispatch is refactored.
    #[test]
    fn subagent_token_event_routes_to_chat_slot() {
        let (tx, mut rx) = mpsc::channel::<SubagentChatEvent>(SUBAGENT_CHAT_CAP);
        tx.try_send(SubagentChatEvent::Token {
            id: "a1".into(),
            text: "hello world".into(),
        })
        .unwrap();
        tx.try_send(SubagentChatEvent::Reasoning {
            id: "a1".into(),
            text: "thinking".into(),
        })
        .unwrap();
        tx.try_send(SubagentChatEvent::Aborted { id: "a1".into() })
            .unwrap();

        match rx.try_recv().unwrap() {
            SubagentChatEvent::Token { id, text } => {
                assert_eq!(id, "a1");
                assert_eq!(text, "hello world");
            }
            other => panic!("expected Token; got {:?}", other),
        }
        match rx.try_recv().unwrap() {
            SubagentChatEvent::Reasoning { id, text } => {
                assert_eq!(id, "a1");
                assert_eq!(text, "thinking");
            }
            other => panic!("expected Reasoning; got {:?}", other),
        }
        match rx.try_recv().unwrap() {
            SubagentChatEvent::Aborted { id } => assert_eq!(id, "a1"),
            other => panic!("expected Aborted; got {:?}", other),
        }
    }

    #[test]
    fn subagent_tool_call_event_routes_to_chat_slot() {
        let (tx, mut rx) = mpsc::channel::<SubagentChatEvent>(SUBAGENT_CHAT_CAP);
        tx.try_send(SubagentChatEvent::ToolCall {
            id: "a1".into(),
            tool_name: "read".into(),
            args_summary: "path=/tmp/x".into(),
        })
        .unwrap();
        tx.try_send(SubagentChatEvent::ToolResult {
            id: "a1".into(),
            tool_name: "read".into(),
            output_summary: "12 lines".into(),
        })
        .unwrap();

        match rx.try_recv().unwrap() {
            SubagentChatEvent::ToolCall {
                id,
                tool_name,
                args_summary,
            } => {
                assert_eq!(id, "a1");
                assert_eq!(tool_name, "read");
                assert_eq!(args_summary, "path=/tmp/x");
            }
            other => panic!("expected ToolCall; got {:?}", other),
        }
        match rx.try_recv().unwrap() {
            SubagentChatEvent::ToolResult {
                id,
                tool_name,
                output_summary,
            } => {
                assert_eq!(id, "a1");
                assert_eq!(tool_name, "read");
                assert_eq!(output_summary, "12 lines");
            }
            other => panic!("expected ToolResult; got {:?}", other),
        }
    }
}
