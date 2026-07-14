//! User-defined **agent profiles** (dirge-ykeu, Phase 1: load-only).
//!
//! An agent profile is a named bundle of `{ prompt, model, tools, reasoning,
//! temperature }`. This module *loads* them; nothing here changes runtime
//! behavior yet — later phases wire `/agent <name>` switching and fold the
//! built-in roles (critic/review/…) into the same registry. Absent any
//! definitions the registry is empty and dirge behaves exactly as before
//! (fully opt-in).
//!
//! Two sources, layered (later overrides earlier, by name):
//!   1. `config.json` `"agents": { "<name>": { … } }` (lowest precedence)
//!   2. global files  `<config_dir>/agents/<name>.md`
//!   3. project files `.dirge/agents/<name>.md`            (highest)
//!
//! The `.md` files use the same YAML-ish frontmatter + body shape as skills
//! and prompts (a tiny hand-rolled parser — no serde_yaml).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Global agent-profile directory: `~/.config/dirge/agents/`.
pub fn global_agents_dir() -> PathBuf {
    crate::session::storage::config_path().join("agents")
}

/// Per-project agent-profile directory: `<project-root>/.dirge/agents/`.
/// Anchored at the project root (git-root walk-up, `DIRGE_PROJECT_ROOT`
/// override) via `ProjectPaths` rather than the raw launch CWD, so a
/// subdirectory launch still finds the repo's profiles (dirge-vpma.17).
pub fn project_agents_dir(cwd: &Path) -> PathBuf {
    crate::extras::dirge_paths::ProjectPaths::new(cwd).agents_dir()
}

/// Resolve a profile's `model` field to a model string for the active client.
/// If it names a `providers` alias carrying a `model`, that model is used; else
/// the value is treated as the model name. `None` → keep the current model.
///
/// Same-client resolution: only the model string is taken even when the alias
/// implies a different backend (`provider_type`/`base_url`). Shared by `/agent`
/// switching and the `task` tool's per-profile subagent routing.
pub fn resolve_model_alias(cfg: &crate::config::Config, model: Option<&str>) -> Option<String> {
    let m = model?;
    if let Some(providers) = &cfg.providers
        && let Some(entry) = providers.get(m)
        && let Some(model_str) = &entry.model
    {
        return Some(model_str.clone());
    }
    Some(m.to_string())
}

/// Which tools an agent may call. Enforced (in a later phase) through the same
/// permission-layer mechanism that backs prompt `deny_tools`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolPolicy {
    /// No restriction — every tool available (the default).
    #[default]
    All,
    /// Only these tool names are allowed.
    Allow(Vec<String>),
    /// Every tool except these names.
    Deny(Vec<String>),
}

impl ToolPolicy {
    /// Convert to the deny-list shape consumed by the permission layer
    /// (`current_prompt_deny_tools` / `apply_prompt_deny`). `Allow` is
    /// realized as "deny every built-in not in the allow-list" over
    /// `builtins`. Because `builtins` (`BUILTIN_TOOL_NAMES`) includes the
    /// `mcp_tool` and `plugin_tool` umbrella names, an `allow` list that
    /// omits them also denies ALL MCP and plugin tools wholesale — so
    /// `allow_tools` is a genuine cap (dirge-74nb). It cannot, however,
    /// allow-list a SPECIFIC MCP/plugin tool by name (those aren't
    /// enumerable here); to permit one, allow its umbrella (`mcp_tool` /
    /// `plugin_tool`). Names are lowercased to match the permission layer.
    #[allow(dead_code)] // consumed by `/agent` switching
    pub fn to_deny_list(&self, builtins: &[&str]) -> Vec<String> {
        match self {
            ToolPolicy::All => Vec::new(),
            ToolPolicy::Deny(names) => names.clone(),
            ToolPolicy::Allow(allow) => {
                let allow: Vec<String> = allow.iter().map(|s| s.to_ascii_lowercase()).collect();
                builtins
                    .iter()
                    .map(|b| b.to_ascii_lowercase())
                    .filter(|b| !allow.contains(b))
                    .collect()
            }
        }
    }
}

