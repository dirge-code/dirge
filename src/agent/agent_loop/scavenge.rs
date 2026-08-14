//! Scavenge tool calls from reasoning content.
//!
//! Faithful port of `DeepSeek-Reasonix/src/repair/scavenge.ts` (201 lines).
//!
//! DeepSeek R1 sometimes emits tool-call JSON inside reasoning_content
//! and forgets to include it in the structured `tool_calls` field.
//! This module recovers those calls from the reasoning text.
//!
//! Three patterns are recognized:
//!
//! 1. DSML invoke blocks: `<｜DSML｜invoke name="tool_name">...</>`
//! 2. Tagged / fenced text channels (dirge-56vo): `<tool_call>…</tool_call>`
//!    and ```` ```json ```` / ```` ```tool ```` fences.
//! 3. Raw JSON objects matching:
//!    - `{name, arguments}` (simplest form)
//!    - `{type: "function", function: {name, arguments}}` (OpenAI-style)
//!    - `{tool_name, tool_args}` (R1 free-form variant)
//!
//! Only tools whose name appears in the allowed set are returned.
//! A max-calls cap defends against runaway extraction.
//! Inputs over 100KB are skipped (defense against regex O(n²)).
//!
//! # Why the tagged pass exists (dirge-56vo)
//!
//! `<tool_call>…</tool_call>` is Qwen/Hermes' *native* tool channel, and it
//! leaks into plain text whenever llama.cpp is served without `--jinja` — the
//! most likely text-leak shape for dirge's ollama / lmstudio / llama.cpp
//! users. Well-formed JSON inside those tags was already reachable through
//! the raw-JSON scan below, so the tags buy two things the scan cannot:
//!
//!   - **Bounds.** [`iterate_json_objects`] only emits *balanced* objects, so
//!     a call truncated at `max_tokens` produces no candidate at all. The tag
//!     delimits the region explicitly, which is what lets
//!     [`repair_truncated_json`] close it.
//!   - **A repair budget that can't hurt precision.** Applying lenient repair
//!     to every brace-run in prose would be reckless; applying it inside an
//!     explicit tool-call tag is not.
//!
//! Precision throughout rests on the same gate as before: a candidate becomes
//! a call only if it carries a `name` that [`is_dispatchable`] — one this run
//! has, or one the alias table places on a tool this run has. Repair never
//! invents a name, and the gate never widens past the tools that exist.

use crate::agent::agent_loop::tool_input_repair::repair_truncated_json;
use crate::agent::agent_loop::tools::ToolCall;

use std::sync::LazyLock;

use regex::Regex;

/// Maximum input size before we skip scavenging.
/// Port of `MAX_SCAVENGE_INPUT` (scavenge.ts:18).
const MAX_SCAVENGE_INPUT: usize = 100 * 1024;

/// Cap on recorded [`ScavengeResult::unknown_names`] per pass.
///
/// The call budget bounds accepted calls but not rejected ones, and the input
/// runs to 100 KB — a model stuck emitting invented calls could otherwise put
/// a four-figure number of log lines on one turn. Well above any real turn, so
/// it does not shape the measurement it exists to keep affordable.
const MAX_UNKNOWN_NAMES: usize = 16;

