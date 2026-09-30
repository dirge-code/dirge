use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::boundary::{HookRunner, ShellRunner};
use super::domain::{
    Exited, HookCommand, HookError, HookEvent, HookMatcher, HookOutcome, HooksConfig, Submission,
};
use super::loop_hooks;
use super::{CommandHooks, HookBinding, dialect, policy};
use crate::agent::agent_loop::hooks::{BeforeToolCallContext, BeforeToolCallFn};
use crate::agent::agent_loop::message::{AssistantMessage, StopReason};
use crate::permission::ask::UserDecision;

// ---------------------------------------------------------------- stubs

/// Answers every command from a script keyed by command text, recording
/// each payload it was handed.
#[derive(Default)]
struct ScriptedRunner {
    answers: Vec<(String, Result<Exited, HookError>)>,
    seen: Mutex<Vec<(String, Value)>>,
}

impl ScriptedRunner {
    fn answering(answers: Vec<(&str, Result<Exited, HookError>)>) -> Arc<Self> {
        Arc::new(Self {
            answers: answers
                .into_iter()
                .map(|(c, a)| (c.to_string(), a))
                .collect(),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn seen(&self) -> Vec<(String, Value)> {
        self.seen.lock().unwrap().clone()
    }
}

impl HookRunner for ScriptedRunner {
    fn run(&self, cmd: &HookCommand, payload: &str, _: &Path) -> Result<Exited, HookError> {
        self.seen
            .lock()
            .unwrap()
            .push((cmd.command.clone(), serde_json::from_str(payload).unwrap()));
        self.answers
            .iter()
            .find(|(c, _)| *c == cmd.command)
            .map(|(_, a)| a.clone())
            .unwrap_or(Ok(exit(0, "", "")))
    }
}

fn exit(code: i32, stdout: &str, stderr: &str) -> Exited {
    Exited {
        code: Some(code),
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

fn cmd(command: &str) -> HookCommand {
    HookCommand {
        kind: "command".into(),
        command: command.into(),
        timeout: Some(5),
        addon: None,
        handler: None,
    }
}

fn config(entries: &[(HookEvent, Option<&str>, &[&str])]) -> HooksConfig {
    let mut cfg = HooksConfig::new();
    for (event, matcher, commands) in entries {
        cfg.entry(event.as_str().to_string())
            .or_default()
            .push(HookMatcher {
                matcher: matcher.map(str::to_string),
                hooks: commands.iter().map(|c| cmd(c)).collect(),
            });
    }
    cfg
}

fn registry(cfg: HooksConfig, runner: Arc<dyn HookRunner>) -> Arc<CommandHooks> {
    Arc::new(CommandHooks::new(cfg, PathBuf::from("/proj"), runner))
}

fn deny_json(reason: &str) -> String {
    json!({ "hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": reason,
    }})
    .to_string()
}

fn warn_json(context: &str) -> String {
    json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "additionalContext": context } })
        .to_string()
}

fn before_ctx(tool: &str, args: Value) -> BeforeToolCallContext {
    BeforeToolCallContext {
        assistant_message: AssistantMessage::new(Vec::new(), StopReason::ToolUse),
        tool_call_id: "call-1".into(),
        tool_call_name: tool.into(),
        args,
    }
}

// --------------------------------------------------------------- policy

#[test]
fn matcher_follows_claude_semantics() {
    assert!(policy::matcher_accepts(None, "Bash"));
    assert!(policy::matcher_accepts(Some("*"), "Bash"));
    assert!(policy::matcher_accepts(Some(""), "Bash"));
    assert!(policy::matcher_accepts(Some("Bash"), "Bash"));
    assert!(!policy::matcher_accepts(Some("Bash"), "BashOutput"));
    assert!(policy::matcher_accepts(Some("Edit|Write"), "Write"));
    assert!(policy::matcher_accepts(
        Some("mcp__.*"),
        "mcp__github__create_issue"
    ));
    assert!(!policy::matcher_accepts(Some("Edit|Write"), "Read"));
    assert!(policy::matcher_accepts(Some("(unclosed"), "(unclosed"));
}

#[test]
fn exit_two_blocks_with_stderr() {
    let out = policy::interpret(HookEvent::PreToolUse, exit(2, "", "  nope \n")).unwrap();
    assert_eq!(out.block.as_deref(), Some("nope"));
}

#[test]
fn deny_decision_blocks_with_its_reason() {
    let out = policy::interpret(HookEvent::PreToolUse, exit(0, &deny_json("REFUSED"), "")).unwrap();
    assert_eq!(out.block.as_deref(), Some("REFUSED"));
}

#[test]
fn additional_context_is_carried_and_never_blocks() {
    let out = policy::interpret(
        HookEvent::PreToolUse,
        exit(0, &warn_json("policy: prefer rg"), ""),
    )
    .unwrap();
    assert_eq!(out.block, None);
    assert_eq!(out.context, vec!["policy: prefer rg".to_string()]);
}

#[test]
fn top_level_block_decision_blocks() {
    let answer = json!({ "decision": "block", "reason": "keep going" }).to_string();
    let out = policy::interpret(HookEvent::Stop, exit(0, &answer, "")).unwrap();
    assert_eq!(out.block.as_deref(), Some("keep going"));
}

#[test]
fn plain_stdout_is_context_only_for_context_events() {
    let start = policy::interpret(HookEvent::SessionStart, exit(0, "hello\n", "")).unwrap();
    assert_eq!(start.context, vec!["hello".to_string()]);
    let pre = policy::interpret(HookEvent::PreToolUse, exit(0, "hello\n", "")).unwrap();
    assert_eq!(pre, HookOutcome::default());
}

#[test]
fn other_exit_codes_are_failures() {
    let err = policy::interpret(HookEvent::PreToolUse, exit(1, "", "boom")).unwrap_err();
    assert!(matches!(err, HookError::NonZeroExit { code: Some(1), .. }));
}

#[test]
fn updated_input_is_read() {
    let answer =
        json!({ "hookSpecificOutput": { "updatedInput": { "command": "ls" } } }).to_string();
    let out = policy::interpret(HookEvent::PreToolUse, exit(0, &answer, "")).unwrap();
    assert_eq!(out.updated_input, Some(json!({ "command": "ls" })));
}

#[test]
fn outcomes_combine_first_block_wins_contexts_concatenate() {
    let a = HookOutcome::blocked("first").combine(HookOutcome::with_context("x"));
    let b = a
        .combine(HookOutcome::blocked("second"))
        .combine(HookOutcome::with_context("y"));
    assert_eq!(b.block.as_deref(), Some("first"));
    assert_eq!(b.context_text().as_deref(), Some("x\n\ny"));
}

#[test]
fn settings_hooks_parse_and_absent_block_is_empty() {
    let text = json!({ "hooks": { "PreToolUse": [
        { "matcher": "*", "hooks": [{ "type": "command", "command": "~/.claude/hooks/guard.sh", "timeout": 10 }] }
    ]}, "theme": "dark" })
    .to_string();
    let cfg = policy::parse_settings_hooks("s.json", &text).unwrap();
    assert_eq!(
        cfg["PreToolUse"][0].hooks[0].command,
        "~/.claude/hooks/guard.sh"
    );
    assert_eq!(cfg["PreToolUse"][0].hooks[0].timeout, Some(10));
    assert!(
        policy::parse_settings_hooks("s.json", "{}")
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        policy::parse_settings_hooks("s.json", "{ broken"),
        Err(HookError::Unreadable { .. })
    ));
}