/// Capability tier for a `task(agent=…)` subagent's tool set:
/// `Toolless` (the unchanged one-shot `btw_query` default), `Readonly`
/// (a real filtered agent loop with the read-only tool universe), and
/// `ReadWrite` (readonly PLUS the write/bash family — a subagent can
/// edit the code tree and run builds/tests directly). Durable-state /
/// session-attribution / recursion / interactive tools stay stripped
/// regardless of tier (see `SUBAGENT_FORCED_EXCLUDES`), so even
/// `ReadWrite` can't write agent state or attribute to a session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SubagentToolTier {
    /// No tools — the subagent runs a one-shot `btw_query` (unchanged default).
    #[default]
    Toolless,
    /// Read-only tool set (read/grep/glob/…); no mutation, no recursion.
    Readonly,
    /// Read-write tool set — readonly + write/edit/bash/apply_patch.
    ReadWrite,
}

/// Per-profile policy for what tools a `task(agent=…)` subagent may use.
/// Layered over [`SubagentToolTier`]: the tier fixes the tool universe,
/// `allow`/`deny` are raw overrides (for readonly, `allow` cannot escalate
/// past the tier and `deny` narrows), and `max_turns` bounds the loop.
/// Defaults to a tool-less subagent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubagentToolPolicy {
    pub tier: SubagentToolTier,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub max_turns: Option<usize>,
}

/// `config.json` `agents.<name>.subagent` block (serde). Mirrors the `.md`
/// frontmatter's `subagent_*` keys so both sources describe the same shape.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SubagentConfig {
    /// Tier name: "readonly" | "toolless" (omitted/unknown → toolless).
    pub tools: Option<String>,
    pub allow: Option<Vec<String>>,
    pub deny: Option<Vec<String>>,
    pub max_turns: Option<usize>,
}

/// Map a tier name to its enum. Tolerant: known names map to variants,
/// anything else (incl. empty) → `Toolless` with a warning so a typo is
/// visible rather than silently upgrading a subagent.
fn parse_subagent_tier(raw: &str, agent_name: &str) -> SubagentToolTier {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "toolless" | "none" | "off" | "false" => SubagentToolTier::Toolless,
        "readonly" | "read-only" | "read" => SubagentToolTier::Readonly,
        "readwrite" | "read-write" | "rw" | "full" => SubagentToolTier::ReadWrite,
        other => {
            tracing::warn!(
                target: "dirge::agents",
                agent = %agent_name,
                tier = %other,
                "unknown subagent tier; falling back to toolless"
            );
            SubagentToolTier::Toolless
        }
    }
}

/// Where a definition came from — drives precedence and the `/agents` listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    Config,
    GlobalFile,
    ProjectFile,
}

impl AgentSource {
    pub fn label(self) -> &'static str {
        match self {
            AgentSource::Config => "config.json",
            AgentSource::GlobalFile => "global file",
            AgentSource::ProjectFile => "project file",
        }
    }
}

/// A resolved agent profile.
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    pub name: String,
    /// System prompt body. `None` → use the active/default prompt.
    pub prompt: Option<String>,
    /// `providers` alias to route this agent's calls through. `None` → default.
    pub model: Option<String>,
    pub tools: ToolPolicy,
    /// Reasoning effort hint (e.g. "low" / "medium" / "high"). Free-form.
    pub reasoning: Option<String>,
    pub temperature: Option<f64>,
    /// One-line summary for the `/agents` listing.
    pub description: Option<String>,
    /// What tools a `task(agent="<name>")` subagent may use. Defaults to
    /// tool-less (today's behavior); opt into `Readonly` per-profile.
    pub subagent: SubagentToolPolicy,
    pub source: AgentSource,
}

/// `config.json` `agents` entry (serde). Flat tool keys mirror the `.md`
/// frontmatter so both sources describe an agent the same way.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AgentConfig {
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub allow_tools: Option<Vec<String>>,
    pub deny_tools: Option<Vec<String>>,
    pub reasoning: Option<String>,
    pub temperature: Option<f64>,
    pub description: Option<String>,
    /// Per-profile subagent tool policy. Omitted → tool-less subagent.
    pub subagent: Option<SubagentConfig>,
}

/// `deny_tools` wins when both are present (conservative); else `allow_tools`;
/// else unrestricted.
fn policy_from(allow: Option<Vec<String>>, deny: Option<Vec<String>>) -> ToolPolicy {
    match (deny, allow) {
        (Some(d), _) if !d.is_empty() => ToolPolicy::Deny(normalize_names(d)),
        (_, Some(a)) if !a.is_empty() => ToolPolicy::Allow(normalize_names(a)),
        _ => ToolPolicy::All,
    }
}