// Module-level compiled regexes to avoid per-call recompilation.
static RE_DSML_INVOKE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<[｜|]DSML[｜|]invoke\s+name="([^"]+)">([\s\S]*?)</[｜|]DSML[｜|]invoke>"#)
        .expect("DSML invoke regex must compile")
});
static RE_DSML_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"<[｜|]DSML[｜|]parameter\s+name="([^"]+)"(?:\s+string="(true|false)")?\s*>([\s\S]*?)</[｜|]DSML[｜|]parameter>"#
    ).expect("DSML parameter regex must compile")
});
/// Result of a scavenge pass.
#[derive(Debug, Clone, Default)]
pub struct ScavengeResult {
    pub calls: Vec<ToolCall>,
    /// What the pass did, one line per event — which shape each accepted call
    /// came from, or why the pass was skipped entirely.
    ///
    /// These were built and dropped on the floor until dirge-e31n.8. The one
    /// that mattered is the skip: a reasoning blob over [`MAX_SCAVENGE_INPUT`]
    /// turns scavenging off for that turn, which on a model that emits its
    /// calls as text means every call in it is lost, and nothing said so.
    pub notes: Vec<String>,
    /// Names from EXPLICIT call syntax that matched no tool, so the call was
    /// dropped (dirge-e31n.8).
    ///
    /// This is the quietest failure the loop has. A native tool call naming a
    /// missing tool at least comes back as "Tool X not found" and the model
    /// can retry; a call the model wrote as *text* is dropped here with no
    /// result, no error, and — until this field existed — no counter. The
    /// turn then ends with no tool call at all, so the loop reads the raw
    /// call syntax as the final answer.
    ///
    /// Only the two explicit shapes are recorded: a `DSML|invoke` tag and a
    /// tagged/fenced region, both of which say "this is a call" in their own
    /// syntax. A bare JSON object in prose (pattern C) is NOT, and counting
    /// those would report a model quoting a schema as a model fumbling a
    /// call.
    pub unknown_names: Vec<String>,
}

/// Scan reasoning content for tool calls the model forgot to emit.
/// Port of `scavengeToolCalls` (scavenge.ts:20-65).
pub fn scavenge_tool_calls(
    reasoning_content: Option<&str>,
    allowed_names: &std::collections::HashSet<String>,
    max_calls: usize,
) -> ScavengeResult {
    let content = match reasoning_content {
        Some(c) if !c.is_empty() => c,
        _ => return ScavengeResult::default(),
    };

    if content.len() > MAX_SCAVENGE_INPUT {
        return ScavengeResult {
            notes: vec![format!(
                "scavenge skipped: reasoning_content too large ({} chars)",
                content.len()
            )],
            ..ScavengeResult::default()
        };
    }

    let max = if max_calls == 0 { 4 } else { max_calls };
    let mut notes: Vec<String> = Vec::new();
    let mut out: Vec<ToolCall> = Vec::new();
    let mut unknown_names: Vec<String> = Vec::new();

    // Pattern A: DSML invoke blocks.
    for invoke in iterate_dsml_invokes(content) {
        if out.len() >= max {
            break;
        }
        if !is_dispatchable(&invoke.name, allowed_names) {
            if unknown_names.len() < MAX_UNKNOWN_NAMES {
                unknown_names.push(invoke.name.clone());
            }
            continue;
        }
        out.push(ToolCall {
            id: String::new(),
            name: invoke.name.clone(),
            arguments: invoke.args,
        });
        notes.push(format!("scavenged DSML call: {}", invoke.name));
    }

    // Pattern B: tagged / fenced regions (dirge-56vo). Runs before the raw
    // scan, and every region — DSML included — is cut out of what the raw
    // scan sees, so a call inside explicit syntax can't also be picked up as
    // a bare JSON object and counted twice.
    //
    // The regions come from [`super::call_syntax`], which is also what the
    // display filter reads. One definition of where a call starts and ends,
    // so a call that runs cannot also be printed to the user (dirge-n00z).
    let found = super::call_syntax::regions(content);
    let remainder = super::call_syntax::without(content, &found);
    let tagged = found.iter().filter(|r| {
        matches!(
            r.kind,
            super::call_syntax::RegionKind::Tagged | super::call_syntax::RegionKind::Fenced
        )
    });
    for region in tagged {
        if out.len() >= max {
            break;
        }
        let region = content[region.body.clone()].trim();
        if region.is_empty() {
            continue;
        }
        match coerce_to_tool_call(region, allowed_names) {
            Some(call) => {
                notes.push(format!("scavenged tagged call: {}", call.name));
                out.push(call);
            }
            // A `<tool_call>` tag or a ```tool fence carrying a name that
            // matches nothing. The tag is the model saying this IS a call, so
            // the miss is worth recording even though there is nothing to
            // dispatch (dirge-e31n.8).
            None if unknown_names.len() < MAX_UNKNOWN_NAMES => {
                unknown_names.extend(candidate_name(region))
            }
            None => {}
        }
    }

    // Pattern C: raw JSON objects.
    for candidate in iterate_json_objects(&remainder) {
        if out.len() >= max {
            break;
        }
        if let Some(call) = coerce_to_tool_call(&candidate, allowed_names) {
            notes.push(format!("scavenged call: {}", call.name));
            out.push(call);
        }
    }

    ScavengeResult {
        calls: out,
        notes,
        unknown_names,
    }
}

