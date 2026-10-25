//! The loaded addons and the runtime that runs them.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::domain::{
    AddonSummary, BeforeOutcome, CommandOutput, CommandSpec, HookPoint, LoadFailure, ReloadReport,
    ToolSpec,
};
use super::policy;
use super::port::AddonRuntime;
#[allow(unused_imports)]
use crate::sync_util::LockExt;

/// What a load works from: the manifests that validated, the ones that did
/// not, the classpath, and the addons' own source files, which a reload
/// evaluates again.
#[derive(Debug, Clone, Default)]
pub struct LoadSet {
    pub manifests: Vec<PathBuf>,
    pub failures: Vec<LoadFailure>,
    pub source_roots: Vec<PathBuf>,
    pub sources: Vec<PathBuf>,
}

/// The addons one load produced, collisions resolved.
#[derive(Debug, Clone, Default)]
struct Loaded {
    addons: Vec<AddonSummary>,
    failures: Vec<LoadFailure>,
    tools: Vec<ToolSpec>,
    commands: Vec<CommandSpec>,
    /// Every hook key each addon registered, by addon id, the ones no
    /// [`HookPoint`] names included: what [`AddonHost::emit`] reaches.
    hook_keys: Vec<(String, Vec<String>)>,
}

impl Loaded {
    fn new(addons: Vec<AddonSummary>, failures: Vec<LoadFailure>) -> Self {
        let (tools, dropped) = policy::unique_tools(&addons);
        for (addon, tool) in dropped {
            tracing::warn!(
                target: "dirge::addon",
                %addon, %tool,
                "addon tool dropped: an earlier addon already exposes that name"
            );
        }
        let (commands, dropped) = policy::unique_commands(&addons);
        for (addon, command) in dropped {
            tracing::warn!(
                target: "dirge::addon",
                %addon, %command,
                "addon command dropped: an earlier addon already registered that name"
            );
        }
        let hook_keys = addons
            .iter()
            .map(|a| {
                let keys = a.hooks.iter().map(|h| h.key().to_string()).collect();
                (a.id.clone(), keys)
            })
            .collect();
        Self {
            addons,
            failures,
            tools,
            commands,
            hook_keys,
        }
    }

    /// Add the hook keys load reports named beside the ones a [`HookPoint`]
    /// names; `reports` are `(addon id, load report)`.
    fn with_reported_keys(mut self, reports: &[(String, Value)]) -> Self {
        for (id, report) in reports {
            let reported = policy::hook_keys(report);
            if let Some((_, keys)) = self.hook_keys.iter_mut().find(|(a, _)| a == id) {
                for key in reported {
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
            }
        }
        self
    }

    fn listens_key(&self, key: &str) -> bool {
        self.hook_keys
            .iter()
            .any(|(_, keys)| keys.iter().any(|k| k == key))
    }
}

/// Load every manifest of `set` in order; failures stay beside the addons
/// that loaded.
fn load_all(runtime: &dyn AddonRuntime, set: &LoadSet, host_config: &Value) -> Loaded {
    let mut failures = set.failures.clone();
    let mut addons = Vec::new();
    let mut reports = Vec::new();
    for manifest in &set.manifests {
        let report = runtime.load(manifest, host_config);
        match policy::parse_summary(manifest, &report) {
            Ok(summary) => {
                reports.push((summary.id.clone(), report));
                addons.push(summary);
            }
            Err(failure) => failures.push(failure),
        }
    }
    Loaded::new(addons, failures).with_reported_keys(&reports)
}

/// `before` with the summaries a runtime re-read in place, `refreshed` being
/// one load report per addon. An addon whose report failed keeps what it
/// had, and the failure is logged.
fn refreshed(before: &Loaded, refreshed: &[Value]) -> Loaded {
    let mut addons = Vec::new();
    let mut reports = Vec::new();
    for old in &before.addons {
        let report = refreshed
            .iter()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(old.id.as_str()));
        match report.map(|r| (r, policy::parse_summary(&old.manifest, r))) {
            Some((report, Ok(summary))) => {
                reports.push((summary.id.clone(), report.clone()));
                addons.push(summary);
            }
            Some((_, Err(failure))) => {
                tracing::warn!(
                    target: "dirge::addon",
                    addon = %old.id,
                    error = %failure.error,
                    "addon refresh failed; keeping what it registered before"
                );
                if let Some((_, keys)) = before.hook_keys.iter().find(|(a, _)| *a == old.id) {
                    reports.push((old.id.clone(), serde_json::json!({ "hooks": keys })));
                }
                addons.push(old.clone());
            }
            None => addons.push(old.clone()),
        }
    }
    Loaded::new(addons, before.failures.clone()).with_reported_keys(&reports)
}