#[test]
fn normalize_drops_non_command_and_blank_entries() {
    let mut cfg = config(&[(HookEvent::Stop, None, &["  "])]);
    cfg.entry("PreToolUse".into())
        .or_default()
        .push(HookMatcher {
            matcher: None,
            hooks: vec![HookCommand {
                kind: "prompt".into(),
                command: "x".into(),
                timeout: None,
                addon: None,
                handler: None,
            }],
        });
    assert!(policy::normalize(cfg).is_empty());
}

// ------------------------------------------------------- addon entries

fn addon_cmd(addon: &str, handler: &str) -> HookCommand {
    HookCommand {
        kind: "addon".into(),
        command: String::new(),
        timeout: Some(5),
        addon: Some(addon.into()),
        handler: Some(handler.into()),
    }
}

fn one_entry(event: HookEvent, hook: HookCommand) -> HooksConfig {
    let mut cfg = HooksConfig::new();
    cfg.insert(
        event.as_str().to_string(),
        vec![HookMatcher {
            matcher: None,
            hooks: vec![hook],
        }],
    );
    cfg
}

#[test]
fn addon_entries_parse_and_survive_normalize() {
    let text = r#"{"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
        {"type": "addon", "addon": "hive.dirge", "handler": "guard", "timeout": 10},
        {"type": "addon", "addon": "hive.dirge"},
        {"type": "addon", "addon": " ", "handler": "guard"}
    ]}]}}"#;
    let cfg = policy::normalize(policy::parse_settings_hooks("t", text).unwrap());
    let hooks = &cfg["PreToolUse"][0].hooks;
    assert_eq!(
        hooks.len(),
        1,
        "an addon entry needs both addon and handler"
    );
    assert_eq!(hooks[0].addon_target(), Some(("hive.dirge", "guard")));
    assert_eq!(hooks[0].timeout_secs(), 10);
    assert_eq!(hooks[0].label(), "addon:hive.dirge/guard");
}

