//! Addon slash commands and `/addons reload`, run off the event loop.
//!
//! [`spawn`] runs an [`AddonJob`] on a blocking thread behind a phase task;
//! the `addon_phase` arm of the `run_interactive` select loop receives the
//! [`AddonDone`] and [`land`]s it. While a job runs the loop keeps serving
//! input and permission prompts, so a job that calls a permission-gated
//! tool through `dirge.harness/call-tool` can be answered.
//!
//! The handle type is unconditional so the select arm can be too; the rest
//! needs the `addons` feature, and without it the field stays `None`.

use crate::ui::phase::PhaseHandle;

#[cfg(feature = "addons")]
use std::sync::Arc;

#[cfg(feature = "addons")]
use crate::addons::domain::{CommandOutput, CommandSpec, ReloadReport};
#[cfg(feature = "addons")]
use crate::addons::host::AddonHost;
#[cfg(feature = "addons")]
use crate::agent::agent_loop::LoopTool;
#[cfg(feature = "addons")]
use crate::permission::ask::AskSender;
#[cfg(feature = "addons")]
use crate::permission::checker::PermCheck;
#[cfg(feature = "addons")]
use crate::provider::AnyAgent;

/// Handle to a running job: the channel its [`AddonDone`] arrives on, and
/// the task, so Ctrl+C can abort it. Aborting stops the wait, not the addon
/// code already running on the isolate.
#[cfg_attr(not(feature = "addons"), allow(dead_code))]
pub(crate) struct AddonPhaseHandle {
    pub core: PhaseHandle<AddonDone>,
}

/// Never constructed: no job runs without the `addons` feature.
#[cfg(not(feature = "addons"))]
pub(crate) enum AddonDone {}

/// Work an addon slash command hands to the event loop.
#[cfg(feature = "addons")]
#[derive(Debug)]
pub(crate) enum AddonJob {
    /// `/name args` for a command an addon registered.
    Command {
        host: Arc<AddonHost>,
        command: CommandSpec,
        args: String,
    },
    /// `/addons reload` with the configured addon settings.
    Reload {
        settings: crate::config::AddonsConfig,
    },
}

/// What a finished [`AddonJob`] produced.
#[cfg(feature = "addons")]
pub(crate) enum AddonDone {
    Command {
        name: String,
        result: Result<CommandOutput, String>,
    },
    Reload(Result<(Arc<AddonHost>, ReloadReport), String>),
}

#[cfg(feature = "addons")]
impl AddonJob {
    /// Run the job. Blocks on the addon isolate.
    fn run(self) -> AddonDone {
        match self {
            AddonJob::Command {
                host,
                command,
                args,
            } => AddonDone::Command {
                name: command.name.clone(),
                result: host.run_command(&command, &args),
            },
            AddonJob::Reload { settings } => AddonDone::Reload(crate::addons::reload(&settings)),
        }
    }

    /// The command's name, `None` for a reload.
    fn command_name(&self) -> Option<String> {
        match self {
            AddonJob::Command { command, .. } => Some(command.name.clone()),
            AddonJob::Reload { .. } => None,
        }
    }
}

#[cfg(feature = "addons")]
impl AddonDone {
    /// The result of a job whose thread ended with `error` before answering:
    /// command `name`, or a reload when `name` is `None`.
    fn died(name: Option<String>, error: String) -> Self {
        match name {
            Some(name) => AddonDone::Command {
                name,
                result: Err(error),
            },
            None => AddonDone::Reload(Err(error)),
        }
    }
}

/// Start `job` on a blocking thread. Returns at once; the result arrives on
/// the handle's channel.
#[cfg(feature = "addons")]
pub(crate) fn spawn(job: AddonJob) -> AddonPhaseHandle {
    let name = job.command_name();
    let core = PhaseHandle::spawn(1, move |tx| async move {
        let done = tokio::task::spawn_blocking(move || job.run())
            .await
            .unwrap_or_else(|e| AddonDone::died(name, format!("addon task failed: {e}")));
        let _ = tx.send(done).await;
    });
    AddonPhaseHandle { core }
}

/// How a chat line is colored.
#[cfg(feature = "addons")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Agent,
    Result,
    Error,
    Dim,
}

#[cfg(feature = "addons")]
impl Tone {
    pub(crate) fn color(self) -> crossterm::style::Color {
        match self {
            Tone::Agent => crate::ui::theme::agent(),
            Tone::Result => crate::ui::theme::result(),
            Tone::Error => crate::ui::theme::error(),
            Tone::Dim => crate::ui::theme::dim(),
        }
    }
}

