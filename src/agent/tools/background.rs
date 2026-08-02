#[allow(unused_imports)]
use crate::config::SubagentDispatchStrategy;
use crate::sync_util::LockExt;
use indexmap::IndexMap;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// dirge-nmv5: legacy hard cap retained for the `Failed` error
/// path only. Error strings are typically a single-line provider
/// error or "subagent timed out" message — relaying them to disk
/// would be wasteful. Capping goes through `tools::head_cap`
/// (byte-bounded, UTF-8-safe, marker appended — no longer a silent
/// chop; dirge-06cp). Completed payloads no longer hit this cap: they
/// go through the disk-backed `output_relay` instead, so the full
/// subagent answer is recoverable via the `read` tool even when the
/// inline summary elides the middle.
const MAX_TASK_OUTPUT_CHARS: usize = 3000;

/// Maximum number of tasks retained in the store. When a new task is
/// inserted past this cap, the oldest task by insertion order is evicted
/// (FIFO — `get` does not bump access order). Plenty of headroom for any
/// reasonable session; agents only see ids they themselves spawned.
const STORE_CAPACITY: usize = 32;

/// Maximum number of *concurrently running* background subagent
/// tasks (audit M2). Without this cap a misbehaving LLM could spawn
/// dozens of background tasks in parallel and burn the user's API
/// budget. Hit by tracking the in-flight JoinHandle count via
/// `running_count()`; the `task` tool refuses new background spawns
/// when at-cap with a clear error rather than queueing.
const MAX_CONCURRENT_SUBAGENTS: usize = 4;

/// Event surfaced on the UI lifecycle channel.
///
/// `Started` fires when the parent spawns a background task; `Finished` fires
/// when the subagent terminates (with the same TaskNotification later drained
/// for the LLM-side reminder). The UI renders these as colored lines in the
/// human's scrollback so the user can follow background work as it happens.
#[derive(Debug, Clone)]
pub enum LifecycleEvent {
    Started { id: String },
    Finished(TaskNotification),
}

/// Sender half of the UI lifecycle channel.
pub type LifecycleSender = mpsc::UnboundedSender<LifecycleEvent>;
pub type LifecycleReceiver = mpsc::UnboundedReceiver<LifecycleEvent>;

/// Thread-safe store for background subagent tasks.
///
/// Tasks persist after completion so the parent agent can look them up by id
/// via the `task_status` tool. Completion events are queued separately for
/// push-style delivery (see [`drain_notifications`]); the agent does not need
/// to poll.
#[derive(Debug, Clone, Default)]
pub struct BackgroundStore {
    inner: Arc<Mutex<Inner>>,
    /// Optional UI lifecycle sink. Cloned from the constructor; notify() will
    /// best-effort send into it. Drops silently if the receiver is gone.
    ui_sink: Option<LifecycleSender>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Tasks keyed by id. Insertion order preserved so the oldest entry can
    /// be evicted when at capacity. Drain does not remove tasks from here;
    /// they remain looked-up-able by `task_status` until LRU eviction.
    tasks: IndexMap<String, BackgroundTask>,
    /// Pre-snapshotted notifications ready for delivery. FIFO. We carry the
    /// full TaskNotification (not just the id) so eviction between notify
    /// and drain can't lose the payload.
    pending: VecDeque<TaskNotification>,
    /// JoinHandle per in-flight subagent task, keyed by task id. Populated
    /// by `attach_handle` after the spawning code in `task.rs` has the
    /// handle, removed on terminal notify, and aborted en-masse by
    /// `cancel_all` when the parent session is swapped out (e.g. plugin
    /// `harness/switch-session` or the `/sessions <id>` slash). Without
    /// this, subagents continued to consume API budget after their parent
    /// was gone, eventually notifying a dropped store.
    handles: HashMap<String, JoinHandle<()>>,
    #[cfg(feature = "git-worktree")]
    writer_worktrees: HashMap<String, crate::extras::git_worktree::WorktreeInfo>,
    coordinator: Option<CoordinatorState>,
}

#[derive(Debug)]
struct CoordinatorState {
    strategy: SubagentDispatchStrategy,
    next_generation: u64,
    active_task_ids: Vec<String>,
    profiles: crate::agent::tools::task::CoordinatorProfiles,
    dispatches: IndexMap<String, CoordinatorDispatch>,
}

#[derive(Debug, Clone)]
pub struct CoordinatorDispatchInfo {
    pub prompt: String,
    pub is_writer: bool,
    pub is_isolated_writer: bool,
    pub retry_of: Option<String>,
    pub retried_by: Option<String>,
    pub worktree_branch: Option<String>,
    pub worktree_path: Option<String>,
    pub worktree_commits: Vec<String>,
    pub worktree_dirty: Option<bool>,
    pub worktree_retained: Option<bool>,
}

#[derive(Debug)]
struct CoordinatorDispatch {
    prompt: String,
    is_writer: bool,
    is_isolated_writer: bool,
    retry_of: Option<String>,
    retried_by: Option<String>,
    worktree_branch: Option<String>,
    worktree_path: Option<String>,
    worktree_commits: Vec<String>,
    worktree_dirty: Option<bool>,
    worktree_retained: Option<bool>,
}

/// A writer worktree that was left on disk because it contains
/// uncommitted or dirty work. Surfaced by `cancel_all` so the user
/// can find their salvaged work after a session swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedWorktree {
    pub branch: Option<String>,
    pub path: Option<String>,
}

impl CoordinatorState {
    fn new(
        strategy: SubagentDispatchStrategy,
        profiles: crate::agent::tools::task::CoordinatorProfiles,
    ) -> Self {
        Self {
            strategy,
            next_generation: 1,
            active_task_ids: Vec::new(),
            profiles,
            dispatches: IndexMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TaskState {
    Running,
    Completed(String),
    Failed(String),
    Cancelled(String),
}

#[derive(Debug, Clone)]
pub struct BackgroundTask {
    pub state: TaskState,
}

/// A completion event ready to be surfaced to the parent agent at its next
/// turn boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskNotification {
    pub id: String,
    pub state: TaskState,
}

impl BackgroundStore {
    /// Construct a store with no UI sink. Mostly used by tests; production
    /// code goes through [`with_ui_sink`] so the UI gets lifecycle events.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a store wired to a UI lifecycle sink. Each notify() call
    /// also pushes the resulting TaskNotification into `ui_sink` so the UI
    /// can render the completion line immediately.
    pub fn with_ui_sink(ui_sink: LifecycleSender) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            ui_sink: Some(ui_sink),
        }
    }

    /// Insert a new task in Running state. If the store is at capacity, the
    /// oldest task is evicted to make room. Inserting an existing id replaces
    /// it in place without evicting anything.
    pub fn insert(&self, id: String) {
        let mut inner = self.lock();
        if !inner.tasks.contains_key(&id) && inner.tasks.len() >= STORE_CAPACITY {
            // Evict the oldest by insertion order. shift_remove preserves order
            // of the remaining entries.
            inner.tasks.shift_remove_index(0);
        }
        inner.tasks.insert(
            id,
            BackgroundTask {
                state: TaskState::Running,
            },
        );
    }

    #[cfg(test)]
    pub fn enable_coordinator(&self, strategy: SubagentDispatchStrategy) {
        self.enable_coordinator_with_profiles(strategy, Default::default());
    }

    pub fn enable_coordinator_with_profiles(
        &self,
        strategy: SubagentDispatchStrategy,
        profiles: crate::agent::tools::task::CoordinatorProfiles,
    ) {
        if strategy == SubagentDispatchStrategy::Off {
            return;
        }
        self.lock().coordinator = Some(CoordinatorState::new(strategy, profiles));
    }

    pub fn coordinator_strategy(&self) -> Option<SubagentDispatchStrategy> {
        self.lock().coordinator.as_ref().map(|state| state.strategy)
    }

