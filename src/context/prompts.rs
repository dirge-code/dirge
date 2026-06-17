use std::collections::HashMap;
use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};

static EMBEDDED: Dir = include_dir!("$CARGO_MANIFEST_DIR/prompts");

/// A loaded prompt with optional frontmatter metadata.
///
/// Frontmatter is a small leading `---\n…---\n` block on the .md
/// file that carries:
///
///   - `deny_tools: [edit, write, apply_patch, bash]` — names of
///     tools the LLM cannot invoke while this prompt is active. The
///     permission checker enforces this BEFORE rule matching so
///     prompt-text instructions ("don't write code") aren't the
///     only line of defense. Replaces the old `plan_file`-based
///     PLAN.md gate that lived inside edit/write/apply_patch.
///   - `description: "..."` — one-line UX label for the `/prompt`
///     picker. Optional; falls back to no description.
///
/// Files without frontmatter still load as a prompt — `body` becomes
/// the whole file, `deny_tools` is empty, `description` is None.
#[derive(Debug, Clone, Default)]
pub struct Prompt {
    pub body: String,
    pub deny_tools: Vec<String>,
    /// Surfaced by `/prompt` listing in the slash command UI when set.
    /// Single-line summary of the mode (e.g. "Read-only planning mode").
    pub description: Option<String>,
}

/// Parse `---\n…---\n<body>` frontmatter out of a markdown file.
/// Tolerant: returns a body-only Prompt for files without frontmatter,
/// for malformed frontmatter (missing closing `---`), or for keys we
/// don't recognise. Schema is intentionally tiny (no nested objects,
/// no YAML anchors) so we don't pull in serde_yaml for a 30-line
/// parser.
fn parse_frontmatter(raw: &str) -> Prompt {
    // No leading "---" → entire file is the body.
    let Some(after_open) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return Prompt {
            body: raw.to_string(),
            ..Prompt::default()
        };
    };
    // Find the closing "---" on its own line. If absent, the file
    // is malformed; treat the whole thing as body to avoid surprises.
    let close_marker = "\n---\n";
    let close_marker_crlf = "\r\n---\r\n";
    let (front, body) = if let Some(pos) = after_open.find(close_marker) {
        (&after_open[..pos], &after_open[pos + close_marker.len()..])
    } else if let Some(pos) = after_open.find(close_marker_crlf) {
        (
            &after_open[..pos],
            &after_open[pos + close_marker_crlf.len()..],
        )
    } else {
        return Prompt {
            body: raw.to_string(),
            ..Prompt::default()
        };
    };

    let mut deny_tools: Vec<String> = Vec::new();
    let mut description: Option<String> = None;
    let lines: Vec<&str> = front.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        i += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "deny_tools" => {
                if value.starts_with('[') {
                    // Inline list form: `[a, b, c]`. Tolerant of spaces.
                    let stripped = value.trim_start_matches('[').trim_end_matches(']');
                    deny_tools = stripped
                        .split(',')
                        .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\''))
                        .filter(|s| !s.is_empty())
                        .map(|s| {
                            let mut owned = s.to_string();
                            owned.make_ascii_lowercase();
                            owned
                        })
                        .collect();
                } else if value.is_empty() && i < lines.len() && lines[i].trim().starts_with('-') {
                    // PERM-15: YAML block form:
                    //   deny_tools:
                    //     - edit
                    //     - write
                    // Collect indented `- name` entries until a
                    // non-list line or end of frontmatter.
                    let mut items: Vec<String> = Vec::new();
                    while i < lines.len() {
                        let next = lines[i].trim();
                        if let Some(rest) = next.strip_prefix('-') {
                            let name = rest.trim().trim_matches(|c| c == '"' || c == '\'');
                            if !name.is_empty() {
                                let mut owned = name.to_string();
                                owned.make_ascii_lowercase();
                                items.push(owned);
                            }
                            i += 1;
                        } else if next.is_empty() || next.starts_with('#') {
                            i += 1; // skip blank/comment lines between items
                        } else {
                            break;
                        }
                    }
                    deny_tools = items;
                } else {
                    // Unrecognized form — empty value but not block list.
                    // Treat as empty deny list (defensive).
                }
            }
            "description" => {
                let v = value.trim_matches(|c| c == '"' || c == '\'');
                if !v.is_empty() {
                    description = Some(v.to_string());
                }
            }
            _ => {}
        }
    }
    Prompt {
        body: body.to_string(),
        deny_tools,
        description,
    }
}