/// A line for the chat area.
#[cfg(feature = "addons")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub text: String,
    pub tone: Tone,
}

#[cfg(feature = "addons")]
fn line(text: impl Into<String>, tone: Tone) -> Line {
    Line {
        text: text.into(),
        tone,
    }
}

/// What the event loop does with a finished job: show `lines`, then start a
/// turn on `prompt` when there is one.
#[cfg(feature = "addons")]
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Landing {
    pub lines: Vec<Line>,
    pub prompt: Option<String>,
}

/// Land a finished job. A reload's tools replace the live agent's addon
/// tools, the agent is republished and the addon commands are registered
/// for completion. `None` is a job whose task ended without answering.
#[cfg(feature = "addons")]
pub(crate) fn land(
    done: Option<AddonDone>,
    agent: &mut AnyAgent,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
) -> Landing {
    match done {
        None => failure("addon task ended without an answer".to_string()),
        Some(AddonDone::Command { name, result }) => command_landing(&name, result),
        Some(AddonDone::Reload(Err(error))) => failure(format!("addon reload failed: {error}")),
        Some(AddonDone::Reload(Ok((host, report)))) => {
            let (offered, installed) = adopt(&host, agent, permission, ask_tx);
            reload_landing(&report, &offered, &installed)
        }
    }
}

/// Give the live agent `host`'s addon tools in place of the ones it had,
/// republish it, and register the addon commands for completion: the tools
/// offered, and the ones the agent took.
#[cfg(feature = "addons")]
fn adopt(
    host: &Arc<AddonHost>,
    agent: &mut AnyAgent,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
) -> (Vec<String>, Vec<String>) {
    let tools: Vec<Arc<dyn LoopTool>> =
        crate::addons::tool::loop_tools(host, permission.clone(), ask_tx.clone())
            .into_iter()
            .map(|t| Arc::new(t) as Arc<dyn LoopTool>)
            .collect();
    let offered: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
    let installed = agent.upsert_loop_tools(crate::addons::tool::SOURCE, tools);
    crate::provider::publish_live_agent(agent);
    #[cfg(feature = "slash-completion")]
    crate::ui::slash::register_addon_commands(
        host.commands().into_iter().map(|c| c.name).collect(),
    );
    (offered, installed)
}

/// Resolves when the addons changed in place: a REPL evaluation or
/// `dirge.harness/refresh!` re-read their tools, hooks and commands. Never
/// resolves in a build without the `addons` feature, so the select arm
/// waiting on it can be unconditional.
pub(crate) async fn live_change() {
    #[cfg(feature = "addons")]
    crate::addons::live::changed().await;
    #[cfg(not(feature = "addons"))]
    std::future::pending::<()>().await;
}

/// Land an in-place change of the addons: the live agent takes their tools
/// as they are now. Quiet unless the tools changed, so a REPL session does
/// not fill the chat area.
#[cfg(feature = "addons")]
pub(crate) fn land_live(
    agent: &mut AnyAgent,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
) -> Landing {
    let Some(host) = crate::addons::global() else {
        return Landing::default();
    };
    let Some(report) = host.sync() else {
        return Landing::default();
    };
    let (_, installed) = adopt(&host, agent, permission, ask_tx);
    live_landing(&report, &installed)
}

/// What an in-place change says: the tools it added and removed, nothing
/// when it changed none.
#[cfg(feature = "addons")]
fn live_landing(report: &ReloadReport, installed: &[String]) -> Landing {
    let added: Vec<&str> = report
        .tools_added
        .iter()
        .filter(|name| installed.contains(name))
        .map(String::as_str)
        .collect();
    let mut lines = Vec::new();
    if !added.is_empty() {
        lines.push(line(
            format!("[addons] + tools: {}", added.join(", ")),
            Tone::Dim,
        ));
    }
    if !report.tools_removed.is_empty() {
        lines.push(line(
            format!("[addons] - tools: {}", report.tools_removed.join(", ")),
            Tone::Dim,
        ));
    }
    Landing {
        lines,
        prompt: None,
    }
}

#[cfg(feature = "addons")]
fn failure(message: String) -> Landing {
    Landing {
        lines: vec![line(message, Tone::Error)],
        prompt: None,
    }
}