#[test]
fn addon_answers_read_as_the_process_they_stand_in_for() {
    let answer = json!({"hookSpecificOutput": {"permissionDecision": "deny"}});
    assert_eq!(
        policy::addon_answer(&answer),
        exit(0, &answer.to_string(), "")
    );
    assert_eq!(
        policy::addon_answer(&json!({"exit": 2, "stderr": "no"})),
        exit(2, "", "no")
    );
    assert_eq!(policy::addon_answer(&Value::Null), exit(0, "", ""));
    assert_eq!(policy::addon_answer(&json!("ctx")), exit(0, "ctx", ""));
    assert_eq!(policy::addon_answer(&json!({})), exit(0, "{}", ""));
}

/// Stands in for the live addon runner: the handler's value, read by
/// `policy::addon_answer`, or an error as the live runner reports it.
struct ScriptedAddon(Result<Value, HookError>);

impl HookRunner for ScriptedAddon {
    fn run(&self, cmd: &HookCommand, _: &str, _: &Path) -> Result<Exited, HookError> {
        assert!(cmd.addon_target().is_some());
        self.0.clone().map(|v| policy::addon_answer(&v))
    }
}

fn dispatch(addon: Option<Arc<dyn HookRunner>>) -> Arc<dyn HookRunner> {
    Arc::new(super::boundary::DispatchRunner::new(
        Arc::new(ShellRunner),
        Arc::new(move || addon.clone()),
    ))
}

#[test]
fn an_addon_entry_without_an_addon_runner_fails_open() {
    let hooks = registry(
        one_entry(HookEvent::PreToolUse, addon_cmd("hive.dirge", "guard")),
        dispatch(None),
    );
    let out = hooks.run(HookEvent::PreToolUse, &["Bash"], &json!({}));
    assert_eq!(out, HookOutcome::default());
}

#[test]
fn dispatch_sends_command_entries_to_the_shell() {
    let hooks = registry(
        one_entry(HookEvent::PreToolUse, cmd("echo nope >&2; exit 2")),
        dispatch(Some(Arc::new(ScriptedAddon(Ok(Value::Null))))),
    );
    let out = hooks.run(HookEvent::PreToolUse, &["Bash"], &json!({}));
    assert_eq!(out, HookOutcome::blocked("nope"));
}