/// True for a slash command name that dirge dispatches before any addon
/// command, which no addon command may therefore take.
pub type Reserved = Arc<dyn Fn(&str) -> bool + Send + Sync>;

fn nothing_reserved() -> Mutex<Reserved> {
    Mutex::new(Arc::new(|_: &str| false))
}

pub struct AddonHost {
    runtime: Arc<dyn AddonRuntime>,
    host_config: Value,
    loaded: Mutex<Loaded>,
    reserved: Mutex<Reserved>,
}

impl std::fmt::Debug for AddonHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let loaded = self.loaded.lock_ignore_poison();
        f.debug_struct("AddonHost")
            .field("addons", &loaded.addons)
            .field("failures", &loaded.failures)
            .finish_non_exhaustive()
    }
}

impl AddonHost {
    /// A host over `addons` already loaded on `runtime`; tests build hosts
    /// this way around a scripted runtime.
    #[cfg(test)]
    pub fn new(
        runtime: Arc<dyn AddonRuntime>,
        addons: Vec<AddonSummary>,
        failures: Vec<LoadFailure>,
    ) -> Self {
        Self {
            runtime,
            host_config: Value::Object(Default::default()),
            loaded: Mutex::new(Loaded::new(addons, failures)),
            reserved: nothing_reserved(),
        }
    }

    /// Boot: load every manifest of `set` on a freshly started `runtime`.
    pub fn load(runtime: Arc<dyn AddonRuntime>, set: LoadSet, host_config: Value) -> Self {
        let loaded = load_all(runtime.as_ref(), &set, &host_config);
        Self {
            runtime,
            host_config,
            loaded: Mutex::new(loaded),
            reserved: nothing_reserved(),
        }
    }

    /// Replace every addon with what `set` loads now, in the order the
    /// runtime's reload rules demand: every addon is shut down BEFORE its
    /// code is evaluated again, only the addons' own sources are evaluated
    /// (never the libraries they depend on), then each is constructed and
    /// initialized afresh. New tools and hooks reach the agent at its next
    /// run.
    pub fn reload(&self, set: LoadSet) -> ReloadReport {
        let before = self.loaded.lock_ignore_poison().clone();
        for addon in &before.addons {
            self.runtime.unload(&addon.id);
        }
        self.runtime.set_source_roots(&set.source_roots);
        let source_errors = self
            .runtime
            .reload_sources(&set.sources)
            .into_iter()
            .map(|(manifest, error)| LoadFailure { manifest, error })
            .collect();
        let after = load_all(self.runtime.as_ref(), &set, &self.host_config);
        // What a refresh re-read before this reload describes addons that no
        // longer run; taking it in later would undo the reload.
        let _ = self.runtime.take_refreshed();
        let (tools_added, tools_removed) = policy::tool_diff(&before.tools, &after.tools);
        let report = ReloadReport {
            loaded: after.addons.iter().map(|a| a.id.clone()).collect(),
            failures: after.failures.clone(),
            source_errors,
            tools_added,
            tools_removed,
        };
        *self.loaded.lock_ignore_poison() = after;
        report
    }

    /// Every addon loaded, each listing only the commands that run.
    pub fn addons(&self) -> Vec<AddonSummary> {
        let commands = self.commands();
        let mut addons = self.loaded.lock_ignore_poison().addons.clone();
        for addon in &mut addons {
            addon.commands.retain(|c| commands.contains(c));
        }
        addons
    }

    pub fn failures(&self) -> Vec<LoadFailure> {
        self.loaded.lock_ignore_poison().failures.clone()
    }

    /// The tools offered to the model, collisions already resolved.
    pub fn tools(&self) -> Vec<ToolSpec> {
        self.loaded.lock_ignore_poison().tools.clone()
    }

    /// Withhold every addon command whose name `reserved` holds, now and
    /// after every reload, warning about each one withheld now.
    pub fn reserve_commands(&self, reserved: Reserved) {
        let commands = self.loaded.lock_ignore_poison().commands.clone();
        for command in commands.iter().filter(|c| reserved(&c.name)) {
            tracing::warn!(
                target: "dirge::addon",
                addon = %command.addon_id,
                command = %command.name,
                "addon command dropped: a built-in or plugin command already has that name"
            );
        }
        *self.reserved.lock_ignore_poison() = reserved;
    }