/// A command's answer: its text as chat lines, control sequences stripped,
/// and its prompt.
#[cfg(feature = "addons")]
fn command_landing(name: &str, result: Result<CommandOutput, String>) -> Landing {
    let output = match result {
        Ok(output) => output,
        Err(error) => return failure(format!("[addon] /{name} failed: {error}")),
    };
    let lines = output
        .text
        .map(|text| {
            crate::ui::ansi::strip_escapes(&text, crate::ui::ansi::StripPolicy::KEEP_NEWLINE)
                .lines()
                .map(|l| line(l, Tone::Agent))
                .collect()
        })
        .unwrap_or_default();
    Landing {
        lines,
        prompt: output.prompt,
    }
}

/// A reload's report. `offered` are the tools the reloaded host offers and
/// `installed` the ones the agent took; only installed tools count as
/// added, and the rest are listed as skipped.
#[cfg(feature = "addons")]
fn reload_landing(report: &ReloadReport, offered: &[String], installed: &[String]) -> Landing {
    let mut lines = vec![line(
        format!(
            "reloaded {} addon(s): {}",
            report.loaded.len(),
            report.loaded.join(", ")
        ),
        Tone::Agent,
    )];
    let added: Vec<&str> = report
        .tools_added
        .iter()
        .filter(|name| installed.contains(name))
        .map(String::as_str)
        .collect();
    if !added.is_empty() {
        lines.push(line(
            format!("  + tools: {}", added.join(", ")),
            Tone::Result,
        ));
    }
    if !report.tools_removed.is_empty() {
        lines.push(line(
            format!("  - tools: {}", report.tools_removed.join(", ")),
            Tone::Result,
        ));
    }
    let skipped: Vec<&str> = offered
        .iter()
        .filter(|name| !installed.contains(name))
        .map(String::as_str)
        .collect();
    if !skipped.is_empty() {
        lines.push(line(
            format!("  skipped (name taken): {}", skipped.join(", ")),
            Tone::Error,
        ));
    }
    for failure in report.failures.iter().chain(&report.source_errors) {
        lines.push(line(
            format!("  {}: {}", failure.manifest.display(), failure.error),
            Tone::Error,
        ));
    }
    lines.push(line(
        "  tools and hooks take effect at the next prompt",
        Tone::Dim,
    ));
    Landing {
        lines,
        prompt: None,
    }
}

