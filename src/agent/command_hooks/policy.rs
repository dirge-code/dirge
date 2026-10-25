//! L1 calculations: pure functions over the domain. No I/O.

use serde_json::{Map, Value, json};

use super::domain::{
    Exited, HookError, HookEvent, HookMatcher, HookOutcome, HooksConfig, Submission,
    system_reminder,
};

/// Claude Code matcher semantics: absent, empty or `*` accepts all;
/// otherwise a regex that must match the whole target, falling back to
/// literal equality when the pattern does not compile.
pub fn matcher_accepts(matcher: Option<&str>, target: &str) -> bool {
    let m = matcher.map(str::trim).unwrap_or("");
    if m.is_empty() || m == "*" {
        return true;
    }
    match regex::Regex::new(&format!("^(?:{m})$")) {
        Ok(re) => re.is_match(target),
        Err(_) => m == target,
    }
}

/// Whether a group applies to any of `targets`. No targets: every group.
pub fn group_applies(group: &HookMatcher, targets: &[&str]) -> bool {
    targets.is_empty()
        || targets
            .iter()
            .any(|t| matcher_accepts(group.matcher.as_deref(), t))
}

/// Drops non-runnable commands and the groups and events left empty.
pub fn normalize(config: HooksConfig) -> HooksConfig {
    config
        .into_iter()
        .map(|(event, groups)| {
            let groups: Vec<HookMatcher> = groups
                .into_iter()
                .map(|mut g| {
                    g.hooks.retain(|h| h.is_runnable());
                    g
                })
                .filter(|g| !g.hooks.is_empty())
                .collect();
            (event, groups)
        })
        .filter(|(_, groups)| !groups.is_empty())
        .collect()
}

/// Concatenates `from`'s groups after `into`'s, event by event.
pub fn merge(mut into: HooksConfig, from: HooksConfig) -> HooksConfig {
    for (event, groups) in from {
        into.entry(event).or_default().extend(groups);
    }
    into
}

/// The `hooks` block of a Claude Code settings document. A document
/// without one is an empty config.
pub fn parse_settings_hooks(path: &str, text: &str) -> Result<HooksConfig, HookError> {
    let unreadable = |detail: String| HookError::Unreadable {
        path: path.to_string(),
        detail,
    };
    let doc: Value = serde_json::from_str(text).map_err(|e| unreadable(e.to_string()))?;
    match doc.get("hooks") {
        None => Ok(HooksConfig::new()),
        Some(block) => serde_json::from_value(block.clone()).map_err(|e| unreadable(e.to_string())),
    }
}

/// The payload envelope every event carries, plus the event's `extra`
/// fields.
pub fn payload(event: HookEvent, session_id: Option<&str>, cwd: &str, extra: Value) -> Value {
    let mut obj = Map::new();
    obj.insert("hook_event_name".into(), json!(event.as_str()));
    obj.insert("session_id".into(), json!(session_id.unwrap_or("")));
    obj.insert("transcript_path".into(), Value::Null);
    obj.insert("cwd".into(), json!(cwd));
    obj.insert("harness".into(), json!("dirge"));
    if let Value::Object(extra) = extra {
        obj.extend(extra);
    }
    Value::Object(obj)
}

/// An addon handler's answer, read as the process it stands in for, so
/// [`interpret`] decodes both the same way:
/// - `{"exit": n, "stdout": s, "stderr": s}`: that exit, verbatim;
/// - a string: exit 0 with it on stdout;
/// - `null`: exit 0, nothing said;
/// - anything else (a Claude JSON answer, typically): exit 0 with its
///   JSON on stdout.
#[cfg_attr(not(feature = "addons"), allow(dead_code))]
pub fn addon_answer(answer: &Value) -> Exited {
    let text = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or("").to_string();
    match answer {
        Value::Object(fields) if fields.get("exit").is_some_and(Value::is_i64) => Exited {
            code: fields.get("exit").and_then(Value::as_i64).map(|c| c as i32),
            stdout: text(fields.get("stdout")),
            stderr: text(fields.get("stderr")),
        },
        Value::Null => Exited {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        },
        Value::String(s) => Exited {
            code: Some(0),
            stdout: s.clone(),
            stderr: String::new(),
        },
        other => Exited {
            code: Some(0),
            stdout: other.to_string(),
            stderr: String::new(),
        },
    }
}

/// Claude Code's reading of a finished command: exit 2 blocks with
/// stderr, exit 0 may carry a JSON answer (or, for some events, plain
/// context), anything else is a failure.
pub fn interpret(event: HookEvent, exited: Exited) -> Result<HookOutcome, HookError> {
    match exited.code {
        Some(2) => Ok(HookOutcome::blocked(
            non_blank(&exited.stderr)
                .unwrap_or_else(|| format!("{event} hook blocked this action")),
        )),
        Some(0) => Ok(read_stdout(event, exited.stdout.trim())),
        code => Err(HookError::NonZeroExit {
            code,
            stderr: exited.stderr,
        }),
    }
}

/// A `UserPromptSubmit` outcome applied to `prompt`. A block wins over
/// context: as in Claude Code, a blocked prompt never reaches the model.
pub fn submission(outcome: HookOutcome, prompt: String) -> Submission {
    let event = HookEvent::UserPromptSubmit;
    if let Some(reason) = outcome.block {
        return Submission::Blocked(format!("{event} hook blocked this prompt: {reason}"));
    }
    match outcome.context_text() {
        Some(text) => Submission::Proceed(format!("{}\n\n{prompt}", system_reminder(event, &text))),
        None => Submission::Proceed(prompt),
    }
}

fn read_stdout(event: HookEvent, stdout: &str) -> HookOutcome {
    match serde_json::from_str::<Value>(stdout) {
        Ok(answer @ Value::Object(_)) => read_json_answer(event, &answer),
        _ if !stdout.is_empty() && event.stdout_is_context() => HookOutcome::with_context(stdout),
        _ => HookOutcome::default(),
    }
}

/// The JSON answer: `hookSpecificOutput` (`permissionDecision`,
/// `additionalContext`, `updatedInput`) and the top-level
/// `decision: "block"` form.
pub fn read_json_answer(event: HookEvent, answer: &Value) -> HookOutcome {
    let specific = answer.get("hookSpecificOutput").unwrap_or(&Value::Null);
    let decision = specific.get("permissionDecision").and_then(Value::as_str);
    let decision_reason = str_field(specific, "permissionDecisionReason");

    let mut out = match decision {
        Some("deny") => HookOutcome::blocked(
            decision_reason
                .clone()
                .unwrap_or_else(|| format!("{event} hook denied this action")),
        ),
        Some("ask") => HookOutcome::asked(
            decision_reason.unwrap_or_else(|| format!("{event} hook asks to confirm this action")),
        ),
        _ => HookOutcome::default(),
    };
    if let Some(ctx) = str_field(specific, "additionalContext") {
        out = out.combine(HookOutcome::with_context(ctx));
    }
    if let Some(updated) = specific.get("updatedInput").filter(|v| v.is_object()) {
        out.updated_input = Some(updated.clone());
    }
    if answer.get("decision").and_then(Value::as_str) == Some("block") {
        out = out.combine(HookOutcome::blocked(
            str_field(answer, "reason")
                .unwrap_or_else(|| format!("{event} hook blocked this action")),
        ));
    }
    out
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).and_then(non_blank)
}

fn non_blank(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}