/// The same answer through `sh` and through an addon lands on the same
/// outcome: one decoder, `policy::interpret`, reads both.
#[test]
fn addon_and_shell_runners_agree_on_every_answer() {
    let deny = deny_json("rule R1");
    let warn = warn_json("careful");
    let stop = json!({"decision": "block", "reason": "not yet"}).to_string();
    let cases: Vec<(HookEvent, String, Result<Value, HookError>)> = vec![
        (
            HookEvent::PreToolUse,
            format!("printf '%s' '{deny}'"),
            Ok(serde_json::from_str(&deny).unwrap()),
        ),
        (
            HookEvent::PreToolUse,
            format!("printf '%s' '{warn}'"),
            Ok(serde_json::from_str(&warn).unwrap()),
        ),
        (
            HookEvent::Stop,
            format!("printf '%s' '{stop}'"),
            Ok(serde_json::from_str(&stop).unwrap()),
        ),
        (HookEvent::PreToolUse, "printf '{}'".into(), Ok(json!({}))),
        (HookEvent::PreToolUse, "true".into(), Ok(Value::Null)),
        (
            HookEvent::PreToolUse,
            "echo blocked >&2; exit 2".into(),
            Ok(json!({"exit": 2, "stderr": "blocked\n"})),
        ),
        (
            HookEvent::PreToolUse,
            "echo broken >&2; exit 3".into(),
            Ok(json!({"exit": 3, "stderr": "broken\n"})),
        ),
        (
            HookEvent::SessionStart,
            "printf 'plain context'".into(),
            Ok(json!("plain context")),
        ),
        (
            HookEvent::PreToolUse,
            "sleep 5".into(),
            Err(HookError::TimedOut(1)),
        ),
    ];
    for (event, shell, addon) in cases {
        let mut shell_cmd = cmd(&shell);
        shell_cmd.timeout = Some(1);
        let via_shell =
            registry(one_entry(event, shell_cmd), dispatch(None)).run(event, &["Bash"], &json!({}));
        let via_addon = registry(
            one_entry(event, addon_cmd("hive.dirge", "guard")),
            dispatch(Some(Arc::new(ScriptedAddon(addon)))),
        )
        .run(event, &["Bash"], &json!({}));
        assert_eq!(via_addon, via_shell, "{event} `{shell}`");
    }
}

#[test]
fn payload_carries_claude_envelope() {
    let p = policy::payload(
        HookEvent::PreToolUse,
        Some("s1"),
        "/w",
        json!({ "tool_name": "Bash" }),
    );
    assert_eq!(p["hook_event_name"], "PreToolUse");
    assert_eq!(p["session_id"], "s1");
    assert_eq!(p["cwd"], "/w");
    assert_eq!(p["harness"], "dirge");
    assert_eq!(p["tool_name"], "Bash");
}

// -------------------------------------------------------------- dialect

#[test]
fn tool_names_restate_in_claude_vocabulary() {
    assert_eq!(dialect::claude_tool_name("bash"), "Bash");
    assert_eq!(dialect::claude_tool_name("edit_lines"), "Edit");
    assert_eq!(dialect::claude_tool_name("find_files"), "Glob");
    assert_eq!(
        dialect::claude_tool_name("mcp__github__create_issue"),
        "mcp__github__create_issue"
    );
}

#[test]
fn file_tool_input_gains_absolute_file_path_and_claude_keys() {
    let input = dialect::claude_input(
        "edit",
        &json!({ "path": "src/a.rs", "old_text": "x", "new_text": "y" }),
        Path::new("/w"),
    );
    assert_eq!(input["file_path"], "/w/src/a.rs");
    assert_eq!(input["old_string"], "x");
    assert_eq!(input["new_string"], "y");
    assert_eq!(input["path"], "src/a.rs", "dirge keys are kept");
}

#[test]
fn grep_path_is_not_a_file_path() {
    let input = dialect::claude_input(
        "grep",
        &json!({ "pattern": "x", "path": "src" }),
        Path::new("/w"),
    );
    assert!(input.get("file_path").is_none());
}

#[test]
fn apply_patch_splits_into_one_call_per_operation() {
    let args = json!({ "operations": [
        { "action": "create", "path": "a.rs", "content": "c" },
        { "action": "update", "path": "/abs/b.rs", "old_text": "o", "new_text": "n" },
    ]});
    let calls = dialect::claude_calls("apply_patch", &args, Path::new("/w"));
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "Write");
    assert_eq!(calls[0].1["file_path"], "/w/a.rs");
    assert_eq!(calls[1].0, "Edit");
    assert_eq!(calls[1].1["old_string"], "o");
}

#[test]
fn updated_input_folds_back_onto_dirge_keys() {
    let original = json!({ "path": "a.rs", "old_text": "x", "new_text": "y" });
    let updated =
        json!({ "file_path": "/w/b.rs", "path": "a.rs", "old_string": "x", "new_string": "z" });
    let args = dialect::dirge_args_from_claude("edit", &original, &updated);
    assert_eq!(args["path"], "/w/b.rs");
    assert_eq!(args["new_text"], "z");
    assert!(args.get("file_path").is_none());
}

// ------------------------------------------------------------- registry