#[cfg(all(test, feature = "addons"))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;
    use crate::addons::domain::{HookPoint, HookReply, LoadFailure};
    use crate::addons::port::AddonRuntime;
    use crate::permission::ask::{AskRequest, UserDecision};

    /// Runtime whose command handler waits on a permission answer, as one
    /// that calls a permission-gated tool through `call-tool` does.
    struct AskingRuntime {
        asks: AskSender,
    }

    impl AddonRuntime for AskingRuntime {
        fn load(&self, _manifest: &Path, _host_config: &Value) -> Value {
            Value::Null
        }
        fn unload(&self, _addon_id: &str) {}
        fn reload_sources(&self, _files: &[PathBuf]) -> Vec<(PathBuf, String)> {
            Vec::new()
        }
        fn set_source_roots(&self, _roots: &[PathBuf]) {}
        fn call_tool(&self, _addon_id: &str, _tool: &str, _args: &Value) -> Result<Value, String> {
            Err("no tools".into())
        }
        fn run_command(&self, _addon_id: &str, _name: &str, ctx: &Value) -> Result<Value, String> {
            let (reply, answer) = tokio::sync::oneshot::channel();
            self.asks
                .blocking_send(AskRequest {
                    tool: "bash".into(),
                    input: ctx["args"].as_str().unwrap_or_default().to_string(),
                    details: None,
                    reason: None,
                    reply,
                })
                .map_err(|e| e.to_string())?;
            Ok(match answer.blocking_recv() {
                Ok(UserDecision::AllowOnce) => json!({"text": "ran", "prompt": "summarize it"}),
                _ => json!({"text": "denied"}),
            })
        }
        fn run_hook(&self, _point: HookPoint, _ctx: &Value) -> Vec<HookReply> {
            Vec::new()
        }
        fn shutdown(&self) {}
    }

    fn command(name: &str) -> CommandSpec {
        CommandSpec {
            addon_id: "a".into(),
            name: name.into(),
            description: String::new(),
        }
    }

    /// The test runtime is single-threaded like dirge's: the prompt raised by
    /// a running command is received and answered on the same thread that
    /// started the command.
    #[tokio::test]
    async fn a_permission_prompt_raised_by_a_running_command_is_answered() {
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(4);
        let host = Arc::new(AddonHost::new(
            Arc::new(AskingRuntime { asks: ask_tx }),
            Vec::new(),
            Vec::new(),
        ));
        let mut phase = spawn(AddonJob::Command {
            host,
            command: command("ls"),
            args: "src".into(),
        });

        let ask = tokio::time::timeout(Duration::from_secs(10), ask_rx.recv())
            .await
            .expect("the prompt arrives while the command runs")
            .expect("the command asked");
        assert_eq!((ask.tool.as_str(), ask.input.as_str()), ("bash", "src"));
        ask.reply.send(UserDecision::AllowOnce).unwrap();

        let done = tokio::time::timeout(Duration::from_secs(10), phase.core.rx.recv())
            .await
            .expect("the command finishes once answered");
        let Some(AddonDone::Command { name, result }) = done else {
            panic!("a command result");
        };
        assert_eq!(name, "ls");
        let output = result.unwrap();
        assert_eq!(output.text.as_deref(), Some("ran"));
        assert_eq!(output.prompt.as_deref(), Some("summarize it"));
    }

    fn texts(landing: &Landing) -> Vec<(&str, Tone)> {
        landing
            .lines
            .iter()
            .map(|l| (l.text.as_str(), l.tone))
            .collect()
    }

    #[test]
    fn command_text_is_shown_stripped_and_its_prompt_starts_a_turn() {
        let landing = command_landing(
            "rows",
            Ok(CommandOutput {
                text: Some("one\n\u{1b}[31mtwo\u{1b}[0m".into()),
                prompt: Some("go on".into()),
            }),
        );
        assert_eq!(
            texts(&landing),
            vec![("one", Tone::Agent), ("two", Tone::Agent)]
        );
        assert_eq!(landing.prompt.as_deref(), Some("go on"));
    }

    #[test]
    fn a_failed_command_says_why_and_starts_no_turn() {
        let landing = command_landing("rows", Err("handler threw".into()));
        assert_eq!(
            texts(&landing),
            vec![("[addon] /rows failed: handler threw", Tone::Error)]
        );
        assert_eq!(landing.prompt, None);
    }

    #[test]
    fn a_silent_command_shows_nothing() {
        assert_eq!(
            command_landing("rows", Ok(CommandOutput::default())),
            Landing::default()
        );
    }

    #[test]
    fn a_reload_counts_only_installed_tools_as_added_and_lists_the_rest_as_skipped() {
        let report = ReloadReport {
            loaded: vec!["a".into(), "b".into()],
            failures: vec![LoadFailure {
                manifest: PathBuf::from("c.edn"),
                error: "init-fn not found".into(),
            }],
            source_errors: Vec::new(),
            tools_added: vec!["bash".into(), "count_rows".into()],
            tools_removed: vec!["old".into()],
        };
        let offered = ["bash".to_string(), "count_rows".into(), "kept".into()];
        let installed = ["count_rows".to_string(), "kept".into()];

        let landing = reload_landing(&report, &offered, &installed);

        assert_eq!(
            texts(&landing),
            vec![
                ("reloaded 2 addon(s): a, b", Tone::Agent),
                ("  + tools: count_rows", Tone::Result),
                ("  - tools: old", Tone::Result),
                ("  skipped (name taken): bash", Tone::Error),
                ("  c.edn: init-fn not found", Tone::Error),
                (
                    "  tools and hooks take effect at the next prompt",
                    Tone::Dim
                ),
            ]
        );
        assert_eq!(landing.prompt, None);
    }

    #[test]
    fn a_job_whose_thread_died_reports_as_its_own_kind() {
        let reload = AddonJob::Reload {
            settings: Default::default(),
        };
        assert!(matches!(
            AddonDone::died(reload.command_name(), "boom".into()),
            AddonDone::Reload(Err(e)) if e == "boom"
        ));
        let (asks, _) = tokio::sync::mpsc::channel(1);
        let host = Arc::new(AddonHost::new(
            Arc::new(AskingRuntime { asks }),
            Vec::new(),
            Vec::new(),
        ));
        let job = AddonJob::Command {
            host,
            command: command("rows"),
            args: String::new(),
        };
        assert!(matches!(
            AddonDone::died(job.command_name(), "boom".into()),
            AddonDone::Command { name, result: Err(e) } if name == "rows" && e == "boom"
        ));
    }
}