    pub fn coordinator_preamble(&self) -> Option<String> {
        let inner = self.lock();
        let coordinator = inner.coordinator.as_ref()?;
        let mode = match coordinator.strategy {
            SubagentDispatchStrategy::Optional => {
                "Optional mode permits direct trivial work; coordinated work must follow this contract."
            }
            SubagentDispatchStrategy::Full => {
                "Full mode requires substantive tier-routed work to use background dispatch."
            }
            SubagentDispatchStrategy::Off => return None,
        };
        let profiles = |items: &[crate::agent::tools::task::CoordinatorProfile]| {
            items
                .iter()
                .map(|profile| match &profile.description {
                    Some(description) => format!("- {} — {}", profile.name, description),
                    None => format!("- {}", profile.name),
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Some(format!(
            "Coordinator dispatch is enabled. {mode}\n\nRead-only subagent profiles:\n{}\n\nRead-write subagent profiles:\n{}\n\nDispatch at most four background subagents per wave. Reconcile completed work, failures, remaining requirements, required verification, and the next action before dispatching more work. Do not poll task_status for partial output. A failed coordinator task may be retried once with retry_of=<task-id>; then repair on the main thread. Isolated writers may run concurrently in their worktrees; shared-checkout writers are serialized. Writers must not auto-merge: report their branch, commits, tests, and salvage path for the coordinator to reconcile. Keep internal reconciliation summaries private and provide the user only a final summary. Do not combine this strategy with plugins/orchestrator.janet or plugins/delegate.janet.",
            profiles(&coordinator.profiles.readonly),
            profiles(&coordinator.profiles.readwrite),
        ))
    }

    pub fn task_is_awaiting_coordinator_delivery(&self, id: &str) -> Option<u64> {
        let inner = self.lock();
        let coordinator = inner.coordinator.as_ref()?;
        coordinator
            .active_task_ids
            .iter()
            .any(|task_id| task_id == id)
            .then_some(coordinator.next_generation)
    }

    pub fn coordinator_dispatch(&self, id: &str) -> Option<CoordinatorDispatchInfo> {
        let inner = self.lock();
        let dispatch = inner.coordinator.as_ref()?.dispatches.get(id)?;
        Some(CoordinatorDispatchInfo {
            prompt: dispatch.prompt.clone(),
            is_writer: dispatch.is_writer,
            is_isolated_writer: dispatch.is_isolated_writer,
            retry_of: dispatch.retry_of.clone(),
            retried_by: dispatch.retried_by.clone(),
            worktree_branch: dispatch.worktree_branch.clone(),
            worktree_path: dispatch.worktree_path.clone(),
            worktree_commits: dispatch.worktree_commits.clone(),
            worktree_dirty: dispatch.worktree_dirty,
            worktree_retained: dispatch.worktree_retained,
        })
    }

    pub fn set_coordinator_dispatch_isolated(
        &self,
        id: &str,
        is_isolated_writer: bool,
    ) -> Result<(), String> {
        let mut inner = self.lock();
        let coordinator = inner
            .coordinator
            .as_mut()
            .ok_or_else(|| "coordinator mode is not enabled".to_string())?;
        if !is_isolated_writer
            && coordinator
                .active_task_ids
                .iter()
                .filter(|task_id| task_id.as_str() != id)
                .any(|task_id| {
                    coordinator
                        .dispatches
                        .get(task_id)
                        .is_some_and(|dispatch| dispatch.is_writer && !dispatch.is_isolated_writer)
                })
        {
            return Err("a serialized writer is already active; wait for batch reconciliation before dispatching another writer".into());
        }
        let dispatch = coordinator
            .dispatches
            .get_mut(id)
            .ok_or_else(|| format!("task {id} is not a coordinator dispatch"))?;
        dispatch.is_isolated_writer = is_isolated_writer;
        Ok(())
    }

    pub fn set_coordinator_dispatch_worktree(
        &self,
        id: &str,
        branch: String,
        worktree_path: String,
    ) -> Result<(), String> {
        let mut inner = self.lock();
        let dispatch = inner
            .coordinator
            .as_mut()
            .and_then(|coordinator| coordinator.dispatches.get_mut(id))
            .ok_or_else(|| format!("task {id} is not a coordinator dispatch"))?;
        dispatch.worktree_branch = Some(branch);
        dispatch.worktree_path = Some(worktree_path);
        Ok(())
    }

    #[cfg(feature = "git-worktree")]
    pub fn register_writer_worktree(
        &self,
        id: String,
        info: crate::extras::git_worktree::WorktreeInfo,
    ) {
        self.lock().writer_worktrees.insert(id, info);
    }

    #[cfg(feature = "git-worktree")]
    pub fn unregister_writer_worktree(&self, id: &str) {
        self.lock().writer_worktrees.remove(id);
    }

    pub fn set_coordinator_dispatch_worktree_outcome(
        &self,
        id: &str,
        commits: Vec<String>,
        dirty: bool,
        retained: bool,
    ) {
        let mut inner = self.lock();
        let Some(dispatch) = inner
            .coordinator
            .as_mut()
            .and_then(|coordinator| coordinator.dispatches.get_mut(id))
        else {
            return;
        };
        dispatch.worktree_commits = commits;
        dispatch.worktree_dirty = Some(dirty);
        dispatch.worktree_retained = Some(retained);
    }

    pub fn insert_for_dispatch(&self, id: String) {
        if self.coordinator_strategy().is_some() {
            self.insert_coordinated(id);
        } else {
            self.insert(id);
        }
    }

    pub fn insert_coordinator_dispatch(
        &self,
        id: String,
        prompt: String,
        is_writer: bool,
        is_isolated_writer: bool,
        retry_of: Option<&str>,
    ) -> Result<(), String> {
        let mut inner = self.lock();
        if inner.coordinator.is_none() {
            return Err("coordinator mode is not enabled".into());
        }

        if is_writer
            && !is_isolated_writer
            && inner.coordinator.as_ref().is_some_and(|coordinator| {
                coordinator.active_task_ids.iter().any(|task_id| {
                    coordinator
                        .dispatches
                        .get(task_id)
                        .is_some_and(|dispatch| dispatch.is_writer && !dispatch.is_isolated_writer)
                })
            })
        {
            return Err(
                "a serialized writer is already active; wait for batch reconciliation before dispatching another writer"
                    .into(),
            );
        }

        if let Some(original_id) = retry_of {
            let failed = matches!(
                inner.tasks.get(original_id).map(|task| &task.state),
                Some(TaskState::Failed(_))
            );
            if !failed {
                return Err(format!(
                    "task {original_id} must have failed before it can be retried"
                ));
            }
            let original = inner
                .coordinator
                .as_mut()
                .and_then(|coordinator| coordinator.dispatches.get_mut(original_id))
                .ok_or_else(|| {
                    format!(
                        "task {original_id} is not a coordinator dispatch and cannot be retried"
                    )
                })?;
            if original.retried_by.is_some() {
                return Err(format!(
                    "task {original_id} has already used its coordinator retry. Reconcile and repair the work on the main thread."
                ));
            }
            original.retried_by = Some(id.clone());
        }

        if !inner.tasks.contains_key(&id) && inner.tasks.len() >= STORE_CAPACITY {
            inner.tasks.shift_remove_index(0);
        }
        inner.tasks.insert(
            id.clone(),
            BackgroundTask {
                state: TaskState::Running,
            },
        );
        let coordinator = inner.coordinator.as_mut().expect("checked above");
        coordinator.active_task_ids.push(id.clone());
        coordinator.dispatches.insert(
            id,
            CoordinatorDispatch {
                prompt,
                is_writer,
                is_isolated_writer,
                retry_of: retry_of.map(str::to_string),
                retried_by: None,
                worktree_branch: None,
                worktree_path: None,
                worktree_commits: Vec::new(),
                worktree_dirty: None,
                worktree_retained: None,
            },
        );
        Ok(())
    }

    pub fn insert_coordinated(&self, id: String) {
        let mut inner = self.lock();
        if !inner.tasks.contains_key(&id) && inner.tasks.len() >= STORE_CAPACITY {
            inner.tasks.shift_remove_index(0);
        }
        inner.tasks.insert(
            id.clone(),
            BackgroundTask {
                state: TaskState::Running,
            },
        );
        if let Some(coordinator) = &mut inner.coordinator {
            coordinator.active_task_ids.push(id);
        }
    }

    /// Look up the current state of a task without mutating the store.
    pub fn get(&self, id: &str) -> Option<BackgroundTask> {
        self.lock().tasks.get(id).cloned()
    }

    /// Count of subagent tasks currently in flight. Equal to the number
    /// of live `JoinHandle`s the store is tracking. Used by the `task`
    /// tool to refuse a new background spawn at the `MAX_CONCURRENT_SUBAGENTS`
    /// cap, and by the status bar's `agents:N` segment.
    pub fn running_count(&self) -> usize {
        self.lock().handles.len()
    }

    /// Compile-time cap on concurrent subagent spawns.
    pub fn max_concurrent() -> usize {
        MAX_CONCURRENT_SUBAGENTS
    }

    /// Record a terminal state (Completed or Failed) and queue a notification
    /// for delivery. Truncates the payload to MAX_TASK_OUTPUT_CHARS.
    ///
    /// No-op if the id has been evicted from the store. Calling notify with
    /// `TaskState::Running` is also a no-op — Running is the initial state
    /// set by `insert` and not a terminal transition.
    pub fn notify(&self, id: &str, state: TaskState) {
        if matches!(state, TaskState::Running) {
            return;
        }
        let truncated = truncate_state(state);
        let id_owned = id.to_string();
        let mut inner = self.lock();
        let Some(task) = inner.tasks.get_mut(id) else {
            return;
        };
        task.state = truncated.clone();
        // Task has reached a terminal state — drop its JoinHandle so
        // we're not keeping a finished handle alive in the map. Handle
        // drop is fine even if the task itself already exited.
        inner.handles.remove(id);
        // Guard against double-notifies enqueuing the same id twice.
        if !inner.pending.iter().any(|n| n.id == id_owned) {
            inner.pending.push_back(TaskNotification {
                id: id_owned.clone(),
                state: truncated.clone(),
            });
        }
        // Drop the lock before signalling the UI to avoid holding it across
        // an await/send and to keep the receiver's wake free of contention.
        drop(inner);
        if let Some(sink) = &self.ui_sink {
            // Best-effort: receiver may already be gone (UI shut down).
            let _ = sink.send(LifecycleEvent::Finished(TaskNotification {
                id: id_owned,
                state: truncated,
            }));
        }
    }

    /// Notify the task to a terminal state ONLY if it is currently
    /// `Running`. If the task already reached a terminal state
    /// (via explicit `notify`), this call is a no-op — it won't
    /// clobber the real result or re-enqueue a duplicate notification.
    /// Used by drop guards to catch panics / early-returns in spawned
    /// closures without overwriting a successful completion.
    pub fn notify_if_running(&self, id: &str, state: TaskState) {
        if matches!(state, TaskState::Running) {
            // Refuse to "transition" Running → Running.
            return;
        }
        let inner = self.lock();
        let task_is_running = inner
            .tasks
            .get(id)
            .is_some_and(|t| matches!(t.state, TaskState::Running));
        if !task_is_running {
            return;
        }
        // Delegate to normal notify — it already handles truncation,
        // handle removal, pending queue, and UI event. Since we've
        // verified the state is still Running, this is the only path
        // that will fire.
        drop(inner);
        self.notify(id, state);
    }

    /// Attach the `JoinHandle` of a freshly-spawned background subagent
    /// task to its id. Called immediately after the spawn in
    /// `task.rs` so `cancel_all` has something to abort on session
    /// switch. Only attaches when the task is still `Running` — if the
    /// spawned closure already finished (called `notify` from another
    /// thread) the state is terminal and the handle is dropped.
    /// Re-attaching for a still-running id replaces and drops the
    /// previous handle.
    pub fn attach_handle(&self, id: &str, handle: JoinHandle<()>) {
        let mut inner = self.lock();
        // Only keep the handle if the task is still tracked AND still
        // Running. On a multi-thread runtime the spawned closure can
        // race ahead and finish (notify) before we get here — the task
        // entry lives in `tasks` even post-notify (for task_status
        // lookup), so `contains_key` alone isn't enough.
        let is_running = inner
            .tasks
            .get(id)
            .is_some_and(|t| matches!(t.state, TaskState::Running));
        if !is_running {
            return;
        }
        if let Some(prev) = inner.handles.insert(id.to_string(), handle) {
            // Defensive: dropping the old handle without abort is OK
            // (it would continue running) but a session-switch could
            // then leak it. Abort the old one explicitly.
            prev.abort();
        }
    }

    /// Abort every in-flight background subagent task and mark any
    /// still-Running task as Failed("cancelled — session switched").
    /// Called from the UI's session-swap paths (plugin TreeOp
    /// `NewSession` / `SwitchSession`, `/sessions <prefix>` slash)
    /// so subagents stop burning API budget against a session their
    /// parent agent no longer sees. Drained `pending` notifications
    /// are also cleared — they belong to the previous session and
    /// would otherwise surface in the new session's first turn.
    ///
    /// Returns a list of writer worktrees that were *retained* on disk
    /// (dirty or have uncommitted work) so the caller can surface their
    /// paths to the user — otherwise the coordinator metadata would be
    /// lost and the user would have orphaned `dirge-task-*` worktrees.
    pub fn cancel_all(&self) -> Vec<RetainedWorktree> {
        let mut inner = self.lock();
        // Abort handles. `abort()` is best-effort: the awaiter inside
        // the task (e.g. `model.btw_query`) gets dropped at the next
        // suspension point, which collapses its reqwest connection.
        for (_, h) in inner.handles.drain() {
            h.abort();
        }
        // Mark any task still in Running state as cancelled so a
        // later `task_status` lookup returns something useful instead
        // of "Running forever".
        let cancelled_label = "cancelled — session switched".to_string();
        for task in inner.tasks.values_mut() {
            if matches!(task.state, TaskState::Running) {
                task.state = TaskState::Cancelled(cancelled_label.clone());
            }
        }
        // Drop pending notifications. They belong to the previous
        // session; surfacing them in the next session's prompt would
        // be confusing ("you finished a task you didn't start").
        inner.pending.clear();
        #[cfg(feature = "git-worktree")]
        for (_, info) in std::mem::take(&mut inner.writer_worktrees) {
            let _ = crate::extras::git_worktree::remove_worktree_if_clean(&info);
        }
        // Collect retained writer worktrees BEFORE dropping the
        // coordinator so we can tell the user where their salvaged
        // work lives. remove_worktree_if_clean leaves dirty/committed
        // worktrees on disk — without this metadata the user gets
        // orphaned `dirge-task-*` dirs with no report.
        let retained = if let Some(coordinator) = &inner.coordinator {
            let retained: Vec<RetainedWorktree> = coordinator
                .dispatches
                .values()
                .filter(|d| d.is_writer && d.worktree_retained == Some(true))
                .map(|d| RetainedWorktree {
                    branch: d.worktree_branch.clone(),
                    path: d.worktree_path.clone(),
                })
                .collect();
            for r in &retained {
                tracing::warn!(
                    target: "dirge::subagent",
                    branch = ?r.branch,
                    path = ?r.path,
                    "retained writer worktree after cancel — uncommitted work preserved on disk",
                );
            }
            retained
        } else {
            Vec::new()
        };
        inner.coordinator = None;
        retained
    }

    /// Fire a Started lifecycle event for the UI. No effect on the LLM-side
    /// pending queue — "task started" is conveyed via the tool result already.
    /// Best-effort if the UI receiver is gone.
    pub fn notify_started(&self, id: &str) {
        if let Some(sink) = &self.ui_sink {
            let _ = sink.send(LifecycleEvent::Started { id: id.to_string() });
        }
    }

    /// Take all queued notifications and clear the queue. Each notification is
    /// delivered exactly once; subsequent calls return only notifications
    /// arriving after the previous drain.
    ///
    /// The payload is the state captured at notify time, so subsequent task
    /// eviction does not affect what the agent receives.
    pub fn drain_notifications(&self) -> Vec<TaskNotification> {
        let mut inner = self.lock();
        let Some(coordinator) = inner.coordinator.as_ref() else {
            return inner.pending.drain(..).collect();
        };
        let active_task_ids = coordinator.active_task_ids.clone();
        if active_task_ids.is_empty() {
            return inner.pending.drain(..).collect();
        }
        let terminal = active_task_ids.iter().all(|id| {
            !matches!(
                inner.tasks.get(id).map(|task| &task.state),
                Some(TaskState::Running)
            )
        });
        if !terminal {
            let tracked: std::collections::HashSet<_> =
                active_task_ids.iter().map(String::as_str).collect();
            let mut deliverable = Vec::new();
            let mut held = VecDeque::new();
            while let Some(notification) = inner.pending.pop_front() {
                if tracked.contains(notification.id.as_str()) {
                    held.push_back(notification);
                } else {
                    deliverable.push(notification);
                }
            }
            inner.pending = held;
            return deliverable;
        }

        let coordinator = inner.coordinator.as_mut().expect("checked above");
        let tracked_ids = std::mem::take(&mut coordinator.active_task_ids);
        coordinator.next_generation += 1;
        let tracked: std::collections::HashSet<_> =
            tracked_ids.iter().map(String::as_str).collect();
        let mut retained = VecDeque::new();
        while let Some(notification) = inner.pending.pop_front() {
            if !tracked.contains(notification.id.as_str()) {
                retained.push_back(notification);
            }
        }
        inner.pending = retained;
        tracked_ids
            .into_iter()
            .map(|id| TaskNotification {
                state: inner
                    .tasks
                    .get(&id)
                    .map(|task| task.state.clone())
                    .unwrap_or_else(|| {
                        TaskState::Failed("coordinator task was evicted before delivery".into())
                    }),
                id,
            })
            .collect()
    }

    /// dirge-9xo: peek whether there's at least one pending
    /// notification without draining. Used by the UI auto-resume
    /// path to decide whether a subagent completion should kick
    /// the parent agent into a new turn.
    pub fn has_pending_notifications(&self) -> bool {
        let inner = self.lock();
        let Some(coordinator) = &inner.coordinator else {
            return !inner.pending.is_empty();
        };
        if coordinator.active_task_ids.is_empty() {
            return !inner.pending.is_empty();
        }
        let tracked: std::collections::HashSet<_> = coordinator
            .active_task_ids
            .iter()
            .map(String::as_str)
            .collect();
        let batch_terminal = coordinator.active_task_ids.iter().all(|id| {
            !matches!(
                inner.tasks.get(id).map(|task| &task.state),
                Some(TaskState::Running)
            )
        });
        batch_terminal
            || inner
                .pending
                .iter()
                .any(|n| !tracked.contains(n.id.as_str()))
    }

    pub fn coordinator_generation_running(&self) -> bool {
        let inner = self.lock();
        let Some(coordinator) = &inner.coordinator else {
            return false;
        };
        !coordinator.active_task_ids.is_empty()
            && coordinator.active_task_ids.iter().any(|id| {
                matches!(
                    inner.tasks.get(id).map(|task| &task.state),
                    Some(TaskState::Running)
                )
            })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock_ignore_poison()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().tasks.len()
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.lock().pending.len()
    }
}

fn format_notifications(store: &BackgroundStore, notifications: &[TaskNotification]) -> String {
    let coordinated = store.coordinator_strategy().is_some();
    let mut body = String::with_capacity(256);
    if coordinated {
        body.push_str("Coordinator batch is complete. Reconcile completed work, failures, remaining requirements, required verification, and the next action before dispatching more work.\n\n");
    } else {
        body.push_str("The following background tasks finished since your last turn:\n\n");
    }
    for (i, notification) in notifications.iter().enumerate() {
        if i > 0 {
            body.push('\n');
        }
        if coordinated {
            if let Some(dispatch) = store.coordinator_dispatch(&notification.id) {
                let tier = if dispatch.is_writer {
                    "readwrite"
                } else {
                    "readonly"
                };
                body.push_str(&format!(
                    "[task {} ({tier})] prompt: {}\n",
                    notification.id, dispatch.prompt
                ));
                if let Some(retry_of) = dispatch.retry_of {
                    body.push_str(&format!("retry of: {retry_of}\n"));
                }
                if let Some(retried_by) = dispatch.retried_by {
                    body.push_str(&format!("retry budget: exhausted by {retried_by}\n"));
                } else if matches!(notification.state, TaskState::Failed(_)) {
                    body.push_str("retry budget: 1/1 available\n");
                }
                if dispatch.is_writer {
                    body.push_str(&format!(
                        "writer isolation: {}\n",
                        if dispatch.is_isolated_writer {
                            "worktree"
                        } else {
                            "serialized parent checkout"
                        }
                    ));
                }
                if let Some(branch) = dispatch.worktree_branch {
                    body.push_str(&format!("writer branch: {branch}\n"));
                }
                if let Some(dirty) = dispatch.worktree_dirty {
                    body.push_str(&format!(
                        "writer worktree: {}\n",
                        if dirty {
                            "dirty; retained for salvage"
                        } else {
                            "clean"
                        }
                    ));
                }
                if !dispatch.worktree_commits.is_empty() {
                    body.push_str(&format!(
                        "writer commits: {}\n",
                        dispatch.worktree_commits.join(", ")
                    ));
                }
                if let Some(path) = dispatch.worktree_path {
                    body.push_str(&format!("writer salvage path: {path}\n"));
                }
                if dispatch.worktree_retained == Some(false) {
                    body.push_str("writer worktree removed (clean with no commits)\n");
                }
            }
            match &notification.state {
                TaskState::Completed(text) => body.push_str(&format!("completed: {text}\n")),
                TaskState::Failed(error) => body.push_str(&format!("failed: {error}\n")),
                TaskState::Cancelled(reason) => body.push_str(&format!("cancelled: {reason}\n")),
                TaskState::Running => {}
            }
        } else {
            match &notification.state {
                TaskState::Completed(text) => {
                    body.push_str(&format!("[task {}] completed: {text}\n", notification.id))
                }
                TaskState::Failed(error) => {
                    body.push_str(&format!("[task {}] failed: {error}\n", notification.id))
                }
                TaskState::Cancelled(reason) => {
                    body.push_str(&format!("[task {}] cancelled: {reason}\n", notification.id))
                }
                TaskState::Running => {}
            }
        }
    }
    body
}

/// dirge-9tfq: build a `GetFollowupMessagesFn` bound to this store.
///
/// At the outer-loop boundary (inner loop has no more tool calls AND no
/// pending steering), the agent_loop polls this hook. If background
/// subagents have completed since the last poll, the hook drains the
/// pending notifications and returns a synthetic `LoopMessage::User`
/// containing a `<system-reminder>` block — one per completed task,
/// formatted exactly like `prepend_pending_notifications`.
///
/// The returned message tells the model to consider the result and
/// decide whether to act on it. Because this fires at the outer-loop
/// boundary, the loop will re-enter the inner loop with the
/// notification as `pending_messages` and the model sees the
/// completion on its next turn — even if the user never types again.
///
/// Empty store / no pending → empty `Vec`, outer loop exits naturally.
pub fn followup_from_background_store(
    store: BackgroundStore,
) -> crate::agent::agent_loop::hooks::GetFollowupMessagesFn {
    use crate::agent::agent_loop::message::{LoopMessage, UserMessage};
    Arc::new(move || {
        let store = store.clone();
        Box::pin(async move {
            let drained = store.drain_notifications();
            if drained.is_empty() {
                return Vec::new();
            }
            vec![LoopMessage::User(UserMessage::text(format!(
                "<system-reminder>\n{}</system-reminder>",
                format_notifications(&store, &drained)
            )))]
        })
    })
}

/// Format pending notifications as a `<system-reminder>` block prepended to
/// the next user prompt. Returns the prompt unchanged when there's nothing
/// pending or no store is provided.
///
/// Drains the queue so each notification is delivered exactly once. The
/// underlying tasks remain in the store and remain looked-up-able by
/// `task_status` until LRU eviction.
///
/// # The `<system-reminder>` convention
///
/// This is dirge's canonical out-of-band injection format. Anthropic models
/// and most modern frontier LLMs recognise the wrapping XML-ish tags as
/// "out of the user's voice — harness instructions or environmental updates."
/// Any future feature that needs to inject context into a user turn from the
/// harness side (todo nudges, post-tool hooks, environment changes, etc.)
/// should use the same `<system-reminder>...</system-reminder>` wrapper.
/// Inventing variant tags (`<reminder>`, `<system-note>`, `[REMINDER]`, ...)
/// would dilute the signal.
pub(crate) fn prepend_pending_notifications(
    prompt: &str,
    store: Option<&BackgroundStore>,
) -> String {
    let Some(store) = store else {
        return prompt.to_string();
    };
    let drained = store.drain_notifications();
    if drained.is_empty() {
        return prompt.to_string();
    }

    format!(
        "<system-reminder>\n{}</system-reminder>\n\n{prompt}",
        format_notifications(store, &drained)
    )
}

/// dirge-nmv5: relay large `Completed` payloads through the
/// disk-backed `output_relay`. Below the inline byte/line budget
/// the payload is returned verbatim; above it the full text is
/// written to `~/.dirge/transient/<pid>/task-<unix_ts>.txt` and a
/// head/tail summary (with a `read`-tool hint pointing at the
/// transient file) is stored on the task state instead. Failed
/// error strings still hit the legacy `MAX_TASK_OUTPUT_CHARS` cap
/// because provider error messages don't benefit from disk relay.
///
/// Replaces the prior behavior of silently chopping the tail of
/// large `Completed` payloads at 3000 chars, which lost
/// information without telling the agent it could recover it.
fn truncate_state(state: TaskState) -> TaskState {
    match state {
        TaskState::Completed(text) => {
            let outcome = crate::agent::tools::output_relay::relay_if_large("task", text, "");
            TaskState::Completed(outcome.text)
        }
        TaskState::Failed(err) => {
            // Was a silent `chars().take` chop; now caps with a marker
            // so the agent knows the error was truncated (dirge-06cp).
            TaskState::Failed(crate::agent::tools::head_cap(
                err,
                MAX_TASK_OUTPUT_CHARS,
                "task error",
            ))
        }
        TaskState::Cancelled(reason) => TaskState::Cancelled(reason),
        s => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_reports_nonterminal_generation_until_batch_finishes() {
        let store = BackgroundStore::new();
        store.enable_coordinator(crate::config::SubagentDispatchStrategy::Full);
        store
            .insert_coordinator_dispatch("task".into(), "research".into(), false, false, None)
            .unwrap();

        assert!(store.coordinator_generation_running());

        store.notify("task", TaskState::Completed("done".into()));
        assert!(!store.coordinator_generation_running());
    }

    #[test]
    fn coordinator_allows_one_retry_for_a_failed_dispatch() {
        let store = BackgroundStore::new();
        store.enable_coordinator(crate::config::SubagentDispatchStrategy::Full);
        store
            .insert_coordinator_dispatch("first".into(), "failed work".into(), false, false, None)
            .unwrap();
        store.notify("first", TaskState::Failed("boom".into()));
        store
            .insert_coordinator_dispatch(
                "retry".into(),
                "retry failed work".into(),
                false,
                false,
                Some("first"),
            )
            .unwrap();

        let error = store
            .insert_coordinator_dispatch(
                "second-retry".into(),
                "retry again".into(),
                false,
                false,
                Some("first"),
            )
            .unwrap_err();
        assert!(error.contains("already used its coordinator retry"));
    }

    #[test]
    fn coordinator_rejects_retry_for_cancelled_dispatch() {
        let store = BackgroundStore::new();
        store.enable_coordinator(crate::config::SubagentDispatchStrategy::Full);
        store
            .insert_coordinator_dispatch(
                "cancelled".into(),
                "cancelled work".into(),
                false,
                false,
                None,
            )
            .unwrap();
        store.notify("cancelled", TaskState::Cancelled("session switched".into()));

        let error = store
            .insert_coordinator_dispatch(
                "retry".into(),
                "retry cancelled work".into(),
                false,
                false,
                Some("cancelled"),
            )
            .unwrap_err();
        assert!(error.contains("must have failed"));
    }

    #[test]
    fn coordinator_allows_concurrent_isolated_writers() {
        let store = BackgroundStore::new();
        store.enable_coordinator(crate::config::SubagentDispatchStrategy::Full);
        store
            .insert_coordinator_dispatch("writer-one".into(), "edit one".into(), true, true, None)
            .unwrap();
        store
            .insert_coordinator_dispatch("writer-two".into(), "edit two".into(), true, true, None)
            .unwrap();
    }

    #[test]
    fn coordinator_serializes_writers_but_not_readers() {
        let store = BackgroundStore::new();
        store.enable_coordinator(crate::config::SubagentDispatchStrategy::Full);
        store
            .insert_coordinator_dispatch("writer".into(), "edit code".into(), true, false, None)
            .unwrap();
        store
            .insert_coordinator_dispatch("reader".into(), "inspect code".into(), false, false, None)
            .unwrap();

        let error = store
            .insert_coordinator_dispatch("writer-two".into(), "edit more".into(), true, false, None)
            .unwrap_err();
        assert!(error.contains("serialized writer"));
    }

    #[test]
    fn coordinator_dispatch_records_worktree_metadata() {
        let store = BackgroundStore::new();
        store.enable_coordinator(crate::config::SubagentDispatchStrategy::Full);
        store
            .insert_coordinator_dispatch("writer".into(), "edit code".into(), true, false, None)
            .unwrap();

        store
            .set_coordinator_dispatch_worktree(
                "writer",
                "dirge-task-writer".into(),
                "/tmp/dirge-task-writer".into(),
            )
            .unwrap();

        let dispatch = store.coordinator_dispatch("writer").unwrap();
        assert_eq!(
            dispatch.worktree_branch.as_deref(),
            Some("dirge-task-writer")
        );
        assert_eq!(
            dispatch.worktree_path.as_deref(),
            Some("/tmp/dirge-task-writer")
        );
    }

    #[test]
    fn insert_then_get_returns_running() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        let task = store.get("t1").expect("task present");
        assert_eq!(task.state, TaskState::Running);
    }

    #[test]
    fn get_on_missing_returns_none() {
        assert!(BackgroundStore::new().get("nope").is_none());
    }

    /// C1: `attach_handle` after terminal notify must NOT leak a
    /// phantom "running" slot into `handles` — the spawned-closure
    /// can race ahead on a multi-thread runtime and finish (notify)
    /// BEFORE the parent calls `attach_handle`. Only Running tasks
    /// get their handle tracked; a terminal task drops the handle.
    #[tokio::test]
    async fn attach_handle_after_terminal_notify_is_noop() {
        let store = BackgroundStore::new();
        store.insert("t1".into());

        // Simulate the spawned task finishing first.
        store.notify("t1", TaskState::Completed("done".into()));
        assert_eq!(store.running_count(), 0, "notify removes from handles");

        // Now attach happens — task exists but is Completed, so
        // attach must NOT insert the handle.
        let dummy_handle = tokio::spawn(async {});
        store.attach_handle("t1", dummy_handle);
        assert_eq!(
            store.running_count(),
            0,
            "attach_handle after terminal notify must not leak"
        );

        // Normal case: attach while Running → handle tracked.
        store.insert("t2".into());
        assert_eq!(store.get("t2").unwrap().state, TaskState::Running);
        let handle2 = tokio::spawn(async {});
        store.attach_handle("t2", handle2);
        assert_eq!(
            store.running_count(),
            1,
            "attach on Running must track handle"
        );
    }

    /// C2: `notify_if_running` transitions a Running task to terminal
    /// but is a no-op when the task is already terminal (so the drop
    /// guard can't clobber a real result).
    #[test]
    fn notify_if_running_guards_against_clobber() {
        let store = BackgroundStore::new();

        // notify_if_running on Running task → transitions.
        store.insert("t1".into());
        store.notify_if_running("t1", TaskState::Failed("early exit".into()));
        assert_eq!(
            store.get("t1").unwrap().state,
            TaskState::Failed("early exit".into()),
            "notify_if_running must transition Running → Failed"
        );

        // notify_if_running on terminal task → no-op.
        store.insert("t2".into());
        store.notify("t2", TaskState::Completed("real result".into()));
        store.notify_if_running("t2", TaskState::Failed("late bump".into()));
        assert_eq!(
            store.get("t2").unwrap().state,
            TaskState::Completed("real result".into()),
            "notify_if_running on terminal task must not clobber"
        );

        // notify_if_running with Running state is refused.
        store.insert("t3".into());
        store.notify_if_running("t3", TaskState::Running);
        assert_eq!(
            store.get("t3").unwrap().state,
            TaskState::Running,
            "notify_if_running with Running must be a no-op"
        );

        // notify_if_running on missing id → no-op (no panic).
        store.notify_if_running("ghost", TaskState::Failed("nope".into()));
    }

    /// Audit C6: `cancel_all` must abort in-flight handles, mark
    /// Running tasks as Failed("cancelled"), and clear pending
    /// notifications so the next session doesn't inherit them.
    /// Single-thread runtime is enough — `JoinHandle::abort()`
    /// only requires that the task be polled again to drop, which
    /// `yield_now().await` triggers below.
    #[tokio::test]
    async fn cancel_all_aborts_in_flight_tasks() {
        let store = BackgroundStore::new();
        store.insert("t1".into());

        // Spawn a long-running task and register its handle.
        let store_for_task = store.clone();
        let handle = tokio::spawn(async move {
            // Never completes naturally within the test window.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            store_for_task.notify("t1", TaskState::Completed("should not run".into()));
        });
        store.attach_handle("t1", handle);

        // Also enqueue a stale pending notification to verify
        // cancel_all clears the queue.
        store.insert("t_stale".into());
        store.notify("t_stale", TaskState::Completed("prev session".into()));
        assert_eq!(store.pending_len(), 1);

        store.cancel_all();

        // Pending notifications gone.
        assert_eq!(store.pending_len(), 0, "cancel_all must clear pending");

        // The still-Running task now reads as Cancelled("cancelled — ...").
        let t1 = store.get("t1").expect("t1 retained");
        match &t1.state {
            TaskState::Cancelled(reason) => assert!(
                reason.contains("cancelled"),
                "expected cancellation reason; got {:?}",
                reason
            ),
            other => panic!("expected Cancelled, got {:?}", other),
        }

        // Give the runtime a tick so the abort lands.
        tokio::task::yield_now().await;
    }

    /// C3: `cancel_all` collects and returns retained writer worktree
    /// metadata BEFORE dropping the coordinator, so the user can find
    /// their salvaged work instead of orphaned `dirge-task-*` dirs.
    #[test]
    fn cancel_all_reports_retained_worktrees() {
        let store = BackgroundStore::new();
        store.enable_coordinator(crate::config::SubagentDispatchStrategy::Full);

        // Seed a writer dispatch with a retained worktree.
        store
            .insert_coordinator_dispatch("w1".into(), "edit".into(), true, false, None)
            .unwrap();
        store
            .set_coordinator_dispatch_worktree(
                "w1",
                "dirge-task-w1".into(),
                "/tmp/dirge-task-w1".into(),
            )
            .unwrap();
        store.set_coordinator_dispatch_worktree_outcome(
            "w1",
            vec!["abc123".into()],
            true, // dirty
            true, // retained
        );

        let retained = store.cancel_all();
        assert_eq!(
            retained.len(),
            1,
            "retained writer worktree must be reported"
        );
        assert_eq!(retained[0].branch.as_deref(), Some("dirge-task-w1"));
        assert_eq!(retained[0].path.as_deref(), Some("/tmp/dirge-task-w1"));
    }

    /// C3: non-writer and non-retained dispatches must NOT appear in cancel_all output.
    #[test]
    fn cancel_all_omits_non_retained_and_readers() {
        let store = BackgroundStore::new();
        store.enable_coordinator(crate::config::SubagentDispatchStrategy::Full);

        // READER: non-writer — must not appear.
        store
            .insert_coordinator_dispatch("r1".into(), "read".into(), false, false, None)
            .unwrap();

        let retained = store.cancel_all();
        assert!(
            retained.is_empty(),
            "non-writer dispatches must not appear in retained list"
        );
    }

    // Regression: previously get() evicted completed/failed tasks. The new
    // model keeps tasks until eviction by LRU cap, since notifications are
    // delivered out-of-band and task_status is read-only.
    #[test]
    fn regression_get_is_read_only_after_completion() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("done".into()));

        for _ in 0..3 {
            let task = store.get("t1").expect("must remain after read");
            assert_eq!(task.state, TaskState::Completed("done".into()));
        }
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn notify_pushes_completed_to_pending_queue() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("done".into()));
        assert_eq!(store.pending_len(), 1);
    }