/// The tool name an explicit call region names, whatever it is — the same
/// three shapes [`coerce_to_tool_call`] reads, minus the allowed-name gate.
///
/// Kept beside that function on purpose: it exists only to answer "was there a
/// name here at all?" after the gate said no, and the two must read the same
/// shapes or the miss counter would disagree with the thing that dropped the
/// call.
fn candidate_name(candidate_json: &str) -> Option<String> {
    let parsed = repair_json_lenient(candidate_json)?;
    let obj = parsed.as_object()?;
    let direct = obj.get("name").or_else(|| obj.get("tool_name"));
    let nested = (obj.get("type").and_then(|v| v.as_str()) == Some("function"))
        .then(|| obj.get("function").and_then(|v| v.as_object())?.get("name"))
        .flatten();
    direct
        .or(nested)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

// ---- internal helpers ----

pub struct DsmlInvoke {
    pub name: String,
    args: serde_json::Value,
}

/// Yield every DSML invoke block found in text.
/// Port of `iterateDsmlInvokes` (scavenge.ts:80-90).
pub fn iterate_dsml_invokes(text: &str) -> Vec<DsmlInvoke> {
    let mut out = Vec::new();
    for caps in RE_DSML_INVOKE.captures_iter(text) {
        let name = match caps.get(1) {
            Some(m) => m.as_str().to_string(),
            None => continue,
        };
        let body = match caps.get(2) {
            Some(m) => m.as_str(),
            None => continue,
        };
        out.push(DsmlInvoke {
            name,
            args: parse_dsml_parameters(body),
        });
    }
    out
}

/// Parse DSML parameter blocks into a JSON Value.
/// Port of `parseDsmlParameters` (scavenge.ts:92-113).
/// Falls back to literal text when `string="false"` JSON parse fails.
fn parse_dsml_parameters(body: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for caps in RE_DSML_PARAM.captures_iter(body) {
        let key = match caps.get(1) {
            Some(m) => m.as_str().to_string(),
            None => continue,
        };
        if key.is_empty() {
            continue;
        }
        let string_flag = caps.get(2).map(|m| m.as_str());
        let raw = caps
            .get(3)
            .map(|m| m.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        if string_flag == Some("false") {
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(v) => {
                    map.insert(key, v);
                    continue;
                }
                Err(_) => {
                    // Fall through — keep as literal string.
                }
            }
        }
        map.insert(key, serde_json::Value::String(raw));
    }
    serde_json::Value::Object(map)
}

/// dirge-56vo: parse `raw` as JSON, escalating through repairs.
///
/// Three rungs, cheapest first:
///
///   1. strict parse;
///   2. re-escape literal newlines / tabs / CRs inside string literals — the
///      single most common break in a leaked call, because the leaked calls
///      that matter carry multi-line file content;
///   3. drop trailing commas before a closing `}` / `]`;
///   4. [`repair_truncated_json`], which closes unterminated strings and
///      containers and trims a dangling comma or key at EOF.
///
/// Rung 3 and rung 4 are not the same fix: `repair_truncated_json` only trims
/// at the input's end, so an *interior* `,}` survives it.
///
/// Returns `None` rather than a sentinel when nothing parses: a call the
/// scavenger can't read is a call it must not invent.
fn repair_json_lenient(raw: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        return Some(v);
    }
    let escaped = escape_control_chars_in_strings(raw);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&escaped) {
        return Some(v);
    }
    let decommaed = strip_trailing_commas(&escaped);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&decommaed) {
        return Some(v);
    }
    let repaired = repair_truncated_json(&decommaed);
    if repaired.fallback {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(&repaired.repaired).ok()
}