pub fn global_prompts_dir() -> PathBuf {
    crate::session::storage::config_path().join("prompts")
}

/// Load all prompts available to the session, with merge order:
///
///   embedded  (lowest precedence — only fills gaps)
///     ↓
///   global    (`~/.config/dirge/prompts/`)
///     ↓
///   local     (`./prompts/`, highest precedence)
///
/// Implementation contract (audit H14): embedded uses `or_insert_with`
/// (soft) so a global / local prompt of the same name overrides it;
/// global and local use `insert` (hard, last-write-wins). The three
/// blocks below MUST stay in this order — swapping them would
/// silently invert precedence. New tiers (e.g. workspace-scoped)
/// should slot in by precedence with the same soft-then-hard pattern.
fn warn_unknown_deny_tools(prompt_name: &str, deny: &[String]) {
    // Review-batch #7: single source of truth for built-in tool
    // names lives in `crate::agent::tools::BUILTIN_TOOL_NAMES`.
    // This used to be a separate `KNOWN_TOOLS` list maintained here
    // AND `BUILTIN_TOOL_NAMES` in `agent/builder.rs`. Drift could
    // produce spurious warnings here, or — worse — an unsafely
    // shadowable name in builder.rs.
    let known: &[&str] = crate::agent::tools::BUILTIN_TOOL_NAMES;
    for t in deny {
        if !known.iter().any(|k| k.eq_ignore_ascii_case(t)) {
            // Could be an MCP-registered tool name (e.g. "edit_file"
            // from an MCP server we don't statically know about) or
            // a typo. We can't distinguish without the registry at
            // load time, so just warn — better to surface a benign
            // hit than to silently fail closed.
            eprintln!(
                "warning: prompt '{}' deny_tools entry {:?} doesn't match any known built-in. \
                 If this is an MCP tool name, ignore this warning; if it's a typo, fix the .md. \
                 Known tools: {}",
                prompt_name,
                t,
                known.join(", "),
            );
        }
    }
}

pub fn load() -> HashMap<String, Prompt> {
    let mut prompts: HashMap<String, Prompt> = HashMap::new();

    for file in EMBEDDED.files() {
        if file.path().extension().is_some_and(|e| e == "md")
            && let Some(name) = file.path().file_stem().and_then(|s| s.to_str())
            && let Some(content) = file.contents_utf8()
        {
            prompts
                .entry(name.to_string())
                .or_insert_with(|| parse_frontmatter(content));
        }
    }

    let global = global_prompts_dir();
    if global.exists()
        && let Ok(entries) = std::fs::read_dir(&global)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Some(name) = path.file_stem().and_then(|s| s.to_str())
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                prompts.insert(name.to_string(), parse_frontmatter(&content));
            }
        }
    }

    let local = PathBuf::from("prompts");
    if local.exists()
        && let Ok(entries) = std::fs::read_dir(&local)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Some(name) = path.file_stem().and_then(|s| s.to_str())
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                prompts.insert(name.to_string(), parse_frontmatter(&content));
            }
        }
    }

    // Warn once per prompt about unknown tool names in deny_tools.
    // Done after the full merge so a global-prompt override of an
    // embedded prompt is checked too.
    for (name, p) in &prompts {
        if !p.deny_tools.is_empty() {
            warn_unknown_deny_tools(name, &p.deny_tools);
        }
    }

    prompts
}

pub fn ensure_global() -> anyhow::Result<()> {
    let dir = global_prompts_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
        copy_embedded(&dir)?;
    }
    Ok(())
}

pub fn regen() -> anyhow::Result<()> {
    let dir = global_prompts_dir();
    std::fs::create_dir_all(&dir)?;
    copy_embedded(&dir)
}