    /// The slash commands addons registered, collisions resolved and
    /// reserved names withheld.
    pub fn commands(&self) -> Vec<CommandSpec> {
        let reserved = self.reserved.lock_ignore_poison().clone();
        let commands = self.loaded.lock_ignore_poison().commands.clone();
        commands
            .into_iter()
            .filter(|c| !reserved(&c.name))
            .collect()
    }

    /// The command typed as `/name`, if an addon registered it and the name
    /// is not reserved.
    pub fn command(&self, name: &str) -> Option<CommandSpec> {
        let reserved = self.reserved.lock_ignore_poison().clone();
        if reserved(name) {
            return None;
        }
        self.loaded
            .lock_ignore_poison()
            .commands
            .iter()
            .find(|c| c.name == name)
            .cloned()
    }

    /// True when at least one addon contributes `point`, so callers can skip
    /// the interpreter round trip on the hot path.
    pub fn listens(&self, point: HookPoint) -> bool {
        self.loaded
            .lock_ignore_poison()
            .addons
            .iter()
            .any(|a| a.hooks.contains(&point))
    }

    /// True when at least one addon registered a hook keyed `key` (a keyword
    /// without its colon), whether or not a [`HookPoint`] names it.
    pub fn listens_key(&self, key: &str) -> bool {
        self.loaded.lock_ignore_poison().listens_key(key)
    }

    /// Every addon's answer to the hook keyed `key`, failures logged. The
    /// open counterpart of the [`HookPoint`] calls: a new seam calls this
    /// with its own key and reads the replies, and no addon type changes.
    pub fn emit(&self, key: &str, ctx: &Value) -> Vec<super::domain::HookReply> {
        if !self.listens_key(key) {
            return Vec::new();
        }
        let replies = self.runtime.run_hook_key(key, ctx);
        log_key_failures(key, &replies);
        replies
    }

    /// Hand `ctx` to the hook keyed `key` without waiting; answers are
    /// dropped. For observers on paths that must never block.
    pub fn post(&self, key: &str, ctx: &Value) {
        if self.listens_key(key) {
            self.runtime.post_hook(key, ctx);
        }
    }

    /// Take in what the runtime re-read since the last call (after a REPL
    /// evaluation or `dirge.harness/refresh!`): the addons' tools, hooks and
    /// commands are replaced, their lifecycles untouched. `None` when
    /// nothing was re-read.
    pub fn sync(&self) -> Option<ReloadReport> {
        let reports = self.runtime.take_refreshed()?;
        let mut loaded = self.loaded.lock_ignore_poison();
        let after = refreshed(&loaded, &reports);
        let (tools_added, tools_removed) = policy::tool_diff(&loaded.tools, &after.tools);
        let report = ReloadReport {
            loaded: after.addons.iter().map(|a| a.id.clone()).collect(),
            failures: after.failures.clone(),
            source_errors: Vec::new(),
            tools_added,
            tools_removed,
        };
        *loaded = after;
        Some(report)
    }

    /// Where a REPL into the addon runtime listens, if one does.
    pub fn repl_endpoint(&self) -> Option<String> {
        self.runtime.repl_endpoint()
    }

    /// Every hook key each addon registered, by addon id.
    pub fn hook_keys(&self) -> Vec<(String, Vec<String>)> {
        self.loaded.lock_ignore_poison().hook_keys.clone()
    }

    /// Run a tool: the runtime call, then the handler's return value read as
    /// `(content blocks, details)`.
    pub fn call_tool(&self, tool: &ToolSpec, args: &Value) -> Result<(Vec<Value>, Value), String> {
        self.runtime
            .call_tool(&tool.addon_id, &tool.name, args)
            .and_then(|out| policy::tool_output(&out))
    }

    /// Run a slash command with the text typed after its name.
    pub fn run_command(&self, command: &CommandSpec, args: &str) -> Result<CommandOutput, String> {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.runtime
            .run_command(
                &command.addon_id,
                &command.name,
                &policy::command_ctx(args, &cwd),
            )
            .map(|answer| policy::command_output(&answer))
    }

    /// Texts every addon answered `point` with.
    pub fn texts(&self, point: HookPoint, ctx: &Value) -> Vec<String> {
        policy::texts(&self.emit(point.key(), ctx))
    }