/// Drop a `,` that is followed only by whitespace and a closing `}` / `]`.
/// String-aware, so a comma inside a string literal is left alone.
fn strip_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for (i, &c) in chars.iter().enumerate() {
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            out.push(c);
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c == ',' {
            let next = chars[i + 1..].iter().find(|n| !n.is_whitespace());
            if matches!(next, Some('}') | Some(']')) {
                continue; // drop it
            }
        }
        out.push(c);
    }
    out
}

/// Escape raw newlines / tabs / CRs that appear *inside* JSON string literals.
/// Text outside strings is untouched, so formatting whitespace between tokens
/// still parses normally.
fn escape_control_chars_in_strings(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => {
                out.push(c);
                escaped = true;
            }
            '"' => {
                in_string = !in_string;
                out.push(c);
            }
            '\n' if in_string => out.push_str("\\n"),
            '\t' if in_string => out.push_str("\\t"),
            '\r' if in_string => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// Argument-object keys seen in the wild, in precedence order.
///
/// Before dirge-56vo only `arguments` was read, so a `{name, parameters}` call
/// coerced to an EMPTY argument object — worse than dropping it, because the
/// result was a correctly-named call that then failed schema validation at
/// promotion and looked like a model error rather than a parse gap.
const ARG_KEYS: [&str; 4] = ["arguments", "parameters", "input", "args"];

/// Read the argument object out of `obj`, accepting any of [`ARG_KEYS`].
/// A value that is itself a JSON *string* is parsed one level (OpenAI encodes
/// `arguments` that way).
fn extract_args(obj: &serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    for key in ARG_KEYS {
        let Some(raw) = obj.get(key) else { continue };
        if let Some(s) = raw.as_str() {
            return repair_json_lenient(s).unwrap_or_else(|| serde_json::json!({}));
        }
        if !raw.is_null() {
            return raw.clone();
        }
    }
    serde_json::json!({})
}

/// Yield every top-level JSON object substring in text.
/// Port of `iterateJsonObjects` (scavenge.ts:116-148).
fn iterate_json_objects(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] != '{' {
            i += 1;
            continue;
        }
        let mut depth: i32 = 0;
        let mut in_string = false;
        let mut escaped = false;

        for j in i..chars.len() {
            let c = chars[j];
            if escaped {
                escaped = false;
                continue;
            }
            if in_string {
                if c == '\\' {
                    escaped = true;
                    continue;
                }
                if c == '"' {
                    in_string = false;
                }
                continue;
            }
            if c == '"' {
                in_string = true;
            } else if c == '{' {
                depth += 1;
            } else if c == '}' {
                depth -= 1;
                if depth == 0 {
                    let candidate: String = chars[i..=j].iter().collect();
                    out.push(candidate);
                    i = j;
                    break;
                }
            }
        }
        // Unmatched brace — skip past it to avoid O(n²) rescan.
        i += 1;
    }
    out
}

/// Whether a name in scavenged text will reach a tool — either because it IS
/// one, or because the alias table places it (dirge-e31n.8).
///
/// The name is NOT rewritten here. Resolution happens once, in the loop, over
/// the final call list, so a scavenged call and a native one carrying the same
/// guessed name are handled by the same code and counted in the same place.
/// This gate only decides what gets that far — before dirge-e31n.8 a synonym
/// was dropped here and the call was simply gone.
pub fn is_dispatchable(name: &str, allowed: &std::collections::HashSet<String>) -> bool {
    allowed.contains(name)
        || crate::agent::agent_loop::tool_aliases::resolve(name, allowed.iter()).is_some()
}