fn normalize_names(names: Vec<String>) -> Vec<String> {
    names
        .into_iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

impl AgentConfig {
    fn into_definition(self, name: &str, source: AgentSource) -> AgentDefinition {
        let s = self.subagent.unwrap_or_default();
        AgentDefinition {
            name: name.to_string(),
            prompt: self.prompt.filter(|p| !p.trim().is_empty()),
            model: self.model.filter(|m| !m.trim().is_empty()),
            tools: policy_from(self.allow_tools, self.deny_tools),
            reasoning: self.reasoning,
            temperature: self.temperature,
            description: self.description,
            subagent: SubagentToolPolicy {
                tier: s
                    .tools
                    .as_deref()
                    .map(|t| parse_subagent_tier(t, name))
                    .unwrap_or_default(),
                allow: s.allow.unwrap_or_default(),
                deny: s.deny.unwrap_or_default(),
                max_turns: s.max_turns,
            },
            source,
        }
    }
}

/// The merged, precedence-resolved set of agent profiles.
#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    agents: BTreeMap<String, AgentDefinition>,
}

impl AgentRegistry {
    /// Load + merge from all sources. Order matters: config first, then global
    /// files, then project files — each `insert` overrides the same name, so
    /// the effective precedence is project > global > config.
    pub fn load(
        config_agents: Option<&std::collections::HashMap<String, AgentConfig>>,
        global_dir: Option<&Path>,
        project_dir: Option<&Path>,
    ) -> Self {
        let mut agents: BTreeMap<String, AgentDefinition> = BTreeMap::new();

        if let Some(cfg) = config_agents {
            for (name, ac) in cfg {
                if name.trim().is_empty() {
                    continue;
                }
                agents.insert(
                    name.clone(),
                    ac.clone().into_definition(name, AgentSource::Config),
                );
            }
        }
        if let Some(dir) = global_dir {
            load_dir(dir, AgentSource::GlobalFile, &mut agents);
        }
        if let Some(dir) = project_dir {
            load_dir(dir, AgentSource::ProjectFile, &mut agents);
        }

        Self { agents }
    }

    // Used by `/agent <name>` switching (next phase) and the test suite.
    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Profiles in stable (name-sorted) order.
    pub fn iter(&self) -> impl Iterator<Item = &AgentDefinition> {
        self.agents.values()
    }

    // Used by `/agent` tab-completion (next phase).
    #[allow(dead_code)]
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.agents.keys().map(String::as_str)
    }
}

/// Scan `<dir>/*.md`, parsing each into a definition (filename stem = name).
/// Missing dir or unreadable files are skipped silently — agents are optional.
fn load_dir(dir: &Path, source: AgentSource, out: &mut BTreeMap<String, AgentDefinition>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        out.insert(name.to_string(), parse_agent_md(name, &raw, source));
    }
}

/// Parse `---\n<frontmatter>\n---\n<body>` into an [`AgentDefinition`]. The body
/// is the agent's prompt; frontmatter keys (all optional): `model`,
/// `deny_tools`, `allow_tools`, `reasoning`, `temperature`, `description`.
/// Tolerant: a file without frontmatter is treated as a body-only (prompt)
/// agent. Mirrors `context::prompts`' tiny parser (no serde_yaml).
pub(crate) fn parse_agent_md(name: &str, raw: &str, source: AgentSource) -> AgentDefinition {
    let mut def = AgentDefinition {
        name: name.to_string(),
        prompt: None,
        model: None,
        tools: ToolPolicy::All,
        reasoning: None,
        temperature: None,
        description: None,
        subagent: SubagentToolPolicy::default(),
        source,
    };

    let after_open = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"));
    let (front, body) = match after_open {
        Some(rest) => match rest
            .find("\n---\n")
            .map(|p| (p, 5))
            .or_else(|| rest.find("\r\n---\r\n").map(|p| (p, 7)))
        {
            Some((pos, marker_len)) => (&rest[..pos], &rest[pos + marker_len..]),
            None => ("", raw), // malformed → whole file is the body
        },
        None => ("", raw),
    };

    let body = body.trim();
    if !body.is_empty() {
        def.prompt = Some(body.to_string());
    }

    let mut allow: Option<Vec<String>> = None;
    let mut deny: Option<Vec<String>> = None;
    let mut sub_tier: Option<String> = None;
    let mut sub_allow: Option<Vec<String>> = None;
    let mut sub_deny: Option<Vec<String>> = None;
    let mut sub_max_turns: Option<usize> = None;
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "model" if !value.is_empty() => def.model = Some(value.to_string()),
            "reasoning" if !value.is_empty() => def.reasoning = Some(value.to_string()),
            "description" if !value.is_empty() => def.description = Some(value.to_string()),
            "temperature" => def.temperature = value.parse::<f64>().ok(),
            "deny_tools" => deny = Some(parse_inline_list(value)),
            "allow_tools" => allow = Some(parse_inline_list(value)),
            "subagent_tools" if !value.is_empty() => sub_tier = Some(value.to_string()),
            "subagent_max_turns" => sub_max_turns = value.parse::<usize>().ok(),
            "subagent_allow" => sub_allow = Some(parse_inline_list(value)),
            "subagent_deny" => sub_deny = Some(parse_inline_list(value)),
            _ => {}
        }
    }
    def.tools = policy_from(allow, deny);
    def.subagent = SubagentToolPolicy {
        tier: sub_tier
            .as_deref()
            .map(|t| parse_subagent_tier(t, name))
            .unwrap_or_default(),
        allow: sub_allow.unwrap_or_default(),
        deny: sub_deny.unwrap_or_default(),
        max_turns: sub_max_turns,
    };
    def
}