    // dirge-nmv5: large Completed payloads are relayed to the
    // disk-backed `output_relay`. The stored state now holds a
    // head/tail summary that points the agent at the full text on
    // disk via the `read` tool — the prior 3000-char silent
    // truncation is gone. We verify the relayed text carries the
    // recovery hint so the agent can fetch the missing middle.
    #[test]
    fn regression_notify_relays_large_completed_payload() {
        let _relay_dir = crate::agent::tools::output_relay::TransientDirGuard::new();
        let store = BackgroundStore::new();
        store.insert("t1".into());
        // Multi-line payload well past the 8 KiB byte threshold AND
        // the 200-line threshold so the relay fires AND its
        // head/tail summary elides the middle.
        let huge: String = (0..5_000)
            .map(|i| format!("subagent output line {i}\n"))
            .collect();
        store.notify("t1", TaskState::Completed(huge));

        let TaskState::Completed(text) = store.get("t1").unwrap().state else {
            panic!("expected Completed");
        };
        // Relay summary includes the `read`-tool hint + transient
        // path so the agent can recover the elided middle.
        assert!(
            text.contains("`read`"),
            "relayed summary must mention `read` tool: {text}",
        );
        assert!(
            text.contains(
                &crate::agent::tools::output_relay::transient_base()
                    .display()
                    .to_string()
            ),
            "relayed summary must reference the transient path: {text}",
        );
    }