#[test]
fn registry_folds_commands_and_fails_open() {
    let runner = ScriptedRunner::answering(vec![
        ("broken", Err(HookError::SpawnFailed("no such file".into()))),
        ("warn", Ok(exit(0, &warn_json("heads up"), ""))),
        ("deny", Ok(exit(0, &deny_json("no"), ""))),
    ]);
    let hooks = registry(
        config(&[(
            HookEvent::PreToolUse,
            Some("Bash"),
            &["broken", "warn", "deny"],
        )]),
        runner.clone(),
    );
    let out = hooks.run(HookEvent::PreToolUse, &["Bash"], &json!({}));
    assert_eq!(out.block.as_deref(), Some("no"));
    assert_eq!(out.context, vec!["heads up".to_string()]);
    assert_eq!(runner.seen().len(), 3);
}

#[test]
fn registry_respects_matchers() {
    let runner = ScriptedRunner::answering(vec![]);
    let hooks = registry(
        config(&[
            (HookEvent::PreToolUse, Some("Edit|Write"), &["files"]),
            (HookEvent::PreToolUse, Some("*"), &["all"]),
        ]),
        runner.clone(),
    );
    hooks.run(HookEvent::PreToolUse, &["Bash", "bash"], &json!({}));
    let ran: Vec<String> = runner.seen().into_iter().map(|(c, _)| c).collect();
    assert_eq!(ran, vec!["all".to_string()]);
}

#[test]
fn session_start_runs_once_per_session() {
    let runner = ScriptedRunner::answering(vec![("start", Ok(exit(0, "ctx", "")))]);
    let hooks = registry(
        config(&[(HookEvent::SessionStart, None, &["start"])]),
        runner.clone(),
    );
    assert_eq!(
        hooks.session_start_context(Some("s"), false).as_deref(),
        Some("ctx")
    );
    assert_eq!(
        hooks.session_start_context(Some("s"), false).as_deref(),
        Some("ctx")
    );
    assert_eq!(runner.seen().len(), 1);
    assert_eq!(runner.seen()[0].1["source"], "startup");
}