fn copy_embedded(dest: &Path) -> anyhow::Result<()> {
    for file in EMBEDDED.files() {
        if let Some(name) = file.path().file_name().and_then(|s| s.to_str()) {
            let dest_path = dest.join(name);
            if let Some(content) = file.contents_utf8() {
                std::fs::write(&dest_path, content)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter_loads_whole_file_as_body() {
        let raw = "You are dirge.\n\nDo the thing.\n";
        let p = parse_frontmatter(raw);
        assert_eq!(p.body, raw);
        assert!(p.deny_tools.is_empty());
        assert!(p.description.is_none());
    }

    #[test]
    fn frontmatter_extracts_deny_tools_and_description() {
        let raw = "---\ndeny_tools: [edit, write, apply_patch, bash]\ndescription: Read-only plan mode\n---\nYou are dirge.\n";
        let p = parse_frontmatter(raw);
        assert_eq!(p.deny_tools, vec!["edit", "write", "apply_patch", "bash"]);
        assert_eq!(p.description.as_deref(), Some("Read-only plan mode"));
        assert_eq!(p.body, "You are dirge.\n");
    }

    /// #429: plan mode must be a comprehensive read-only lock — every tool
    /// that can change the filesystem, run a command, reach the network, or
    /// delegate work must be denied. A denylist is fragile (a new mutating
    /// tool is unsafe-by-default until added here), so pin the required set.
    #[test]
    fn plan_prompt_locks_down_all_mutation_and_exec_tools() {
        let raw = EMBEDDED
            .get_file("plan.md")
            .and_then(|f| f.contents_utf8())
            .expect("embedded plan.md present");
        let p = parse_frontmatter(raw);
        for required in [
            "edit",
            "write",
            "apply_patch",
            "edit_lines",
            "edit_minified",
            "bash",
            "webfetch",
            "task",
            "mcp_tool",
            "plugin_tool",
            "debug",
            "spec",
        ] {
            assert!(
                p.deny_tools
                    .iter()
                    .any(|d| d.eq_ignore_ascii_case(required)),
                "plan mode must deny {required:?}; deny_tools = {:?}",
                p.deny_tools,
            );
        }
        // Every entry must be a known built-in, or it warns at load.
        for d in &p.deny_tools {
            assert!(
                crate::agent::tools::BUILTIN_TOOL_NAMES
                    .iter()
                    .any(|k| k.eq_ignore_ascii_case(d)),
                "plan deny_tools entry {d:?} isn't a known built-in (would warn at load)",
            );
        }
    }

    #[test]
    fn frontmatter_tolerates_quoted_tool_names_and_whitespace() {
        let raw = "---\ndeny_tools: [ \"edit\" , 'write' ,  bash ]\n---\nbody\n";
        let p = parse_frontmatter(raw);
        assert_eq!(p.deny_tools, vec!["edit", "write", "bash"]);
    }

    #[test]
    fn malformed_frontmatter_falls_back_to_whole_body() {
        // Opens with `---` but never closes — treat as body so the
        // LLM still sees something useful.
        let raw = "---\ndeny_tools: [edit]\n\nbody without close\n";
        let p = parse_frontmatter(raw);
        assert_eq!(p.body, raw);
        assert!(p.deny_tools.is_empty());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let raw = "---\ndeny_tools: [edit]\nunknown_key: whatever\nfuture_thing: 42\n---\nbody\n";
        let p = parse_frontmatter(raw);
        assert_eq!(p.deny_tools, vec!["edit"]);
        assert_eq!(p.body, "body\n");
    }

    /// PERM-15: YAML block-form `deny_tools:` with indented `- name`
    /// entries must NOT silently produce an empty list. Plan-mode
    /// prompts using block form would otherwise fail open.
    #[test]
    fn block_form_deny_tools_is_parsed() {
        let raw = "\
---
deny_tools:
  - edit
  - write
  - apply_patch
  - bash
description: Plan mode
---
body
";
        let p = parse_frontmatter(raw);
        assert_eq!(p.deny_tools, vec!["edit", "write", "apply_patch", "bash"]);
        assert_eq!(p.description.as_deref(), Some("Plan mode"));
    }

    /// PERM-15: Block form with quoted entries and extra whitespace.
    #[test]
    fn block_form_deny_tools_with_quotes() {
        let raw = "\
---
deny_tools:
  - edit
  - \"write\"
  - 'bash'
description: Mixed quotes
---
body
";
        let p = parse_frontmatter(raw);
        assert_eq!(p.deny_tools, vec!["edit", "write", "bash"]);
    }
}