    // dirge-cnx3: the truncation marker is prefixed to the payload BEFORE it
    // reaches the store, so it has to survive the relay's head/tail summary
    // (which keeps the first 50 lines) AND the coordinator's notification
    // rendering. If it didn't, a cut-off subagent would go back to reading as
    // `completed:` for exactly the large payloads the relay fires on.
    #[test]
    fn truncation_marker_survives_relay_and_notification() {
        use crate::agent::tools::task::SUBAGENT_TRUNCATED_MARKER;
        let _relay_dir = crate::agent::tools::output_relay::TransientDirGuard::new();
        let store = BackgroundStore::new();
        store.enable_coordinator(crate::config::SubagentDispatchStrategy::Full);
        store
            .insert_coordinator_dispatch("t1".into(), "audit the loop".into(), false, false, None)
            .unwrap();
        let body: String = (0..5_000)
            .map(|i| format!("subagent narration line {i}\n"))
            .collect();
        let marked = format!("{SUBAGENT_TRUNCATED_MARKER}\n\n{body}");
        store.notify("t1", TaskState::Completed(marked));

        let drained = store.drain_notifications();
        let rendered = format_notifications(&store, &drained);
        assert!(
            rendered.contains(SUBAGENT_TRUNCATED_MARKER),
            "coordinator must still see the cutoff marker after relay: {rendered}",
        );
        // And it stays adjacent to the `completed:` heading rather than being
        // pushed past the elision — the parent reads the two together.
        let completed_at = rendered.find("completed:").expect("completed heading");
        let marker_at = rendered.find(SUBAGENT_TRUNCATED_MARKER).unwrap();
        assert!(
            marker_at > completed_at && marker_at - completed_at < 32,
            "marker must lead the payload, not trail it: {rendered}",
        );
    }

