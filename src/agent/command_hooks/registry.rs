//! L3 pipeline: the registry of configured hooks and the per-event run.
//!
//! Each command runs down one track:
//! `runner.run(..).and_then(policy::interpret)`. A failure on that track
//! allows the action and is logged at the end.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::boundary::{self, HookRunner};
use super::domain::{HookCommand, HookError, HookEvent, HookOutcome, HooksConfig};
use super::policy;

/// The resolved hook registry.
pub struct CommandHooks {
    events: HooksConfig,
    project_dir: PathBuf,
    runner: Arc<dyn HookRunner>,
    session_context: Mutex<HashMap<String, Option<String>>>,
}

impl std::fmt::Debug for CommandHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandHooks")
            .field("events", &self.events)
            .field("project_dir", &self.project_dir)
            .finish_non_exhaustive()
    }
}

impl CommandHooks {
    pub fn new(events: HooksConfig, project_dir: PathBuf, runner: Arc<dyn HookRunner>) -> Self {
        Self {
            events: policy::normalize(events),
            project_dir,
            runner,
            session_context: Mutex::new(HashMap::new()),
        }
    }

    /// dirge's own `hooks` block plus, when `claude_hooks` is set, the
    /// `hooks` blocks of `~/.claude/settings.json`,
    /// `<project>/.claude/settings.json` and
    /// `<project>/.claude/settings.local.json`, concatenated per event.
    pub fn from_sources(
        native: Option<&HooksConfig>,
        claude_hooks: bool,
        project_dir: PathBuf,
        home: Option<&Path>,
        runner: Arc<dyn HookRunner>,
    ) -> Self {
        let mut events = native.cloned().unwrap_or_default();
        if claude_hooks {
            for path in claude_settings_paths(home, &project_dir) {
                match load_settings_hooks(&path) {
                    Ok(block) => events = policy::merge(events, block),
                    Err(e) => {
                        tracing::warn!(target: "dirge::hooks", error = %e, "settings hooks skipped")
                    }
                }
            }
        }
        Self::new(events, project_dir, runner)
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn has(&self, event: HookEvent) -> bool {
        self.events.contains_key(event.as_str())
    }

    pub fn configured_events(&self) -> Vec<HookEvent> {
        HookEvent::ALL
            .into_iter()
            .filter(|e| self.has(*e))
            .collect()
    }

    /// Commands for `event` whose group accepts any of `targets`.
    pub fn commands_for(&self, event: HookEvent, targets: &[&str]) -> Vec<HookCommand> {
        self.events
            .get(event.as_str())
            .map(|groups| {
                groups
                    .iter()
                    .filter(|g| policy::group_applies(g, targets))
                    .flat_map(|g| g.hooks.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One command, start to verdict.
    fn judge(
        &self,
        event: HookEvent,
        cmd: &HookCommand,
        payload: &str,
    ) -> Result<HookOutcome, HookError> {
        self.runner
            .run(cmd, payload, &self.project_dir)
            .and_then(|exited| policy::interpret(event, exited))
    }

    /// Every matching command, folded. Blocking.
    pub fn run(&self, event: HookEvent, targets: &[&str], payload: &Value) -> HookOutcome {
        let payload = payload.to_string();
        self.commands_for(event, targets)
            .iter()
            .map(|cmd| {
                self.judge(event, cmd, &payload).unwrap_or_else(|e| {
                    tracing::warn!(
                        target: "dirge::hooks",
                        event = %event, command = %cmd.label(), error = %e,
                        "hook produced no verdict, action allowed",
                    );
                    HookOutcome::default()
                })
            })
            .fold(HookOutcome::default(), HookOutcome::combine)
    }

    /// [`Self::run`] off the async executor.
    pub async fn run_async(
        self: &Arc<Self>,
        event: HookEvent,
        targets: Vec<String>,
        payload: Value,
    ) -> HookOutcome {
        let refs: Vec<&str> = targets.iter().map(String::as_str).collect();
        if self.commands_for(event, &refs).is_empty() {
            return HookOutcome::default();
        }
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = targets.iter().map(String::as_str).collect();
            this.run(event, &refs, &payload)
        })
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(target: "dirge::hooks", event = %event, error = %e, "hook task failed, action allowed");
            HookOutcome::default()
        })
    }

    /// [`Self::run`] from synchronous code that may sit on a tokio worker.
    pub fn run_blocking(&self, event: HookEvent, targets: &[&str], payload: &Value) -> HookOutcome {
        if self.commands_for(event, targets).is_empty() {
            return HookOutcome::default();
        }
        let on_multi_thread_runtime = tokio::runtime::Handle::try_current()
            .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
            .unwrap_or(false);
        if on_multi_thread_runtime {
            tokio::task::block_in_place(|| self.run(event, targets, payload))
        } else {
            self.run(event, targets, payload)
        }
    }

    /// Payload envelope for `event`, stamped with the current directory.
    pub fn payload(&self, event: HookEvent, session_id: Option<&str>, extra: Value) -> Value {
        let cwd = std::env::current_dir()
            .unwrap_or_else(|_| self.project_dir.clone())
            .display()
            .to_string();
        policy::payload(event, session_id, &cwd, extra)
    }

    /// `SessionStart` context, run the first time a session is seen and
    /// replayed afterwards.
    pub fn session_start_context(&self, session_id: Option<&str>, resumed: bool) -> Option<String> {
        if !self.has(HookEvent::SessionStart) {
            return None;
        }
        let key = session_id.unwrap_or("").to_string();
        let cached = self.session_cache().get(&key).cloned();
        if let Some(cached) = cached {
            return cached;
        }
        let source = if resumed { "resume" } else { "startup" };
        let payload = self.payload(
            HookEvent::SessionStart,
            session_id,
            json!({ "source": source }),
        );
        let context = self
            .run_blocking(HookEvent::SessionStart, &[source], &payload)
            .context_text();
        self.session_cache().insert(key, context.clone());
        context
    }

    fn session_cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, Option<String>>> {
        self.session_context
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// `SubagentStart` context for a freshly forked child.
    pub fn subagent_start_context(&self, agent_id: &str, agent_type: &str) -> Option<String> {
        if !self.has(HookEvent::SubagentStart) {
            return None;
        }
        let payload = self.payload(
            HookEvent::SubagentStart,
            Some(agent_id),
            json!({ "agent_id": agent_id, "agent_type": agent_type }),
        );
        self.run_blocking(HookEvent::SubagentStart, &[agent_type], &payload)
            .context_text()
    }

    /// `UserPromptSubmit` answer for `prompt`.
    pub fn user_prompt_submit(&self, session_id: Option<&str>, prompt: &str) -> HookOutcome {
        if !self.has(HookEvent::UserPromptSubmit) {
            return HookOutcome::default();
        }
        let payload = self.payload(
            HookEvent::UserPromptSubmit,
            session_id,
            json!({ "prompt": prompt }),
        );
        self.run_blocking(HookEvent::UserPromptSubmit, &[], &payload)
    }
}

fn load_settings_hooks(path: &Path) -> Result<HooksConfig, HookError> {
    let shown = path.display().to_string();
    boundary::read_settings(path).and_then(|text| match text {
        Some(text) => policy::parse_settings_hooks(&shown, &text),
        None => Ok(HooksConfig::new()),
    })
}

fn claude_settings_paths(home: Option<&Path>, project_dir: &Path) -> Vec<PathBuf> {
    let project = project_dir.join(".claude");
    let mut paths: Vec<PathBuf> = home
        .map(|h| h.join(".claude").join("settings.json"))
        .into_iter()
        .collect();
    for p in [
        project.join("settings.json"),
        project.join("settings.local.json"),
    ] {
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    paths
}
