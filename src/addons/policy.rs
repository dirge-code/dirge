//! Reading the Clojure host's answers.

use std::path::Path;

use serde_json::{Value, json};

use super::domain::{
    AddonSummary, BeforeOutcome, CommandOutput, CommandSpec, HookPoint, HookReply, LoadFailure,
    PanelRequest, ToolSpec,
};

/// Longest tool name the providers accept.
const MAX_TOOL_NAME: usize = 64;

/// `name` restricted to `[A-Za-z0-9_-]` and 64 characters.
pub fn exposed_tool_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(MAX_TOOL_NAME)
        .collect();
    if cleaned.is_empty() {
        "addon_tool".to_string()
    } else {
        cleaned
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The host's load report for one manifest, as a summary or the reason it
/// failed.
/// Every hook key a load report names, as written (`dirge/event`, a colon
/// stripped), those no [`HookPoint`] names included.
pub fn hook_keys(report: &Value) -> Vec<String> {
    report
        .get("hooks")
        .and_then(Value::as_array)
        .map(|keys| {
            keys.iter()
                .filter_map(Value::as_str)
                .map(|k| k.strip_prefix(':').unwrap_or(k).to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_summary(manifest: &Path, report: &Value) -> Result<AddonSummary, LoadFailure> {
    let fail = |error: String| LoadFailure {
        manifest: manifest.to_path_buf(),
        error,
    };
    if let Some(error) = str_field(report, "error") {
        return Err(fail(error.to_string()));
    }
    let id = str_field(report, "id")
        .ok_or_else(|| fail("the addon reported no id".to_string()))?
        .to_string();
    let tools = report
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| {
                    let name = str_field(t, "name")?.to_string();
                    Some(ToolSpec {
                        addon_id: id.clone(),
                        exposed_name: exposed_tool_name(&name),
                        description: str_field(t, "description").unwrap_or("").to_string(),
                        input_schema: t
                            .get("inputSchema")
                            .filter(|s| s.is_object())
                            .cloned()
                            .unwrap_or_else(|| json!({ "type": "object" })),
                        name,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let hooks = report
        .get("hooks")
        .and_then(Value::as_array)
        .map(|keys| {
            keys.iter()
                .filter_map(Value::as_str)
                .filter_map(HookPoint::from_key)
                .collect()
        })
        .unwrap_or_default();
    let commands = report
        .get("commands")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|c| {
                    let name = str_field(c, "name")?.trim_start_matches('/');
                    valid_command_name(name).then(|| CommandSpec {
                        addon_id: id.clone(),
                        name: name.to_string(),
                        description: str_field(c, "description").unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(AddonSummary {
        id,
        manifest: manifest.to_path_buf(),
        tools,
        hooks,
        commands,
        health: report.get("health").cloned().unwrap_or(Value::Null),
    })
}

/// Longest slash command name an addon may register.
const MAX_COMMAND_NAME: usize = 32;

/// A command name is 1 to 32 characters of `[a-z0-9_:-]`, starting with a
/// letter, so it can never be mistaken for a path or carry terminal bytes.
pub fn valid_command_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    first_ok
        && name.len() <= MAX_COMMAND_NAME
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_:-".contains(c))
}

/// The commands users can type: every addon's commands in load order, a
/// later one whose name is already taken dropped, like tools. Also returns
/// the `(addon-id, name)` of each dropped command.
pub fn unique_commands(addons: &[AddonSummary]) -> (Vec<CommandSpec>, Vec<(String, String)>) {
    let mut seen = std::collections::HashSet::new();
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for command in addons.iter().flat_map(|a| a.commands.iter()) {
        if seen.insert(command.name.clone()) {
            kept.push(command.clone());
        } else {
            dropped.push((command.addon_id.clone(), command.name.clone()));
        }
    }
    (kept, dropped)
}

/// The `ctx` a command handler receives for the text typed after its name.
pub fn command_ctx(args: &str, cwd: &str) -> Value {
    let argv: Vec<&str> = args.split_whitespace().collect();
    json!({ "args": args.trim(), "argv": argv, "cwd": cwd })
}

/// A command handler's answer: nil, a string (shown), or a map with `text`,
/// `markdown` (both shown) and `prompt` (submitted as the next turn).
pub fn command_output(answer: &Value) -> CommandOutput {
    let shown = |key: &str| {
        str_field(answer, key)
            .map(str::trim_end)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    match answer {
        Value::String(s) if !s.trim().is_empty() => CommandOutput {
            text: Some(s.trim_end().to_string()),
            prompt: None,
        },
        Value::Object(_) => CommandOutput {
            text: shown("text").or_else(|| shown("markdown")),
            prompt: shown("prompt").map(|p| p.trim().to_string()),
        },
        _ => CommandOutput::default(),
    }
}

/// `(dirge.harness/panel op)`'s map as a panel request.
///
/// `{:op "show" :id :title :lines [{:text :face} | "text"]}`,
/// `{:op "show" :id :title :markdown "..."}`, `{:op "append" :id :text :face}`,
/// `{:op "focus" :id :title}`, `{:op "close" :id}`. `op` may be a keyword.
pub fn panel_request(op: &Value) -> Result<PanelRequest, String> {
    let kind = str_field(op, "op")
        .map(|k| k.trim_start_matches(':'))
        .ok_or("panel op needs :op")?;
    let id = str_field(op, "id")
        .filter(|id| !id.trim().is_empty())
        .ok_or("panel op needs a non-empty :id")?
        .to_string();
    let title = || str_field(op, "title").unwrap_or(&id).to_string();
    match kind {
        "show" => match str_field(op, "markdown") {
            Some(markdown) => Ok(PanelRequest::Markdown {
                title: title(),
                markdown: markdown.to_string(),
                id,
            }),
            None => Ok(PanelRequest::Show {
                title: title(),
                lines: panel_lines(op.get("lines")),
                id,
            }),
        },
        "append" => Ok(PanelRequest::Append {
            text: str_field(op, "text").unwrap_or("").to_string(),
            face: str_field(op, "face").unwrap_or("normal").to_string(),
            id,
        }),
        "focus" => Ok(PanelRequest::Focus { title: title(), id }),
        "close" => Ok(PanelRequest::Close { id }),
        other => Err(format!("unknown panel op {other}")),
    }
}

fn panel_lines(lines: Option<&Value>) -> Vec<(String, String)> {
    lines
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| match row {
                    Value::String(s) => (s.clone(), "normal".to_string()),
                    other => (
                        str_field(other, "text").unwrap_or("").to_string(),
                        str_field(other, "face").unwrap_or("normal").to_string(),
                    ),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Exposed tool names in `after` but not `before`, and the reverse.
pub fn tool_diff(before: &[ToolSpec], after: &[ToolSpec]) -> (Vec<String>, Vec<String>) {
    let names = |tools: &[ToolSpec]| -> std::collections::BTreeSet<String> {
        tools.iter().map(|t| t.exposed_name.clone()).collect()
    };
    let (old, new) = (names(before), names(after));
    (
        new.difference(&old).cloned().collect(),
        old.difference(&new).cloned().collect(),
    )
}

/// The tools the model is offered: every addon's tools in load order, a
/// later tool whose exposed name is already taken dropped. Also returns the
/// `(addon-id, tool-name)` of each dropped tool.
pub fn unique_tools(addons: &[AddonSummary]) -> (Vec<ToolSpec>, Vec<(String, String)>) {
    let mut seen = std::collections::HashSet::new();
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for tool in addons.iter().flat_map(|a| a.tools.iter()) {
        if seen.insert(tool.exposed_name.clone()) {
            kept.push(tool.clone());
        } else {
            dropped.push((tool.addon_id.clone(), tool.name.clone()));
        }
    }
    (kept, dropped)
}

/// The host's `call-tool` envelope, `{:ok v}` or `{:error msg}`, as a
/// `Result`. A malformed envelope is an error rather than a silent null.
pub fn tool_reply(envelope: &Value) -> Result<Value, String> {
    if let Some(error) = envelope.get("error") {
        return Err(error
            .as_str()
            .map_or_else(|| error.to_string(), str::to_string));
    }
    envelope
        .get("ok")
        .cloned()
        .ok_or_else(|| format!("addon host returned no result: {envelope}"))
}

/// The host's `run-hook` answer, a vector of `{:addon id :ok v}` /
/// `{:addon id :error msg}`, as replies. Rows without an addon id are dropped.
pub fn hook_replies(answer: &Value) -> Vec<HookReply> {
    answer
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let addon_id = str_field(row, "addon")?.to_string();
                    Some(HookReply {
                        addon_id,
                        result: tool_reply(row),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Source files, each with what a reload made of it.
pub type SourceNotes = Vec<(std::path::PathBuf, String)>;

/// The host's `reload-sources!` answer, rows of `{:file path :error msg}`
/// or `{:file path :skipped msg}`, as `(errors, skipped)`. A row with
/// neither key is an error; a malformed answer reports nothing.
pub fn source_report(answer: &Value) -> (SourceNotes, SourceNotes) {
    let mut errors = Vec::new();
    let mut skipped = Vec::new();
    for row in answer.as_array().into_iter().flatten() {
        let Some(file) = str_field(row, "file").map(std::path::PathBuf::from) else {
            continue;
        };
        match (str_field(row, "error"), str_field(row, "skipped")) {
            (None, Some(why)) => skipped.push((file, why.to_string())),
            (error, _) => {
                errors.push((file, error.unwrap_or("could not be evaluated").to_string()))
            }
        }
    }
    (errors, skipped)
}

/// A hook reply's text: a bare string, or a map's `context`.
fn reply_text(v: &Value) -> Option<String> {
    let text = match v {
        Value::String(s) => Some(s.as_str()),
        Value::Object(_) => str_field(v, "context"),
        _ => None,
    }?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Every non-empty text the addons answered with, in load order. Failed
/// hooks are dropped: a broken addon must not take the session down.
pub fn texts(replies: &[HookReply]) -> Vec<String> {
    replies
        .iter()
        .filter_map(|r| r.result.as_ref().ok())
        .filter_map(reply_text)
        .collect()
}

/// Fold `BeforeToolCall` replies: the first block wins and stops the fold,
/// contexts accumulate, the last `args` replacement wins.
pub fn fold_before(replies: &[HookReply]) -> BeforeOutcome {
    let mut out = BeforeOutcome::default();
    for reply in replies {
        let Ok(v) = &reply.result else { continue };
        if let Some(text) = reply_text(v) {
            out.context.push(text);
        }
        if let Some(args) = v.get("args").filter(|a| a.is_object()) {
            out.args = Some(args.clone());
        }
        if let Some(reason) = v.get("block").and_then(block_reason) {
            out.block = Some((reply.addon_id.clone(), reason));
            break;
        }
    }
    out
}

fn block_reason(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Bool(true) => Some("blocked by addon".to_string()),
        _ => None,
    }
}

/// A tool handler's return value as `(content blocks, details)`, or the
/// error text when the handler reported failure.
///
/// Accepts the MCP result shape IAddon documents
/// (`{:content [...] :isError bool}`), a bare string, or any other data,
/// which is shown to the model as JSON.
pub fn tool_output(result: &Value) -> Result<(Vec<Value>, Value), String> {
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_error {
            return Err(content_text(content));
        }
        return Ok((content.clone(), Value::Null));
    }
    match result {
        Value::String(s) => Ok((vec![text_block(s)], Value::Null)),
        Value::Null => Ok((vec![text_block("")], Value::Null)),
        other => Ok((vec![text_block(&other.to_string())], other.clone())),
    }
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// The text blocks of a tool result's content, joined by newlines.
pub fn content_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `<system-reminder>` wrapping for addon-supplied context, labelled with
/// the hook it came from so the model can tell sources apart.
pub fn reminder(point: HookPoint, text: &str) -> String {
    format!(
        "<system-reminder>\n{} addon context:\n{text}\n</system-reminder>",
        point.key()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ok(addon: &str, v: Value) -> HookReply {
        HookReply {
            addon_id: addon.to_string(),
            result: Ok(v),
        }
    }

    #[test]
    fn tool_names_are_made_provider_safe() {
        assert_eq!(exposed_tool_name("swarm-view"), "swarm-view");
        assert_eq!(exposed_tool_name("haystack:convert"), "haystack_convert");
        assert_eq!(exposed_tool_name(""), "addon_tool");
        assert_eq!(exposed_tool_name(&"x".repeat(80)).len(), 64);
    }

    #[test]
    fn summary_reads_tools_and_known_hooks_only() {
        let report = json!({
            "id": "my.addon",
            "tools": [{"name": "a:b", "description": "d", "inputSchema": {"type": "object"}},
                      {"description": "nameless is dropped"}],
            "hooks": ["dirge/on-prompt", "other-host/startup"],
            "health": {"status": "ok"}
        });
        let s = parse_summary(&PathBuf::from("m.edn"), &report).unwrap();
        assert_eq!(s.id, "my.addon");
        assert_eq!(s.tools.len(), 1);
        assert_eq!(s.tools[0].exposed_name, "a_b");
        assert_eq!(s.hooks, vec![HookPoint::OnPrompt]);
    }

    #[test]
    fn summary_error_is_a_load_failure() {
        let err = parse_summary(&PathBuf::from("m.edn"), &json!({"error": "boom"})).unwrap_err();
        assert_eq!(err.error, "boom");
    }

    #[test]
    fn missing_schema_defaults_to_an_object_schema() {
        let s = parse_summary(
            &PathBuf::from("m.edn"),
            &json!({"id": "x", "tools": [{"name": "t"}]}),
        )
        .unwrap();
        assert_eq!(s.tools[0].input_schema, json!({"type": "object"}));
    }

    #[test]
    fn first_block_wins_and_stops_the_fold() {
        let replies = vec![
            ok("a", json!({"context": "note-a"})),
            ok("b", json!({"block": "no"})),
            ok("c", json!({"block": "never seen", "context": "note-c"})),
        ];
        let out = fold_before(&replies);
        assert_eq!(out.block, Some(("b".to_string(), "no".to_string())));
        assert_eq!(out.context, vec!["note-a".to_string()]);
    }

    #[test]
    fn failed_and_empty_replies_contribute_nothing() {
        let replies = vec![
            HookReply {
                addon_id: "a".into(),
                result: Err("boom".into()),
            },
            ok("b", json!("  ")),
            ok("c", Value::Null),
            ok("d", json!("kept")),
        ];
        assert_eq!(texts(&replies), vec!["kept".to_string()]);
        assert_eq!(
            fold_before(&replies),
            BeforeOutcome {
                context: vec!["kept".to_string()],
                ..BeforeOutcome::default()
            }
        );
    }

    #[test]
    fn last_args_replacement_wins() {
        let replies = vec![
            ok("a", json!({"args": {"x": 1}})),
            ok("b", json!({"args": {"x": 2}})),
        ];
        assert_eq!(fold_before(&replies).args, Some(json!({"x": 2})));
    }

    #[test]
    fn mcp_shaped_results_pass_through() {
        let (content, _) =
            tool_output(&json!({"content": [{"type": "text", "text": "rows=3"}]})).unwrap();
        assert_eq!(content, vec![json!({"type": "text", "text": "rows=3"})]);
    }

    #[test]
    fn mcp_error_results_become_errors() {
        let err =
            tool_output(&json!({"content": [{"type": "text", "text": "bad"}], "isError": true}))
                .unwrap_err();
        assert_eq!(err, "bad");
    }

    #[test]
    fn plain_data_is_shown_as_json_and_kept_as_details() {
        let (content, details) = tool_output(&json!({"n": 1})).unwrap();
        assert_eq!(content, vec![json!({"type": "text", "text": "{\"n\":1}"})]);
        assert_eq!(details, json!({"n": 1}));
    }

    #[test]
    fn tool_envelopes_read_as_results() {
        assert_eq!(tool_reply(&json!({"ok": {"n": 1}})), Ok(json!({"n": 1})));
        assert_eq!(tool_reply(&json!({"ok": null})), Ok(Value::Null));
        assert_eq!(
            tool_reply(&json!({"error": "boom"})),
            Err("boom".to_string())
        );
        assert!(tool_reply(&json!({})).is_err());
    }

    #[test]
    fn hook_rows_become_replies_in_order() {
        let rows = json!([
            {"addon": "a", "ok": "x"},
            {"addon": "b", "error": "boom"},
            {"ok": "orphan row is dropped"}
        ]);
        let replies = hook_replies(&rows);
        assert_eq!(
            replies,
            vec![
                HookReply {
                    addon_id: "a".into(),
                    result: Ok(json!("x"))
                },
                HookReply {
                    addon_id: "b".into(),
                    result: Err("boom".into())
                },
            ]
        );
        assert!(hook_replies(&json!({"not": "a vector"})).is_empty());
    }

    #[test]
    fn reload_rows_split_into_errors_and_skips() {
        let (errors, skipped) = source_report(&json!([
            {"file": "/a.cljc", "error": "eof"},
            {"file": "/b.cljc", "skipped": "nothing has loaded b"},
            {"file": "/c.cljc"},
            {"error": "a row without a file is dropped"}
        ]));
        assert_eq!(
            errors,
            vec![
                (PathBuf::from("/a.cljc"), "eof".to_string()),
                (
                    PathBuf::from("/c.cljc"),
                    "could not be evaluated".to_string()
                ),
            ]
        );
        assert_eq!(
            skipped,
            vec![(PathBuf::from("/b.cljc"), "nothing has loaded b".to_string())]
        );
        assert_eq!(
            source_report(&json!({"not": "rows"})),
            (Vec::new(), Vec::new())
        );
    }

    fn summary(id: &str, tools: &[&str]) -> AddonSummary {
        parse_summary(
            &PathBuf::from(format!("{id}.edn")),
            &json!({"id": id, "tools": tools.iter().map(|t| json!({"name": t})).collect::<Vec<_>>()}),
        )
        .unwrap()
    }

    #[test]
    fn first_addon_keeps_a_contested_tool_name() {
        let addons = vec![summary("a", &["x", "y"]), summary("b", &["x:", "z"])];
        let (kept, dropped) = unique_tools(&addons);
        let names: Vec<_> = kept
            .iter()
            .map(|t| (t.addon_id.as_str(), t.exposed_name.as_str()))
            .collect();
        assert_eq!(names, vec![("a", "x"), ("a", "y"), ("b", "x_"), ("b", "z")]);
        assert!(dropped.is_empty());

        let addons = vec![summary("a", &["x"]), summary("b", &["x"])];
        let (kept, dropped) = unique_tools(&addons);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].addon_id, "a");
        assert_eq!(dropped, vec![("b".to_string(), "x".to_string())]);
    }

    #[test]
    fn hook_keys_round_trip_with_or_without_colon() {
        for p in HookPoint::ALL {
            assert_eq!(HookPoint::from_key(p.key()), Some(p));
            assert_eq!(HookPoint::from_key(&format!(":{}", p.key())), Some(p));
        }
        assert_eq!(HookPoint::from_key("dirge/unknown"), None);
    }

    #[test]
    fn summary_reads_commands_and_drops_unsafe_names() {
        let report = json!({
            "id": "a",
            "commands": [
                {"name": "/swarm", "description": "grid"},
                {"name": "kanban:list"},
                {"name": "Bad Name"},
                {"name": "x\u{1b}[31m"},
                {"description": "nameless"}
            ]
        });
        let s = parse_summary(&PathBuf::from("a.edn"), &report).unwrap();
        let names: Vec<_> = s.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["swarm", "kanban:list"]);
        assert_eq!(s.commands[0].description, "grid");
        assert_eq!(s.commands[0].addon_id, "a");
    }

    #[test]
    fn command_names_are_short_lowercase_words() {
        assert!(valid_command_name("swarm"));
        assert!(valid_command_name("k8s-pods"));
        assert!(!valid_command_name(""));
        assert!(!valid_command_name("9lives"));
        assert!(!valid_command_name("../x"));
        assert!(!valid_command_name(&"a".repeat(33)));
    }

    #[test]
    fn first_addon_keeps_a_contested_command() {
        let with = |id: &str, names: &[&str]| {
            parse_summary(
                &PathBuf::from(format!("{id}.edn")),
                &json!({"id": id, "commands": names.iter().map(|n| json!({"name": n})).collect::<Vec<_>>()}),
            )
            .unwrap()
        };
        let (kept, dropped) = unique_commands(&[with("a", &["x", "y"]), with("b", &["x", "z"])]);
        let names: Vec<_> = kept
            .iter()
            .map(|c| (c.addon_id.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(names, vec![("a", "x"), ("a", "y"), ("b", "z")]);
        assert_eq!(dropped, vec![("b".to_string(), "x".to_string())]);
    }

    #[test]
    fn command_ctx_splits_argv() {
        assert_eq!(
            command_ctx("  list  todo ", "/w"),
            json!({"args": "list  todo", "argv": ["list", "todo"], "cwd": "/w"})
        );
    }

    #[test]
    fn command_answers_read_as_text_and_prompt() {
        assert_eq!(command_output(&Value::Null), CommandOutput::default());
        assert_eq!(command_output(&json!("  ")), CommandOutput::default());
        assert_eq!(
            command_output(&json!("done\n")),
            CommandOutput {
                text: Some("done".into()),
                prompt: None
            }
        );
        assert_eq!(
            command_output(&json!({"markdown": "# t", "prompt": " go on "})),
            CommandOutput {
                text: Some("# t".into()),
                prompt: Some("go on".into())
            }
        );
    }

    #[test]
    fn panel_ops_parse_each_shape() {
        assert_eq!(
            panel_request(
                &json!({"op": ":show", "id": "p", "lines": ["a", {"text": "b", "face": "warn"}]})
            ),
            Ok(PanelRequest::Show {
                id: "p".into(),
                title: "p".into(),
                lines: vec![("a".into(), "normal".into()), ("b".into(), "warn".into())],
            })
        );
        assert_eq!(
            panel_request(&json!({"op": "show", "id": "p", "title": "T", "markdown": "# x"})),
            Ok(PanelRequest::Markdown {
                id: "p".into(),
                title: "T".into(),
                markdown: "# x".into()
            })
        );
        assert_eq!(
            panel_request(&json!({"op": "append", "id": "log", "text": "hi"})),
            Ok(PanelRequest::Append {
                id: "log".into(),
                text: "hi".into(),
                face: "normal".into()
            })
        );
        assert_eq!(
            panel_request(&json!({"op": "close", "id": "p"})),
            Ok(PanelRequest::Close { id: "p".into() })
        );
        assert!(panel_request(&json!({"op": "show"})).is_err());
        assert!(panel_request(&json!({"id": "p"})).is_err());
        assert!(panel_request(&json!({"op": "explode", "id": "p"})).is_err());
    }

    #[test]
    fn tool_diff_names_what_came_and_went() {
        let before = summary("a", &["x", "y"]).tools;
        let after = summary("a", &["y", "z"]).tools;
        assert_eq!(
            tool_diff(&before, &after),
            (vec!["z".to_string()], vec!["x".to_string()])
        );
    }
}