    #[test]
    fn regression_notify_truncates_failed_error() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        let huge = "e".repeat(MAX_TASK_OUTPUT_CHARS * 2);
        let huge_len = huge.len();
        store.notify("t1", TaskState::Failed(huge));

        let TaskState::Failed(text) = store.get("t1").unwrap().state else {
            panic!("expected Failed");
        };
        // dirge-06cp: capped to the byte ceiling AND marked (no longer a
        // silent chop) — head preserved, a truncation marker appended.
        assert!(
            text.starts_with(&"e".repeat(MAX_TASK_OUTPUT_CHARS)),
            "head must be preserved up to the cap"
        );
        assert!(
            text.contains("truncated"),
            "must carry a truncation marker: {text}"
        );
        assert!(text.len() < huge_len, "must be shorter than the original");
    }

    // dirge-nmv5: short Completed payloads must pass through the
    // relay verbatim — no summary, no transient file. The agent
    // sees exactly what the subagent produced.
    #[test]
    fn small_completed_payload_passes_through_relay_verbatim() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        let small = "subagent answer: 42\nplus a couple more lines.\n".to_string();
        store.notify("t1", TaskState::Completed(small.clone()));

        let TaskState::Completed(text) = store.get("t1").unwrap().state else {
            panic!("expected Completed");
        };
        assert_eq!(text, small, "small payload must round-trip unchanged");
    }

    #[test]
    fn notify_on_missing_id_is_noop() {
        let store = BackgroundStore::new();
        store.notify("ghost", TaskState::Completed("never inserted".into()));
        assert!(store.get("ghost").is_none());
        assert_eq!(store.pending_len(), 0);
    }

    #[test]
    fn notify_with_running_state_is_noop() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Running);
        assert_eq!(store.pending_len(), 0);
        // State unchanged.
        assert_eq!(store.get("t1").unwrap().state, TaskState::Running);
    }

    // Regression: notify() must be idempotent on the pending queue — if a
    // subagent runner accidentally double-notifies, the agent must not see
    // the same completion twice.
    #[test]
    fn regression_double_notify_enqueues_once() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("first".into()));
        store.notify("t1", TaskState::Completed("second".into()));
        assert_eq!(store.pending_len(), 1);
        // The latest state wins.
        let TaskState::Completed(text) = store.get("t1").unwrap().state else {
            panic!("expected Completed");
        };
        assert_eq!(text, "second");
    }

    #[test]
    fn drain_returns_pending_then_empties_queue() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.insert("t2".into());
        store.notify("t1", TaskState::Completed("a".into()));
        store.notify("t2", TaskState::Failed("b".into()));

        let drained = store.drain_notifications();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].id, "t1");
        assert_eq!(drained[0].state, TaskState::Completed("a".into()));
        assert_eq!(drained[1].id, "t2");
        assert_eq!(drained[1].state, TaskState::Failed("b".into()));

        // Drained items don't reappear; tasks remain in the store for lookup.
        assert!(store.drain_notifications().is_empty());
        assert!(store.get("t1").is_some());
        assert!(store.get("t2").is_some());
    }

    #[test]
    fn drain_is_empty_when_nothing_pending() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        // Insert alone doesn't enqueue — only notify() does.
        assert!(store.drain_notifications().is_empty());
    }

    // Regression: previously the store grew unbounded across long sessions.
    // The new bound is an LRU cap that evicts the oldest entry on overflow.
    #[test]
    fn regression_lru_evicts_oldest_at_capacity() {
        let store = BackgroundStore::new();
        for i in 0..STORE_CAPACITY {
            store.insert(format!("t{i}"));
        }
        assert_eq!(store.len(), STORE_CAPACITY);
        // One more push past capacity.
        store.insert("overflow".into());
        assert_eq!(store.len(), STORE_CAPACITY);
        // The oldest (t0) is gone; the newest is retained.
        assert!(store.get("t0").is_none());
        assert!(store.get("overflow").is_some());
        assert!(store.get(&format!("t{}", STORE_CAPACITY - 1)).is_some());
    }

    // Re-inserting an existing id must not trigger eviction — that would
    // surprise callers who happened to reuse an id.
    #[test]
    fn re_insert_existing_id_does_not_evict() {
        let store = BackgroundStore::new();
        for i in 0..STORE_CAPACITY {
            store.insert(format!("t{i}"));
        }
        // Re-insert at capacity: the existing id should just be reset.
        store.insert("t5".into());
        assert_eq!(store.len(), STORE_CAPACITY);
        assert!(store.get("t0").is_some(), "oldest must NOT be evicted");
        assert_eq!(store.get("t5").unwrap().state, TaskState::Running);
    }

    // Regression: a task that's been evicted before notify() runs must not
    // produce a phantom notification.
    #[test]
    fn regression_notify_on_evicted_id_is_noop() {
        let store = BackgroundStore::new();
        for i in 0..STORE_CAPACITY {
            store.insert(format!("t{i}"));
        }
        store.insert("overflow".into()); // evicts t0

        store.notify("t0", TaskState::Completed("late".into()));
        assert_eq!(store.pending_len(), 0);
        assert!(store.drain_notifications().is_empty());
    }

    // The store is Clone + thread-safe; clones must share inner state.
    #[test]
    fn clones_share_state() {
        let a = BackgroundStore::new();
        let b = a.clone();
        a.insert("t1".into());
        b.notify("t1", TaskState::Completed("via b".into()));

        let drained = a.drain_notifications();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "t1");
    }

    // ---- prepend_pending_notifications ----

    #[test]
    fn prepend_passthrough_when_store_is_none() {
        let out = prepend_pending_notifications("hello", None);
        assert_eq!(out, "hello");
    }

    #[test]
    fn prepend_passthrough_when_nothing_pending() {
        let store = BackgroundStore::new();
        store.insert("t1".into()); // running, not pending
        let out = prepend_pending_notifications("hello", Some(&store));
        assert_eq!(out, "hello");
    }

    #[test]
    fn prepend_formats_system_reminder() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("the result".into()));

        let out = prepend_pending_notifications("user msg", Some(&store));
        assert!(out.starts_with("<system-reminder>\n"));
        assert!(out.contains("[task t1] completed: the result"));
        assert!(out.contains("the result"));
        assert!(out.contains("</system-reminder>\n\n"));
        assert!(out.ends_with("user msg"));
    }

    #[test]
    fn prepend_includes_failed_tasks() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Failed("kaboom".into()));

        let out = prepend_pending_notifications("user msg", Some(&store));
        assert!(out.contains("[task t1] failed: kaboom"));
        assert!(out.contains("kaboom"));
    }

    #[test]
    fn coordinator_prepend_includes_reconciliation_metadata() {
        let store = BackgroundStore::new();
        store.enable_coordinator(crate::config::SubagentDispatchStrategy::Full);
        store
            .insert_coordinator_dispatch("writer".into(), "edit code".into(), true, true, None)
            .unwrap();
        store
            .set_coordinator_dispatch_worktree(
                "writer",
                "dirge-task-writer".into(),
                "/tmp/dirge-task-writer".into(),
            )
            .unwrap();
        store.set_coordinator_dispatch_worktree_outcome(
            "writer",
            vec!["abc123".into()],
            false,
            true,
        );
        store.notify("writer", TaskState::Completed("done".into()));

        let out = prepend_pending_notifications("user msg", Some(&store));
        assert!(out.contains("prompt: edit code"));
        assert!(out.contains("writer isolation: worktree"));
        assert!(out.contains("writer branch: dirge-task-writer"));
        assert!(out.contains("writer commits: abc123"));
        assert!(out.contains("writer salvage path: /tmp/dirge-task-writer"));
        assert!(out.contains("completed: done"));
    }

    // Regression: prepend MUST consume the queue. Calling it twice in
    // succession must not re-deliver the same notifications, otherwise the
    // agent would see the same completion on every turn.
    #[test]
    fn regression_prepend_drains_queue_once() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("once".into()));

        let first = prepend_pending_notifications("msg", Some(&store));
        assert!(first.contains("once"));

        let second = prepend_pending_notifications("msg", Some(&store));
        assert_eq!(second, "msg");
    }

    #[test]
    fn prepend_includes_all_pending_tasks_in_order() {
        let store = BackgroundStore::new();
        for i in 0..3 {
            store.insert(format!("t{i}"));
            store.notify(&format!("t{i}"), TaskState::Completed(format!("r{i}")));
        }
        let out = prepend_pending_notifications("msg", Some(&store));
        // FIFO order preserved.
        let i0 = out.find("[task t0]").unwrap();
        let i1 = out.find("[task t1]").unwrap();
        let i2 = out.find("[task t2]").unwrap();
        assert!(i0 < i1 && i1 < i2);
    }

    // ---- UI lifecycle sink ----

    fn unwrap_finished(evt: LifecycleEvent) -> TaskNotification {
        match evt {
            LifecycleEvent::Finished(n) => n,
            other => panic!("expected Finished, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn ui_sink_receives_completion_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let store = BackgroundStore::with_ui_sink(tx);
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("done".into()));

        let notif = unwrap_finished(rx.recv().await.expect("event delivered"));
        assert_eq!(notif.id, "t1");
        assert_eq!(notif.state, TaskState::Completed("done".into()));
    }

    #[tokio::test]
    async fn ui_sink_receives_failure_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let store = BackgroundStore::with_ui_sink(tx);
        store.insert("t1".into());
        store.notify("t1", TaskState::Failed("boom".into()));

        let notif = unwrap_finished(rx.recv().await.unwrap());
        assert_eq!(notif.state, TaskState::Failed("boom".into()));
    }

    // dirge-nmv5: lifecycle events must carry the RELAYED payload,
    // not the original — otherwise the UI could render an unbounded
    // blob from the subagent into the user's scrollback. The
    // disk-backed relay replaces the prior 3000-char hard cap, so
    // we check the event carries a head/tail summary plus the
    // recovery hint (full payload is on disk).
    #[tokio::test]
    async fn ui_sink_event_carries_relayed_payload() {
        let _relay_dir = crate::agent::tools::output_relay::TransientDirGuard::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let store = BackgroundStore::with_ui_sink(tx);
        store.insert("t1".into());
        // Multi-line payload well over the 200-line threshold and
        // 8 KiB byte threshold so the relay's head/tail summary
        // elides the middle (single-line payloads still relay but
        // the summary contains the whole thing because head=tail).
        let huge: String = (0..5_000)
            .map(|i| format!("subagent output line {i}\n"))
            .collect();
        let original_len = huge.len();
        store.notify("t1", TaskState::Completed(huge));

        let notif = unwrap_finished(rx.recv().await.unwrap());
        let TaskState::Completed(text) = notif.state else {
            panic!("expected Completed");
        };
        // Summary must elide enough lines to come in well under the
        // original — guards against the UI getting the full blob.
        assert!(
            text.len() < original_len / 2,
            "relayed summary should be much smaller than the original (got {} of {} bytes)",
            text.len(),
            original_len,
        );
        // And it should carry the recovery hint.
        assert!(
            text.contains("`read`"),
            "relayed summary must mention `read` tool",
        );
    }

    // Regression: notify on a running state must NOT emit a lifecycle event
    // (Running isn't a terminal transition; no UI line wanted).
    #[tokio::test]
    async fn ui_sink_does_not_receive_running_state() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let store = BackgroundStore::with_ui_sink(tx);
        store.insert("t1".into());
        store.notify("t1", TaskState::Running);

        // Drain non-blockingly: nothing should be queued.
        assert!(rx.try_recv().is_err());
    }

    // Regression: notify on an evicted id is a no-op for both the pending
    // queue AND the UI sink — no phantom events.
    #[tokio::test]
    async fn ui_sink_no_event_for_evicted_id() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let store = BackgroundStore::with_ui_sink(tx);
        store.notify("ghost", TaskState::Completed("late".into()));
        assert!(rx.try_recv().is_err());
    }

    // Regression M1: notification payload is snapshotted at notify time, so
    // task eviction between notify and drain does not lose the result.
    #[test]
    fn regression_drain_returns_snapshotted_state_after_eviction() {
        let store = BackgroundStore::new();
        store.insert("victim".into());
        store.notify("victim", TaskState::Completed("the result".into()));

        // Push enough new inserts to evict "victim" from the task map.
        for i in 0..STORE_CAPACITY {
            store.insert(format!("filler{i}"));
        }
        assert!(store.get("victim").is_none(), "victim must be evicted");

        // The pending queue still has the snapshot.
        let drained = store.drain_notifications();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "victim");
        assert_eq!(drained[0].state, TaskState::Completed("the result".into()));
    }

    // Regression M5: notify_started fires a Started event on the UI sink and
    // does NOT enqueue an LLM-side notification (started is conveyed via the
    // tool result; only finished tasks get the <system-reminder>).
    #[tokio::test]
    async fn notify_started_fires_only_on_ui_sink() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let store = BackgroundStore::with_ui_sink(tx);
        store.insert("t1".into());
        store.notify_started("t1");

        let evt = rx.recv().await.expect("Started event delivered");
        match evt {
            LifecycleEvent::Started { id } => assert_eq!(id, "t1"),
            other => panic!("expected Started, got {other:?}"),
        }
        // No LLM-side notification queued.
        assert!(store.drain_notifications().is_empty());
    }

    // notify_started before insert is allowed — the id may or may not be
    // resolvable later. The event still fires (we just told the UI someone
    // is starting work). This is defensive: in practice TaskTool always
    // inserts first, then notify_started.
    #[tokio::test]
    async fn notify_started_with_no_ui_sink_is_noop() {
        let store = BackgroundStore::new();
        store.notify_started("t1");
        assert_eq!(store.pending_len(), 0);
    }

    // Regression: dropping the UI receiver must not break notify() — the
    // store is best-effort with the sink. Used when the UI exits before
    // long-running subagents finish.
    #[tokio::test]
    async fn ui_sink_send_after_receiver_dropped_is_silent() {
        let (tx, rx) = mpsc::unbounded_channel();
        let store = BackgroundStore::with_ui_sink(tx);
        store.insert("t1".into());
        drop(rx);
        // Must not panic.
        store.notify("t1", TaskState::Completed("payload".into()));
        // Drain queue still works for the LLM side.
        let drained = store.drain_notifications();
        assert_eq!(drained.len(), 1);
    }

    // ---- followup_from_background_store (dirge-9tfq) ----

    use crate::agent::agent_loop::message::LoopMessage;

    /// `subagent_completion_injects_followup_message`: when a background
    /// subagent completes, the followup hook returns a synthetic user
    /// message wrapping the result in a `<system-reminder>` block.
    /// This is what the parent loop's outer-boundary poll will see and
    /// inject into the next inner-loop iteration.
    #[tokio::test]
    async fn subagent_completion_injects_followup_message() {
        let store = BackgroundStore::new();
        store.insert("abc123".into());
        store.notify("abc123", TaskState::Completed("the answer is 42".into()));

        let hook = followup_from_background_store(store.clone());
        let messages = hook().await;

        assert_eq!(messages.len(), 1, "exactly one synthesized user message");
        let LoopMessage::User(u) = &messages[0] else {
            panic!("expected User message, got {:?}", messages[0]);
        };
        let body = u.text_joined();
        // System-reminder wrapper present.
        assert!(body.starts_with("<system-reminder>\n"));
        assert!(body.ends_with("</system-reminder>"));
        // Task id + completion marker + result text all present.
        assert!(body.contains("[task abc123] completed:"));
        assert!(body.contains("the answer is 42"));

        // Queue drained — second poll returns empty so the outer loop
        // can exit naturally on a clean board.
        assert!(hook().await.is_empty());
    }

    /// `subagent_failure_injects_followup_with_error_marker`: failed
    /// subagents surface via the same hook but tagged `failed:` rather
    /// than `completed:`. The model needs to distinguish so it can
    /// recover (retry, fall back, or report the failure to the user).
    #[tokio::test]
    async fn subagent_failure_injects_followup_with_error_marker() {
        let store = BackgroundStore::new();
        store.insert("xyz789".into());
        store.notify(
            "xyz789",
            TaskState::Failed("connection reset by peer".into()),
        );

        let hook = followup_from_background_store(store);
        let messages = hook().await;

        assert_eq!(messages.len(), 1);
        let LoopMessage::User(u) = &messages[0] else {
            panic!("expected User message");
        };
        let body = u.text_joined();
        assert!(body.contains("[task xyz789] failed:"));
        assert!(body.contains("connection reset by peer"));
        // Must NOT use the "completed" marker for failures.
        assert!(
            !body.contains("completed:"),
            "failures must not be tagged 'completed': {}",
            body,
        );
    }

    /// Multiple completions since the last poll are batched into a
    /// single `<system-reminder>` so the model gets all results in
    /// one turn rather than waking once per task.
    #[tokio::test]
    async fn followup_batches_multiple_completions_in_one_reminder() {
        let store = BackgroundStore::new();
        for i in 0..3 {
            store.insert(format!("t{i}"));
            store.notify(
                &format!("t{i}"),
                TaskState::Completed(format!("result-{i}")),
            );
        }
        let hook = followup_from_background_store(store);
        let messages = hook().await;

        assert_eq!(messages.len(), 1, "one reminder, not one per task");
        let LoopMessage::User(u) = &messages[0] else {
            panic!("expected User");
        };
        let body = u.text_joined();
        // All three tasks present.
        assert!(body.contains("[task t0] completed: result-0"));
        assert!(body.contains("[task t1] completed: result-1"));
        assert!(body.contains("[task t2] completed: result-2"));
        // FIFO ordering preserved.
        let i0 = body.find("t0").unwrap();
        let i1 = body.find("t1").unwrap();
        let i2 = body.find("t2").unwrap();
        assert!(i0 < i1 && i1 < i2);
    }

    /// Empty store → empty Vec. Outer loop sees no follow-up and exits.
    /// Critical: if this returned `vec![empty_message]`, the outer loop
    /// would spin re-entering the inner loop with a blank user turn.
    #[tokio::test]
    async fn followup_returns_empty_when_no_completions() {
        let store = BackgroundStore::new();
        store.insert("running".into()); // inserted but not notified
        let hook = followup_from_background_store(store);
        assert!(hook().await.is_empty());
    }

    /// Polling twice in a row only delivers each notification once.
    /// This is the same drain-semantics as `prepend_pending_notifications`
    /// — without it, the model would see the same completion on every
    /// outer-loop boundary and spam tool calls reacting to a result it
    /// already handled.
    #[tokio::test]
    async fn followup_drains_queue_once() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("once".into()));

        let hook = followup_from_background_store(store);
        let first = hook().await;
        assert_eq!(first.len(), 1);
        let second = hook().await;
        assert!(second.is_empty(), "second poll must not redeliver");
    }

    #[test]
    fn coordinator_batch_withholds_partial_results_until_every_member_is_terminal() {
        let store = BackgroundStore::new();
        store.enable_coordinator(crate::config::SubagentDispatchStrategy::Full);
        store.insert_coordinated("first".into());
        store.insert_coordinated("second".into());

        store.notify("first", TaskState::Completed("one".into()));
        assert!(store.drain_notifications().is_empty());
        assert!(!store.has_pending_notifications());

        store.notify("second", TaskState::Failed("two".into()));
        let drained = store.drain_notifications();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].id, "first");
        assert_eq!(drained[1].id, "second");
        assert!(store.drain_notifications().is_empty());
    }

    #[test]
    fn coordinator_opens_next_generation_after_delivery() {
        let store = BackgroundStore::new();
        store.enable_coordinator(crate::config::SubagentDispatchStrategy::Full);
        store.insert_coordinated("first".into());
        store.notify("first", TaskState::Completed("one".into()));
        assert_eq!(store.drain_notifications().len(), 1);

        store.insert_coordinated("second".into());
        store.notify("second", TaskState::Completed("two".into()));
        let drained = store.drain_notifications();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "second");
    }

    // Concurrency smoke: many threads inserting + notifying must not lose
    // notifications. Each thread's task should be drainable from any handle.
    #[test]
    fn concurrent_inserts_and_notifies() {
        let store = BackgroundStore::new();
        let mut handles = Vec::new();
        // Stay below STORE_CAPACITY so nothing gets evicted.
        let n = STORE_CAPACITY;
        for i in 0..n {
            let s = store.clone();
            let id = format!("t{i}");
            handles.push(std::thread::spawn(move || {
                s.insert(id.clone());
                s.notify(&id, TaskState::Completed(format!("done-{i}")));
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let drained = store.drain_notifications();
        assert_eq!(drained.len(), n);
        // Every id appears exactly once.
        let mut ids: Vec<String> = drained.into_iter().map(|n| n.id).collect();
        ids.sort();
        let mut expected: Vec<String> = (0..n).map(|i| format!("t{i}")).collect();
        expected.sort();
        assert_eq!(ids, expected);
    }
}