/// Try to coerce a JSON string into a ToolCall.
/// Port of `coerceToToolCall` (scavenge.ts:150-201).
#[allow(clippy::collapsible_if)]
pub fn coerce_to_tool_call(
    candidate_json: &str,
    allowed_names: &std::collections::HashSet<String>,
) -> Option<ToolCall> {
    // dirge-56vo: lenient parse. Precision still rests entirely on the
    // allowed-name gate below — repair can fix a shape, never invent a name.
    let parsed = repair_json_lenient(candidate_json)?;
    let obj = parsed.as_object()?;

    // Pattern 1: { name, arguments } (or parameters / input / args)
    if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
        if is_dispatchable(name, allowed_names) {
            return Some(ToolCall {
                id: String::new(),
                name: name.to_string(),
                arguments: extract_args(obj),
            });
        }
    }

    // Pattern 2: OpenAI-style { type: "function", function: { name, arguments } }
    if obj.get("type").and_then(|v| v.as_str()) == Some("function") {
        if let Some(func) = obj.get("function").and_then(|v| v.as_object()) {
            if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                if is_dispatchable(name, allowed_names) {
                    return Some(ToolCall {
                        id: String::new(),
                        name: name.to_string(),
                        arguments: extract_args(func),
                    });
                }
            }
        }
    }

    // Pattern 3: { tool_name, tool_args } (R1 free-form variant)
    if let Some(name) = obj.get("tool_name").and_then(|v| v.as_str()) {
        if is_dispatchable(name, allowed_names) {
            let args = obj.get("tool_args").cloned().unwrap_or_else(|| {
                // No `tool_args` — fall back to the generic keys rather than
                // handing back an empty object.
                extract_args(obj)
            });
            return Some(ToolCall {
                id: String::new(),
                name: name.to_string(),
                arguments: args,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn allowed() -> HashSet<String> {
        ["get_weather", "search"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn dsml_allowed() -> HashSet<String> {
        ["filesystem_edit_file", "search"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn returns_nothing_for_empty_reasoning() {
        let r = scavenge_tool_calls(None, &allowed(), 4);
        assert!(r.calls.is_empty());
    }

    #[test]
    fn returns_nothing_for_null_reasoning() {
        let r = scavenge_tool_calls(Some(""), &allowed(), 4);
        assert!(r.calls.is_empty());
    }

    #[test]
    fn extracts_pattern_1_name_arguments() {
        let reasoning =
            r#"thinking... I should call {"name": "get_weather", "arguments": {"city": "SF"}}"#;
        let r = scavenge_tool_calls(Some(reasoning), &allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].name, "get_weather");
        assert_eq!(r.calls[0].arguments["city"], "SF");
    }

    #[test]
    fn extracts_openai_style_envelope() {
        let reasoning = r#"plan: {"type":"function","function":{"name":"search","arguments":"{\"q\":\"ts\"}"}}"#;
        let r = scavenge_tool_calls(Some(reasoning), &allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].name, "search");
        assert_eq!(r.calls[0].arguments["q"], "ts");
    }

    #[test]
    fn extracts_tool_name_tool_args_variant() {
        let reasoning = r#"decide: {"tool_name": "search", "tool_args": {"q": "deepseek"}}"#;
        let r = scavenge_tool_calls(Some(reasoning), &allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].name, "search");
        assert_eq!(r.calls[0].arguments["q"], "deepseek");
    }

    #[test]
    fn ignores_tools_not_in_allowed_set() {
        let reasoning = r#"{"name": "rm_rf_slash", "arguments": {}}"#;
        let r = scavenge_tool_calls(Some(reasoning), &allowed(), 4);
        assert!(r.calls.is_empty());
    }

    #[test]
    fn respects_max_calls() {
        let reasoning: String = (0..6)
            .map(|_| r#"{"name": "search", "arguments": {"q": "x"}}"#)
            .collect::<Vec<_>>()
            .join(" then ");
        let r = scavenge_tool_calls(Some(&reasoning), &allowed(), 2);
        assert_eq!(r.calls.len(), 2);
    }

    #[test]
    fn extracts_dsml_invoke_block_with_params() {
        let input = [
            "Let me make the edit.",
            "",
            "<｜DSML｜function_calls> <｜DSML｜invoke name=\"filesystem_edit_file\">",
            "  <｜DSML｜parameter name=\"path\" string=\"true\">F:/x.html</｜DSML｜parameter>",
            "  <｜DSML｜parameter name=\"edits\" string=\"false\">[{\"oldText\":\"a\",\"newText\":\"b\"}]</｜DSML｜parameter>",
            "</｜DSML｜invoke> </｜DSML｜function_calls>",
        ].join("\n");
        let r = scavenge_tool_calls(Some(&input), &dsml_allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        let call = &r.calls[0];
        assert_eq!(call.name, "filesystem_edit_file");
        assert_eq!(call.arguments["path"], "F:/x.html");
        assert_eq!(
            call.arguments["edits"],
            serde_json::json!([{"oldText": "a", "newText": "b"}])
        );
        assert!(r.notes.iter().any(|n| n.contains("DSML")));
    }

    #[test]
    fn accepts_ascii_pipe_dsml_variant() {
        let dsml_search: HashSet<String> = ["search"].iter().map(|s| s.to_string()).collect();
        let input = "<|DSML|invoke name=\"search\"><|DSML|parameter name=\"q\" string=\"true\">ts</|DSML|parameter></|DSML|invoke>";
        let r = scavenge_tool_calls(Some(input), &dsml_search, 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].arguments["q"], "ts");
    }

    #[test]
    fn dsml_call_with_unknown_tool_is_skipped() {
        let input = "<｜DSML｜invoke name=\"rm_rf_slash\"><｜DSML｜parameter name=\"x\" string=\"true\">y</｜DSML｜parameter></｜DSML｜invoke>";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert!(r.calls.is_empty());
    }

    #[test]
    fn dsml_string_false_malformed_json_falls_back_to_literal() {
        let dsml_search: HashSet<String> = ["search"].iter().map(|s| s.to_string()).collect();
        let input = "<｜DSML｜invoke name=\"search\"><｜DSML｜parameter name=\"q\" string=\"false\">not valid json</｜DSML｜parameter></｜DSML｜invoke>";
        let r = scavenge_tool_calls(Some(input), &dsml_search, 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].arguments["q"], "not valid json");
    }

    #[test]
    fn does_not_double_count_json_inside_dsml_block() {
        // Inner JSON is a param value — should not become a separate call
        let input = "<｜DSML｜invoke name=\"filesystem_edit_file\"><｜DSML｜parameter name=\"edits\" string=\"false\">{\"name\": \"filesystem_edit_file\", \"arguments\": {}}</｜DSML｜parameter></｜DSML｜invoke>";
        let r = scavenge_tool_calls(Some(input), &dsml_allowed(), 4);
        assert_eq!(
            r.calls.len(),
            1,
            "should be exactly one call — DSML wrapper, not inner JSON"
        );
    }

    #[test]
    fn skips_large_inputs() {
        let large = "x".repeat(MAX_SCAVENGE_INPUT + 1);
        let r = scavenge_tool_calls(Some(&large), &allowed(), 4);
        assert!(r.calls.is_empty());
        assert!(r.notes.iter().any(|n| n.contains("too large")));
    }

    // ---- dirge-56vo: tagged / fenced text channels + JSON repair ----

    /// The Qwen/Hermes text channel. Well-formed JSON inside the tags was
    /// already reachable through the raw-JSON scan; pin it so the explicit
    /// tag pass can't regress it.
    #[test]
    fn extracts_tool_call_tag() {
        let input = "I'll search.\n<tool_call>\n{\"name\": \"search\", \"arguments\": {\"q\": \"ts\"}}\n</tool_call>";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].name, "search");
        assert_eq!(r.calls[0].arguments["q"], "ts");
    }

    /// The case the raw-JSON scan structurally cannot reach: the object never
    /// closes, so brace-matching never emits a candidate. The tag gives the
    /// region explicit bounds, so the repair can close it.
    #[test]
    fn repairs_truncated_json_inside_tool_call_tag() {
        let input = "<tool_call>{\"name\": \"search\", \"arguments\": {\"q\": \"ts\"</tool_call>";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert_eq!(r.calls.len(), 1, "truncated tagged call should be repaired");
        assert_eq!(r.calls[0].arguments["q"], "ts");
    }

    #[test]
    fn extracts_fenced_tool_call() {
        let input = "Here goes:\n```json\n{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Oslo\"}}\n```";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].name, "get_weather");
        assert_eq!(r.calls[0].arguments["city"], "Oslo");
    }

    /// `parameters` / `input` / `args` are all in the wild. Previously these
    /// coerced to an EMPTY argument object, which is worse than dropping the
    /// call: it produced a well-named call that then failed schema validation.
    #[test]
    fn accepts_alternate_argument_keys() {
        for key in ["parameters", "input", "args"] {
            let input = format!("{{\"name\": \"search\", \"{key}\": {{\"q\": \"ts\"}}}}");
            let r = scavenge_tool_calls(Some(&input), &allowed(), 4);
            assert_eq!(r.calls.len(), 1, "{key}: expected one call");
            assert_eq!(r.calls[0].arguments["q"], "ts", "{key}: args lost");
        }
    }

    /// The highest-value repair in practice: a leaked `write`-shaped call
    /// carries multi-line file content, and a literal newline inside a JSON
    /// string is a strict-parse error.
    #[test]
    fn repairs_literal_newlines_in_json_strings() {
        let input = "{\"name\": \"search\", \"arguments\": {\"q\": \"line one\nline two\"}}";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].arguments["q"], "line one\nline two");
    }

    #[test]
    fn repairs_trailing_commas() {
        let input = "{\"name\": \"search\", \"arguments\": {\"q\": \"ts\",},}";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].arguments["q"], "ts");
    }

    #[test]
    fn tagged_call_with_unknown_tool_is_skipped() {
        let input = "<tool_call>{\"name\": \"rm_rf_slash\", \"arguments\": {}}</tool_call>";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert!(r.calls.is_empty());
    }

    /// Precision guard: the repair pass must not turn ordinary prose or a
    /// non-call JSON blob into a tool call.
    #[test]
    fn prose_and_non_call_json_are_not_scavenged() {
        let input = "I considered {\"q\": \"ts\"} but decided against searching.";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert!(r.calls.is_empty());
    }

    /// A tagged call and the same call in the raw-JSON scan must not both land.
    #[test]
    fn tagged_call_is_not_double_counted() {
        let input = "<tool_call>{\"name\": \"search\", \"arguments\": {\"q\": \"ts\"}}</tool_call>";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert_eq!(r.calls.len(), 1);
    }

    // ---- dirge-e31n.8: the names that get dropped ----
    //
    // The negative half is written FIRST and deliberately: a miss counter that
    // over-fires reports every model quoting a schema as a model fumbling a
    // call, and the resulting number is worse than no number, because it looks
    // like evidence.

    /// Ordinary prose containing JSON with a `name` key is NOT a dropped call.
    /// This is the whole reason pattern C (bare JSON) is excluded — a model
    /// explaining `{"name": "delete_everything"}` is discussing a schema, not
    /// reaching for a tool.
    #[test]
    fn bare_json_in_prose_is_not_counted_as_a_miss() {
        for input in [
            "The MCP server exposes {\"name\": \"delete_everything\", \"arguments\": {}}.",
            "I considered {\"q\": \"ts\"} but decided against searching.",
            "no json here at all",
        ] {
            let r = scavenge_tool_calls(Some(input), &allowed(), 4);
            assert!(
                r.unknown_names.is_empty(),
                "prose scored as a dropped call: {input}"
            );
        }
    }

    /// A call that DISPATCHED is not a miss. Guards the other direction: a
    /// counter that fires on success would make every healthy run look sick.
    #[test]
    fn a_dispatched_call_is_not_counted_as_a_miss() {
        let input = "<tool_call>{\"name\": \"search\", \"arguments\": {\"q\": \"ts\"}}</tool_call>";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        assert!(r.unknown_names.is_empty());
    }

    /// The positive half, DSML: the model wrote an explicit invoke for a tool
    /// that does not exist. Nothing dispatches and nothing errors, so without
    /// this the loss leaves no trace anywhere.
    #[test]
    fn unknown_dsml_invoke_name_is_recorded() {
        let input = "<|DSML|invoke name=\"search_files\">\
                     <|DSML|parameter name=\"q\">ts</|DSML|parameter>\
                     </|DSML|invoke>";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert!(r.calls.is_empty(), "unknown name must not dispatch");
        assert_eq!(r.unknown_names, vec!["search_files".to_string()]);
    }

    /// The positive half, tagged region — and each name shape `coerce_to_tool_call`
    /// reads, so the miss counter cannot go blind to a shape the dropper handles.
    #[test]
    fn unknown_tagged_name_is_recorded_in_every_shape() {
        for (label, body) in [
            ("name", "{\"name\": \"view\", \"arguments\": {}}"),
            (
                "openai",
                "{\"type\": \"function\", \"function\": {\"name\": \"view\", \"arguments\": {}}}",
            ),
            ("tool_name", "{\"tool_name\": \"view\", \"tool_args\": {}}"),
        ] {
            let input = format!("<tool_call>{body}</tool_call>");
            let r = scavenge_tool_calls(Some(&input), &allowed(), 4);
            assert!(r.calls.is_empty(), "{label}: must not dispatch");
            assert_eq!(r.unknown_names, vec!["view".to_string()], "{label}");
        }
    }

    /// A fenced region counts too — same explicit-syntax rule, different tag.
    #[test]
    fn unknown_fenced_name_is_recorded() {
        let input = "```json\n{\"name\": \"execute_command\", \"arguments\": {}}\n```";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert!(r.calls.is_empty());
        assert_eq!(r.unknown_names, vec!["execute_command".to_string()]);
    }

    /// dirge-e31n.8: a synonym in scavenged TEXT used to be dropped here and
    /// the call was simply gone — no result, no error, and the turn ended
    /// with the raw call syntax as the answer. It now survives the gate,
    /// carrying the guessed name for the loop's one resolution pass to
    /// rewrite. Both explicit shapes, since both dropped it before.
    #[test]
    fn an_aliasable_name_survives_the_gate() {
        let tools: HashSet<String> = ["bash", "question"].iter().map(|s| s.to_string()).collect();
        for input in [
            "<|DSML|invoke name=\"shell\"><|DSML|parameter name=\"command\">ls</|DSML|parameter></|DSML|invoke>".to_string(),
            "<tool_call>{\"name\": \"execute_command\", \"arguments\": {\"command\": \"ls\"}}</tool_call>".to_string(),
            "```json\n{\"name\": \"ask_user\", \"arguments\": {}}\n```".to_string(),
        ] {
            let r = scavenge_tool_calls(Some(&input), &tools, 4);
            assert_eq!(r.calls.len(), 1, "aliasable name dropped: {input}");
            assert!(r.unknown_names.is_empty(), "counted as a loss: {input}");
        }
    }

    /// And the gate does not widen past the tools that exist: an alias for a
    /// tool this run does not have is still a miss, not a call that fails one
    /// layer down with a worse message.
    #[test]
    fn an_alias_for_an_absent_tool_is_still_a_miss() {
        let no_bash: HashSet<String> = ["read"].iter().map(|s| s.to_string()).collect();
        let r = scavenge_tool_calls(
            Some("<tool_call>{\"name\": \"shell\", \"arguments\": {}}</tool_call>"),
            &no_bash,
            4,
        );
        assert!(r.calls.is_empty());
        assert_eq!(r.unknown_names, vec!["shell".to_string()]);
    }

    /// A turn full of invented calls is bounded: the accepted-call budget
    /// never applied to rejected ones, and the input runs to 100 KB.
    #[test]
    fn recorded_misses_are_capped() {
        let input = "<tool_call>{\"name\": \"nope\", \"arguments\": {}}</tool_call>".repeat(40);
        let r = scavenge_tool_calls(Some(&input), &allowed(), 4);
        assert_eq!(r.unknown_names.len(), MAX_UNKNOWN_NAMES);
    }

    /// A tagged region with no name at all is malformed, not a miss — there
    /// is no name to alias and nothing to report.
    #[test]
    fn tagged_region_without_a_name_is_not_a_miss() {
        let input = "<tool_call>{\"arguments\": {\"q\": \"ts\"}}</tool_call>";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert!(r.calls.is_empty());
        assert!(r.unknown_names.is_empty());
    }
}