/// Parse an inline `[a, b, c]` (or bare `a, b`) list of tool names.
fn parse_inline_list(value: &str) -> Vec<String> {
    let inner = value.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\''))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn parses_md_frontmatter_and_body() {
        let raw = "---\nmodel: haiku\ndeny_tools: [bash, write, edit]\nreasoning: high\ntemperature: 0.2\ndescription: read-only reviewer\n---\nYou are a careful reviewer. Report findings.\n";
        let def = parse_agent_md("reviewer", raw, AgentSource::ProjectFile);
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.model.as_deref(), Some("haiku"));
        assert_eq!(def.reasoning.as_deref(), Some("high"));
        assert_eq!(def.temperature, Some(0.2));
        assert_eq!(def.description.as_deref(), Some("read-only reviewer"));
        assert_eq!(
            def.tools,
            ToolPolicy::Deny(vec!["bash".into(), "write".into(), "edit".into()])
        );
        assert_eq!(
            def.prompt.as_deref(),
            Some("You are a careful reviewer. Report findings.")
        );
    }

    #[test]
    fn body_only_file_is_a_prompt_agent() {
        let def = parse_agent_md(
            "scout",
            "Find where X is handled. Read-only.",
            AgentSource::GlobalFile,
        );
        assert!(def.model.is_none());
        assert_eq!(def.tools, ToolPolicy::All);
        assert_eq!(
            def.prompt.as_deref(),
            Some("Find where X is handled. Read-only.")
        );
        // Default profile → tool-less subagent (unchanged behavior).
        assert_eq!(def.subagent.tier, SubagentToolTier::Toolless);
    }

    #[test]
    fn subagent_frontmatter_enables_readonly_tier() {
        let raw = "---\nmodel: haiku\nsubagent_tools: readonly\nsubagent_max_turns: 12\nsubagent_deny: [webfetch]\n---\nbody";
        let def = parse_agent_md("researcher", raw, AgentSource::GlobalFile);
        assert_eq!(def.subagent.tier, SubagentToolTier::Readonly);
        assert_eq!(def.subagent.max_turns, Some(12));
        assert_eq!(def.subagent.deny, vec!["webfetch"]);
    }

    #[test]
    fn subagent_frontmatter_unknown_tier_warns_and_falls_back() {
        let raw = "---\nsubagent_tools: banana\n---\nbody";
        let def = parse_agent_md("x", raw, AgentSource::Config);
        assert_eq!(
            def.subagent.tier,
            SubagentToolTier::Toolless,
            "an unknown tier name must fall back to tool-less, never escalate"
        );
    }

    #[test]
    fn subagent_config_json_into_definition() {
        let cfg = AgentConfig {
            subagent: Some(SubagentConfig {
                tools: Some("read-only".into()),
                deny: Some(vec!["grep".into()]),
                max_turns: Some(40),
                ..Default::default()
            }),
            ..Default::default()
        };
        let def = cfg.into_definition("res", AgentSource::Config);
        assert_eq!(def.subagent.tier, SubagentToolTier::Readonly);
        assert_eq!(def.subagent.deny, vec!["grep"]);
        assert_eq!(def.subagent.max_turns, Some(40));
        // readwrite is recognized as the read-write tier (parses; resolver yields the write/bash family)
        let rw = AgentConfig {
            subagent: Some(SubagentConfig {
                tools: Some("readwrite".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            rw.into_definition("rw", AgentSource::Config).subagent.tier,
            SubagentToolTier::ReadWrite
        );
    }

    #[test]
    fn deny_wins_over_allow_when_both_present() {
        let raw = "---\nallow_tools: [read]\ndeny_tools: [bash]\n---\nbody";
        let def = parse_agent_md("a", raw, AgentSource::Config);
        assert_eq!(def.tools, ToolPolicy::Deny(vec!["bash".into()]));
    }

    #[test]
    fn precedence_project_over_global_over_config() {
        let tmp = std::env::temp_dir().join(format!("dirge-agents-test-{}", std::process::id()));
        let global = tmp.join("global");
        let project = tmp.join("project");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        // Same agent name "rev" defined in all three sources with a distinct model.
        let mut config: HashMap<String, AgentConfig> = HashMap::new();
        config.insert(
            "rev".into(),
            AgentConfig {
                model: Some("config-model".into()),
                ..Default::default()
            },
        );
        std::fs::write(global.join("rev.md"), "---\nmodel: global-model\n---\nb").unwrap();
        std::fs::write(project.join("rev.md"), "---\nmodel: project-model\n---\nb").unwrap();
        // A second agent only in config to confirm merge (not just override).
        config.insert(
            "only-config".into(),
            AgentConfig {
                model: Some("c".into()),
                ..Default::default()
            },
        );

        let reg = AgentRegistry::load(Some(&config), Some(&global), Some(&project));
        assert_eq!(reg.len(), 2);
        assert_eq!(
            reg.get("rev").unwrap().model.as_deref(),
            Some("project-model")
        );
        assert_eq!(reg.get("rev").unwrap().source, AgentSource::ProjectFile);
        assert_eq!(reg.get("only-config").unwrap().model.as_deref(), Some("c"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn tool_policy_to_deny_list() {
        let builtins = ["read", "write", "edit", "bash"];
        assert!(ToolPolicy::All.to_deny_list(&builtins).is_empty());
        assert_eq!(
            ToolPolicy::Deny(vec!["bash".into()]).to_deny_list(&builtins),
            vec!["bash".to_string()]
        );
        // Allow(read) → deny every other built-in.
        let mut got = ToolPolicy::Allow(vec!["read".into()]).to_deny_list(&builtins);
        got.sort();
        assert_eq!(got, vec!["bash", "edit", "write"]);
    }

    /// dirge-74nb: over the REAL builtin set, an `allow_tools` list that
    /// omits the umbrellas denies all MCP *and* plugin tools — `allow_tools`
    /// is a genuine cap, not a built-ins-only filter.
    #[test]
    fn allow_tools_caps_mcp_and_plugin_via_umbrellas() {
        let deny = ToolPolicy::Allow(vec!["read".into(), "grep".into()])
            .to_deny_list(crate::agent::tools::BUILTIN_TOOL_NAMES);
        assert!(
            deny.iter().any(|d| d == "mcp_tool"),
            "MCP umbrella must be denied"
        );
        assert!(
            deny.iter().any(|d| d == "plugin_tool"),
            "plugin umbrella must be denied (dirge-74nb)"
        );
        // The allowed tools are NOT denied.
        assert!(!deny.iter().any(|d| d == "read"));
        assert!(!deny.iter().any(|d| d == "grep"));

        // Explicitly allowing an umbrella keeps that whole class callable.
        let deny = ToolPolicy::Allow(vec!["read".into(), "plugin_tool".into()])
            .to_deny_list(crate::agent::tools::BUILTIN_TOOL_NAMES);
        assert!(
            !deny.iter().any(|d| d == "plugin_tool"),
            "allowed umbrella stays callable"
        );
    }

    #[test]
    fn resolve_model_alias_prefers_provider_entry() {
        use crate::config::{Config, ProviderEntry};
        let mut providers = HashMap::new();
        providers.insert(
            "fast".to_string(),
            ProviderEntry {
                model: Some("anthropic/haiku".to_string()),
                ..Default::default()
            },
        );
        let cfg = Config {
            providers: Some(providers),
            ..Default::default()
        };
        // Alias with a model → that model string.
        assert_eq!(
            resolve_model_alias(&cfg, Some("fast")).as_deref(),
            Some("anthropic/haiku")
        );
        // Unknown alias → used verbatim as a model name.
        assert_eq!(
            resolve_model_alias(&cfg, Some("openai/gpt-4o")).as_deref(),
            Some("openai/gpt-4o")
        );
        // None → None (keep current model).
        assert_eq!(resolve_model_alias(&cfg, None), None);
    }

    #[test]
    fn empty_when_nothing_configured() {
        let reg = AgentRegistry::load(None, None, None);
        assert!(reg.is_empty());
    }
}