    /// The folded `BeforeToolCall` answer.
    pub fn before_tool_call(&self, ctx: &Value) -> BeforeOutcome {
        policy::fold_before(&self.emit(HookPoint::BeforeToolCall.key(), ctx))
    }

    /// Texts every addon answered `:dirge/session-start` with.
    pub fn session_start(&self, ctx: &Value) -> Vec<String> {
        self.texts(HookPoint::SessionStart, ctx)
    }

    /// Run `:dirge/session-end`; answers are ignored and failures logged.
    pub fn session_end(&self, ctx: &Value) {
        self.emit(HookPoint::SessionEnd.key(), ctx);
    }

    pub fn shutdown(&self) {
        self.runtime.shutdown();
    }
}

/// Logs failed hook replies.
fn log_key_failures(key: &str, replies: &[super::domain::HookReply]) {
    for reply in replies {
        if let Err(error) = &reply.result {
            tracing::warn!(
                target: "dirge::addon",
                addon = %reply.addon_id,
                hook = key,
                %error,
                "addon hook failed; ignored"
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::addons::domain::HookReply;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::path::Path;

    /// Scripted runtime: answers from fixed tables and records every call.
    #[derive(Default)]
    pub(crate) struct ScriptedRuntime {
        pub tool_answer: Option<Result<Value, String>>,
        pub command_answer: Option<Result<Value, String>>,
        pub hook_answers: Vec<HookReply>,
        /// Load reports, answered in order.
        pub load_answers: Mutex<VecDeque<Value>>,
        pub source_errors: Vec<(PathBuf, String)>,
        /// What `take_refreshed` answers, once.
        pub refreshed: Mutex<Option<Vec<Value>>>,
        pub calls: Mutex<Vec<String>>,
    }

    impl ScriptedRuntime {
        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl AddonRuntime for ScriptedRuntime {
        fn load(&self, manifest: &Path, _host_config: &Value) -> Value {
            self.record(format!("load {}", manifest.display()));
            self.load_answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| json!({"error": "unscripted load"}))
        }

        fn unload(&self, addon_id: &str) {
            self.record(format!("unload {addon_id}"));
        }

        fn reload_sources(&self, files: &[PathBuf]) -> Vec<(PathBuf, String)> {
            self.record(format!("reload-sources {}", files.len()));
            self.source_errors.clone()
        }

        fn set_source_roots(&self, roots: &[PathBuf]) {
            self.record(format!("roots {}", roots.len()));
        }

        fn call_tool(&self, addon_id: &str, tool: &str, args: &Value) -> Result<Value, String> {
            self.record(format!("tool {addon_id}/{tool} {args}"));
            self.tool_answer.clone().unwrap_or(Ok(Value::Null))
        }

        fn run_command(&self, addon_id: &str, name: &str, ctx: &Value) -> Result<Value, String> {
            self.record(format!("command {addon_id}/{name} {}", ctx["args"]));
            self.command_answer.clone().unwrap_or(Ok(Value::Null))
        }

        fn run_hook(&self, point: HookPoint, ctx: &Value) -> Vec<HookReply> {
            self.record(format!("hook {} {ctx}", point.key()));
            self.hook_answers.clone()
        }

        fn take_refreshed(&self) -> Option<Vec<Value>> {
            self.refreshed.lock().unwrap().take()
        }

        fn shutdown(&self) {
            self.record("shutdown".into());
        }
    }

    pub(crate) fn summary(id: &str, tools: &[&str], hooks: &[HookPoint]) -> AddonSummary {
        AddonSummary {
            id: id.into(),
            manifest: PathBuf::from(format!("{id}.edn")),
            tools: tools
                .iter()
                .map(|t| ToolSpec {
                    addon_id: id.into(),
                    name: t.to_string(),
                    exposed_name: policy::exposed_tool_name(t),
                    description: String::new(),
                    input_schema: json!({"type": "object"}),
                })
                .collect(),
            hooks: hooks.to_vec(),
            commands: Vec::new(),
            health: Value::Null,
        }
    }

    fn host(
        runtime: ScriptedRuntime,
        addons: Vec<AddonSummary>,
    ) -> (AddonHost, Arc<ScriptedRuntime>) {
        let rt = Arc::new(runtime);
        (AddonHost::new(rt.clone(), addons, Vec::new()), rt)
    }

    #[test]
    fn unhooked_points_never_reach_the_runtime() {
        let (host, rt) = host(
            ScriptedRuntime::default(),
            vec![summary("a", &[], &[HookPoint::OnPrompt])],
        );
        assert!(host.texts(HookPoint::SystemPrompt, &json!({})).is_empty());
        assert_eq!(host.before_tool_call(&json!({})), BeforeOutcome::default());
        assert!(rt.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn hooked_points_fold_replies_and_drop_failures() {
        let replies = vec![
            HookReply {
                addon_id: "a".into(),
                result: Err("boom".into()),
            },
            HookReply {
                addon_id: "b".into(),
                result: Ok(json!("from b")),
            },
        ];
        let (host, rt) = host(
            ScriptedRuntime {
                hook_answers: replies,
                ..Default::default()
            },
            vec![summary("a", &[], &[HookPoint::SystemPrompt])],
        );
        assert_eq!(
            host.texts(HookPoint::SystemPrompt, &json!({"cwd": "/w"})),
            vec!["from b".to_string()]
        );
        assert_eq!(
            *rt.calls.lock().unwrap(),
            vec![r#"hook dirge/system-prompt {"cwd":"/w"}"#.to_string()]
        );
    }

    #[test]
    fn tool_calls_run_through_the_runtime_then_the_output_policy() {
        let (host, rt) = host(
            ScriptedRuntime {
                tool_answer: Some(Ok(json!({"content": [{"type": "text", "text": "rows=3"}]}))),
                ..Default::default()
            },
            vec![summary("hd", &["swarm-view"], &[])],
        );
        let tool = host.tools()[0].clone();
        let (content, _) = host.call_tool(&tool, &json!({"rows": [1, 2, 3]})).unwrap();
        assert_eq!(content[0]["text"], "rows=3");
        assert_eq!(
            *rt.calls.lock().unwrap(),
            vec![r#"tool hd/swarm-view {"rows":[1,2,3]}"#.to_string()]
        );
    }

    #[test]
    fn runtime_errors_stay_on_the_failure_track() {
        let (host, _) = host(
            ScriptedRuntime {
                tool_answer: Some(Err("handler threw".into())),
                ..Default::default()
            },
            vec![summary("hd", &["t"], &[])],
        );
        let tool = host.tools()[0].clone();
        assert_eq!(
            host.call_tool(&tool, &json!({})).unwrap_err(),
            "handler threw"
        );
    }

    fn load_set(manifests: &[&str]) -> LoadSet {
        LoadSet {
            manifests: manifests.iter().map(PathBuf::from).collect(),
            failures: Vec::new(),
            source_roots: vec![PathBuf::from("/addons/a/src")],
            sources: vec![PathBuf::from("/addons/a/src/a/core.cljc")],
        }
    }

    #[test]
    fn reload_shuts_down_before_evaluating_then_loads_afresh() {
        let rt = ScriptedRuntime::default();
        rt.load_answers.lock().unwrap().push_back(json!({
            "id": "a",
            "tools": [{"name": "y"}, {"name": "z"}],
            "commands": [{"name": "go", "description": "run it"}]
        }));
        let (host, rt) = host(rt, vec![summary("a", &["x", "y"], &[])]);

        let report = host.reload(load_set(&["a.edn"]));

        assert_eq!(
            *rt.calls.lock().unwrap(),
            vec!["unload a", "roots 1", "reload-sources 1", "load a.edn"]
        );
        assert_eq!(report.loaded, vec!["a".to_string()]);
        assert_eq!(report.tools_added, vec!["z".to_string()]);
        assert_eq!(report.tools_removed, vec!["x".to_string()]);
        let tools: Vec<String> = host.tools().into_iter().map(|t| t.exposed_name).collect();
        assert_eq!(tools, vec!["y", "z"]);
        assert_eq!(host.command("go").map(|c| c.addon_id), Some("a".into()));
    }

    #[test]
    fn a_reload_that_fails_keeps_the_reason_and_drops_the_tools() {
        let rt = ScriptedRuntime {
            source_errors: vec![(PathBuf::from("/addons/a/src/a/core.cljc"), "eof".into())],
            ..Default::default()
        };
        rt.load_answers
            .lock()
            .unwrap()
            .push_back(json!({"error": "init-fn not found"}));
        let (host, _) = host(rt, vec![summary("a", &["x"], &[])]);

        let report = host.reload(load_set(&["a.edn"]));

        assert!(report.loaded.is_empty());
        assert_eq!(report.failures[0].error, "init-fn not found");
        assert_eq!(report.source_errors[0].error, "eof");
        assert_eq!(report.tools_removed, vec!["x".to_string()]);
        assert!(host.tools().is_empty());
        assert_eq!(host.failures().len(), 1);
    }

    #[test]
    fn commands_run_with_the_typed_text_and_read_the_answer() {
        let mut a = summary("a", &[], &[]);
        a.commands = vec![CommandSpec {
            addon_id: "a".into(),
            name: "go".into(),
            description: String::new(),
        }];
        let (host, rt) = host(
            ScriptedRuntime {
                command_answer: Some(Ok(json!({"text": "ok", "prompt": "continue"}))),
                ..Default::default()
            },
            vec![a],
        );
        let command = host.command("go").expect("registered");
        let out = host.run_command(&command, "fast please").unwrap();
        assert_eq!(out.text.as_deref(), Some("ok"));
        assert_eq!(out.prompt.as_deref(), Some("continue"));
        assert_eq!(
            *rt.calls.lock().unwrap(),
            vec![r#"command a/go "fast please""#.to_string()]
        );
        assert!(host.command("missing").is_none());
    }

    fn names(commands: Vec<CommandSpec>) -> Vec<String> {
        commands.into_iter().map(|c| c.name).collect()
    }

    #[test]
    fn reserved_command_names_are_withheld_from_every_listing() {
        let mut a = summary("a", &[], &[]);
        a.commands = ["memory", "go"]
            .iter()
            .map(|n| CommandSpec {
                addon_id: "a".into(),
                name: n.to_string(),
                description: String::new(),
            })
            .collect();
        let (host, _) = host(ScriptedRuntime::default(), vec![a]);
        assert!(host.command("memory").is_some(), "nothing reserved yet");

        host.reserve_commands(Arc::new(|name: &str| name == "memory"));

        assert_eq!(names(host.commands()), vec!["go"]);
        assert_eq!(names(host.addons()[0].commands.clone()), vec!["go"]);
        assert!(host.command("memory").is_none());
        assert!(host.command("go").is_some());
    }

    #[test]
    fn a_sync_takes_in_what_the_runtime_re_read_and_keeps_an_addon_that_failed() {
        let rt = ScriptedRuntime::default();
        *rt.refreshed.lock().unwrap() = Some(vec![
            json!({"id": "a", "tools": [{"name": "x"}, {"name": "new"}], "hooks": ["acme/ping"]}),
            json!({"id": "b", "error": "threw"}),
        ]);
        let (host, _) = host(
            rt,
            vec![summary("a", &["x"], &[]), summary("b", &["kept"], &[])],
        );

        let report = host.sync().expect("something was re-read");

        assert_eq!(report.tools_added, vec!["new".to_string()]);
        assert!(report.tools_removed.is_empty(), "b keeps its tool");
        let tools: Vec<String> = host.tools().into_iter().map(|t| t.exposed_name).collect();
        assert_eq!(tools, vec!["x", "new", "kept"]);
        assert!(
            host.listens_key("acme/ping"),
            "open keys come with the refresh"
        );
        assert!(host.sync().is_none(), "taken once");
    }

    #[test]
    fn a_reload_drops_what_a_refresh_re_read_before_it() {
        let rt = ScriptedRuntime::default();
        rt.load_answers
            .lock()
            .unwrap()
            .push_back(json!({"id": "a", "tools": [{"name": "y"}]}));
        *rt.refreshed.lock().unwrap() =
            Some(vec![json!({"id": "a", "tools": [{"name": "stale"}]})]);
        let (host, _) = host(rt, vec![summary("a", &["x"], &[])]);

        host.reload(load_set(&["a.edn"]));

        assert!(host.sync().is_none());
        let tools: Vec<String> = host.tools().into_iter().map(|t| t.exposed_name).collect();
        assert_eq!(tools, vec!["y"]);
    }

    #[test]
    fn a_reservation_holds_across_reloads() {
        let rt = ScriptedRuntime::default();
        rt.load_answers.lock().unwrap().push_back(json!({
            "id": "a",
            "commands": [{"name": "plan"}, {"name": "go"}]
        }));
        let (host, _) = host(rt, vec![summary("a", &[], &[])]);
        host.reserve_commands(Arc::new(|name: &str| name == "plan"));

        host.reload(load_set(&["a.edn"]));

        assert_eq!(names(host.commands()), vec!["go"]);
        assert!(host.command("plan").is_none());
    }
}