#[test]
fn claude_settings_are_read_only_when_enabled() {
    let dir = std::env::temp_dir().join(format!("dirge-command-hooks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let home = dir.join("home");
    let project = dir.join("proj");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(project.join(".claude")).unwrap();
    let block = json!({ "hooks": { "PreToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "g" }] }] } });
    std::fs::write(home.join(".claude/settings.json"), block.to_string()).unwrap();
    std::fs::write(project.join(".claude/settings.local.json"), "{ not json").unwrap();

    let runner: Arc<dyn HookRunner> = ScriptedRunner::answering(vec![]);
    let off = CommandHooks::from_sources(None, false, project.clone(), Some(&home), runner.clone());
    assert!(off.is_empty());
    let on = CommandHooks::from_sources(None, true, project, Some(&home), runner);
    assert_eq!(on.configured_events(), vec![HookEvent::PreToolUse]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ----------------------------------------------------------- loop hooks

#[tokio::test(flavor = "multi_thread")]
async fn pre_tool_hook_blocks_in_claude_terms() {
    let runner = ScriptedRunner::answering(vec![("guard", Ok(exit(0, &deny_json("REFUSED"), "")))]);
    let hooks = registry(
        config(&[(HookEvent::PreToolUse, Some("Read"), &["guard"])]),
        runner.clone(),
    );
    let hook = loop_hooks::pre_tool_hook(HookBinding::main(hooks), Some("s1".into()));

    let ret = hook(before_ctx("read", json!({ "path": "/w/a.clj" }))).await;
    let result = ret.result.expect("blocked");
    assert_eq!(result.block, Some(true));
    assert_eq!(
        result.reason.as_deref(),
        Some("PreToolUse:Read hook error: REFUSED")
    );

    let (_, payload) = &runner.seen()[0];
    assert_eq!(payload["tool_name"], "Read");
    assert_eq!(payload["tool_input"]["file_path"], "/w/a.clj");
    assert_eq!(payload["session_id"], "s1");
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_tool_hook_warning_rides_as_context() {
    let runner =
        ScriptedRunner::answering(vec![("guard", Ok(exit(0, &warn_json("prefer rg"), "")))]);
    let hooks = registry(config(&[(HookEvent::PreToolUse, None, &["guard"])]), runner);
    let hook = loop_hooks::pre_tool_hook(HookBinding::main(hooks), None);
    let ret = hook(before_ctx("bash", json!({ "command": "ls" }))).await;
    assert!(ret.result.is_none());
    assert_eq!(ret.context.len(), 1);
    assert!(ret.context[0].contains("prefer rg"));
}

fn ask_json(reason: &str) -> String {
    json!({ "hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "ask",
        "permissionDecisionReason": reason,
    }})
    .to_string()
}

type SeenAsk = (String, String, Option<String>);

/// A permission prompt that gives every ask the same answer, recording
/// the tool, input and reason it was shown.
fn prompt_answering(
    decision: UserDecision,
) -> (crate::permission::ask::AskSender, Arc<Mutex<Vec<SeenAsk>>>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::permission::ask::AskRequest>(4);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            log.lock()
                .unwrap()
                .push((req.tool.clone(), req.input.clone(), req.reason.clone()));
            let _ = req.reply.send(decision.clone());
        }
    });
    (tx, seen)
}

fn asking_hook(ask: Option<crate::permission::ask::AskSender>) -> BeforeToolCallFn {
    let runner = ScriptedRunner::answering(vec![("guard", Ok(exit(0, &ask_json("risky"), "")))]);
    let hooks = registry(config(&[(HookEvent::PreToolUse, None, &["guard"])]), runner);
    loop_hooks::pre_tool_hook(HookBinding::main(hooks).with_ask(ask), None)
}

#[test]
fn ask_decision_asks_with_its_reason() {
    let out = policy::interpret(HookEvent::PreToolUse, exit(0, &ask_json("risky"), "")).unwrap();
    assert_eq!(out.ask.as_deref(), Some("risky"));
    assert_eq!(out.block, None);
    assert!(out.context.is_empty());
}

#[test]
fn a_block_outranks_an_ask_and_the_first_ask_wins() {
    let asked = HookOutcome::asked("first").combine(HookOutcome::asked("second"));
    assert_eq!(asked.pending_ask(), Some("first"));
    let blocked = asked.combine(HookOutcome::blocked("no"));
    assert_eq!(blocked.pending_ask(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_tool_ask_allowed_at_the_prompt_runs_the_call() {
    let (tx, seen) = prompt_answering(UserDecision::AllowOnce);
    let ret = asking_hook(Some(tx))(before_ctx("bash", json!({ "command": "rm -rf build" }))).await;
    assert!(ret.result.is_none());
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "Bash");
    assert_eq!(seen[0].1, "rm -rf build");
    assert!(seen[0].2.as_deref().unwrap().contains("risky"));
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_tool_ask_denied_at_the_prompt_blocks_with_the_note() {
    let (tx, _) = prompt_answering(UserDecision::Deny {
        note: Some("use make clean".into()),
    });
    let ret = asking_hook(Some(tx))(before_ctx("bash", json!({ "command": "rm -rf build" }))).await;
    let result = ret.result.expect("blocked");
    assert_eq!(result.block, Some(true));
    assert_eq!(
        result.reason.as_deref(),
        Some("PreToolUse:Bash hook error: risky; the user denied it: use make clean")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_tool_ask_without_a_prompt_blocks() {
    let ret = asking_hook(None)(before_ctx("bash", json!({ "command": "ls" }))).await;
    let reason = ret.result.expect("blocked").reason.unwrap();
    assert!(reason.contains("risky"));
    assert!(reason.contains("no permission prompt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn compose_before_short_circuits_on_first_block() {
    let runner = ScriptedRunner::answering(vec![]);
    let hooks = registry(
        config(&[(HookEvent::PreToolUse, None, &["second"])]),
        runner.clone(),
    );
    let first: crate::agent::agent_loop::hooks::BeforeToolCallFn = Arc::new(|ctx| {
        Box::pin(async move {
            crate::agent::agent_loop::hooks::BeforeToolCallReturn {
                result: Some(crate::agent::agent_loop::result::BeforeToolCallResult {
                    block: Some(true),
                    reason: Some("plugin".into()),
                }),
                args: ctx.args,
                context: Vec::new(),
            }
        })
    });
    let composed = loop_hooks::compose_before(
        Some(first),
        loop_hooks::pre_tool_hook(HookBinding::main(hooks), None),
    );
    let ret = composed(before_ctx("bash", json!({ "command": "ls" }))).await;
    assert_eq!(ret.result.unwrap().reason.as_deref(), Some("plugin"));
    assert!(runner.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_block_feeds_back_then_lets_go() {
    let runner = ScriptedRunner::answering(vec![(
        "stop",
        Ok(exit(
            0,
            &json!({ "decision": "block", "reason": "run the tests" }).to_string(),
            "",
        )),
    )]);
    let hooks = registry(
        config(&[(HookEvent::Stop, None, &["stop"])]),
        runner.clone(),
    );
    let follow = loop_hooks::stop_followup(HookBinding::main(hooks), None);

    let first = follow().await;
    assert_eq!(first.len(), 1);
    assert_eq!(runner.seen()[0].1["stop_hook_active"], false);
    let _ = follow().await;
    assert_eq!(runner.seen()[1].1["stop_hook_active"], true);
    for _ in 2..loop_hooks::MAX_CONSECUTIVE_STOP_BLOCKS {
        assert_eq!(follow().await.len(), 1);
    }
    assert!(
        follow().await.is_empty(),
        "the streak cap releases the loop"
    );
}

#[test]
fn subagent_binding_stops_on_subagent_stop() {
    let hooks = registry(HooksConfig::new(), ScriptedRunner::answering(vec![]));
    assert_eq!(
        HookBinding::subagent(hooks.clone()).stop_event,
        HookEvent::SubagentStop
    );
    assert_eq!(HookBinding::main(hooks).stop_event, HookEvent::Stop);
}

// ------------------------------------------------------- shell adapter

#[test]
fn shell_runner_feeds_stdin_and_reads_exit() {
    let echoed = ShellRunner
        .run(
            &cmd("cat; echo oops >&2; exit 2"),
            r#"{"a":1}"#,
            Path::new("/tmp"),
        )
        .unwrap();
    assert_eq!(echoed.code, Some(2));
    assert_eq!(echoed.stdout, r#"{"a":1}"#);
    assert_eq!(echoed.stderr.trim(), "oops");
}

#[test]
fn shell_runner_exposes_project_dir() {
    let out = ShellRunner
        .run(
            &cmd("printf %s \"$CLAUDE_PROJECT_DIR\""),
            "{}",
            Path::new("/some/proj"),
        )
        .unwrap();
    assert_eq!(out.stdout, "/some/proj");
}

#[test]
fn shell_runner_times_out() {
    let mut slow = cmd("sleep 5");
    slow.timeout = Some(1);
    assert_eq!(
        ShellRunner.run(&slow, "{}", Path::new("/tmp")),
        Err(HookError::TimedOut(1))
    );
}

// ------------------------------------------------------ prompt submission

#[test]
fn submission_passes_the_prompt_through_when_no_hook_speaks() {
    assert_eq!(
        policy::submission(HookOutcome::default(), "hello".into()),
        Submission::Proceed("hello".into())
    );
}

#[test]
fn submission_prepends_context() {
    let Submission::Proceed(text) =
        policy::submission(HookOutcome::with_context("ticket 42"), "hello".into())
    else {
        panic!("context alone must not block");
    };
    assert!(text.contains("ticket 42"), "{text}");
    assert!(text.ends_with("hello"), "the prompt stays last: {text}");
}

#[test]
fn submission_block_wins_over_context_and_drops_the_prompt() {
    let outcome =
        HookOutcome::with_context("ticket 42").combine(HookOutcome::blocked("no secrets"));
    let Submission::Blocked(message) = policy::submission(outcome, "my password is x".into())
    else {
        panic!("a block must stop the run");
    };
    assert!(
        message.contains("UserPromptSubmit") && message.contains("no secrets"),
        "{message}"
    );
    assert!(
        !message.contains("my password is x"),
        "a blocked prompt is not echoed back: {message}"
    );
}

#[test]
fn a_blocking_prompt_hook_yields_blocked_not_a_rewritten_prompt() {
    let runner = ScriptedRunner::answering(vec![("gate", Ok(exit(2, "", "no secrets")))]);
    let hooks = registry(
        config(&[(HookEvent::UserPromptSubmit, None, &["gate"])]),
        runner.clone(),
    );
    let submitted = loop_hooks::submitted_prompt(&hooks, Some("s-1"), "my password is x".into());
    assert!(matches!(submitted, Submission::Blocked(ref m) if m.contains("no secrets")));
    assert_eq!(runner.seen()[0].1["prompt"], "my password is x");
}
