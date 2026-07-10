use std::collections::HashMap;
use std::path::PathBuf;

use smallvec::SmallVec;

use crate::session::storage;

pub mod agent_defs;
pub mod prompts;

pub struct ContextFiles {
    pub agents: Option<String>,
    pub prompts: HashMap<String, prompts::Prompt>,
    /// User-defined agent profiles (dirge-ykeu). Empty unless the user opts
    /// in via `.dirge/agents/*.md` or `config.json` `agents`. Populated by
    /// `main` after config load (it needs `config.agents` for the lowest-
    /// precedence tier); `context::load` leaves it default-empty.
    pub agent_defs: agent_defs::AgentRegistry,
    /// Name of the active agent profile (set by `/agent <name>`), or `None`
    /// when no profile is active. Transient (not persisted); purely for the
    /// `/agent` listing's active marker and the status display.
    ///
    /// EFFECTIVE OUTPUT — derived by [`recompute_composition`]. Do not write
    /// directly; mutate the [`agent_layer`] via [`set_agent_layer`] /
    /// [`clear_agent_layer`].
    pub current_agent: Option<String>,
    /// EFFECTIVE system-prompt body. Derived by [`recompute_composition`]:
    /// the agent layer's body (if it defines one) wins over the prompt
    /// layer's body. Do not write directly.
    pub current_prompt: Option<String>,
    /// EFFECTIVE "mode" name — drives the plan/review mode reminders and
    /// session persistence. Owned by the prompt layer (the `/prompt`
    /// selection), falling back to the agent name only when no prompt is
    /// active. Do not write directly.
    pub current_prompt_name: Option<String>,
    /// EFFECTIVE deny-list: the UNION of the prompt layer's and the agent
    /// layer's denied tools. Consumed by the permission checker BEFORE rule
    /// matching so prompt-/profile-level restrictions (e.g. plan mode
    /// forbidding edit/write/apply_patch) are enforced at the security
    /// layer, not just via prose. Denies COMPOSE — an agent can only ADD
    /// restrictions to an active prompt, never weaken them. Do not write
    /// directly; it is recomputed from the layers.
    pub current_prompt_deny_tools: Vec<String>,

    // ---- Composition layers (dirge-x7c8 / dirge-anhw) ----------------
    // The effective fields above are a fold of these layers. Two commands
    // contribute independently — `/prompt` sets `prompt_layer`, `/agent`
    // sets `agent_layer` — and neither clobbers the other. `/agent off`
    // pops only the agent layer and recomputes, restoring the prompt
    // layer's prompt + denies and the pre-agent model.
    /// The `/prompt` selection (mode): name, body, and its `deny_tools`.
    pub prompt_layer: Option<PromptLayer>,
    /// The active `/agent` profile, or `None`. Contributes an optional
    /// body override, a model, and extra denies.
    pub agent_layer: Option<agent_defs::AgentDefinition>,
    /// The `session.model` value captured when an agent was activated from
    /// the no-agent state, so `/agent off` can restore it (dirge-anhw).
    /// `None` when no agent is active.
    pub model_before_agent: Option<String>,
}

/// The `/prompt`-selected layer: a named mode with a body and its own
/// `deny_tools`. Independent of any active agent profile.
#[derive(Debug, Clone, Default)]
pub struct PromptLayer {
    pub name: Option<String>,
    pub body: Option<String>,
    pub deny_tools: Vec<String>,
}

impl ContextFiles {
    /// Install / replace the `/prompt` layer and refold the effective
    /// state. Leaves any active agent layer intact.
    pub fn set_prompt_layer(
        &mut self,
        name: Option<String>,
        body: Option<String>,
        deny: Vec<String>,
    ) {
        self.prompt_layer = Some(PromptLayer {
            name,
            body,
            deny_tools: deny,
        });
        self.recompute_composition();
    }

    /// Drop the `/prompt` layer (e.g. `/prompt default`) and refold.
    pub fn clear_prompt_layer(&mut self) {
        self.prompt_layer = None;
        self.recompute_composition();
    }

    /// Install / replace the `/agent` layer and refold. The caller owns
    /// the model swap + `model_before_agent` capture (model lives in
    /// `session`, not here).
    pub fn set_agent_layer(&mut self, def: agent_defs::AgentDefinition) {
        self.agent_layer = Some(def);
        self.recompute_composition();
    }

    /// Drop the `/agent` layer (`/agent off`) and refold, restoring the
    /// prompt layer's prompt + denies. The caller owns the model restore.
    pub fn clear_agent_layer(&mut self) {
        self.agent_layer = None;
        self.recompute_composition();
    }

    /// Fold base → prompt layer → agent layer into the effective fields.
    /// Precedence: the agent body overrides the prompt body; the "mode"
    /// name is owned by the prompt layer (so plan/review reminders survive
    /// an agent override); denies are the UNION (compose, never weaken).
    pub fn recompute_composition(&mut self) {
        // Body: agent persona on top, else the prompt body, else base.
        self.current_prompt = self
            .agent_layer
            .as_ref()
            .and_then(|a| a.prompt.clone())
            .or_else(|| self.prompt_layer.as_ref().and_then(|p| p.body.clone()));

        // Mode name: owned by the prompt layer; fall back to the agent
        // name only when no prompt is active.
        self.current_prompt_name = self
            .prompt_layer
            .as_ref()
            .and_then(|p| p.name.clone())
            .or_else(|| self.agent_layer.as_ref().map(|a| a.name.clone()));

        // Denies: union of prompt + agent, de-duplicated. Composing means
        // a profile can only tighten an active prompt's restrictions.
        let mut deny: Vec<String> = self
            .prompt_layer
            .as_ref()
            .map(|p| p.deny_tools.clone())
            .unwrap_or_default();
        if let Some(a) = self.agent_layer.as_ref() {
            for t in a
                .tools
                .to_deny_list(crate::agent::tools::BUILTIN_TOOL_NAMES)
            {
                if !deny.contains(&t) {
                    deny.push(t);
                }
            }
        }
        self.current_prompt_deny_tools = deny;

        // Active-agent marker tracks the agent layer.
        self.current_agent = self.agent_layer.as_ref().map(|a| a.name.clone());
    }
}

impl ContextFiles {
    #[allow(dead_code)]
    pub fn reload(&mut self) {
        self.agents = load_agents();
        self.prompts = prompts::load();
        // Refresh the prompt layer's body/denies from the reloaded
        // definition (the agent layer is unaffected by a context reload),
        // then refold the effective state.
        if let Some(name) = self.prompt_layer.as_ref().and_then(|p| p.name.clone()) {
            match self.prompts.get(&name) {
                Some(p) => {
                    self.set_prompt_layer(Some(name), Some(p.body.clone()), p.deny_tools.clone())
                }
                None => self.clear_prompt_layer(),
            }
        }
    }
}

pub fn load(no_context_files: bool) -> ContextFiles {
    let _ = prompts::ensure_global();
    let agents = if no_context_files {
        None
    } else {
        load_agents()
    };
    let prompt_map = prompts::load();
    ContextFiles {
        agents,
        prompts: prompt_map,
        agent_defs: agent_defs::AgentRegistry::default(),
        current_agent: None,
        current_prompt: None,
        current_prompt_name: None,
        current_prompt_deny_tools: Vec::new(),
        prompt_layer: None,
        agent_layer: None,
        model_before_agent: None,
    }
}

fn load_file(path: &PathBuf) -> Option<String> {
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(path) {
        Ok(content) => Some(content),
        Err(e) => {
            // Previously the error was silently swallowed via `.ok()`
            // — a permission-denied AGENTS.md looked the same as a
            // missing file. Surface the path + reason at warn so
            // users can investigate when context they expected is
            // missing.
            eprintln!(
                "warning: failed to read context file {}: {}",
                path.display(),
                e,
            );
            None
        }
    }
}

fn load_agents() -> Option<String> {
    let mut parts: SmallVec<[String; 4]> = SmallVec::new();

    let global = storage::agents_path();
    if let Some(content) = load_file(&global)
        && !content.trim().is_empty()
    {
        parts.push(format!("# Global AGENTS.md\n{}", content));
    }

    // Batch2-2 (audit fix): cap the ancestor walk. Previously this
    // walked to / (typically 6-10 stat+open calls per startup on a
    // nested project) and would pick up any AGENTS.md/CLAUDE.md
    // under $HOME or /Users that the user didn't intend to apply
    // globally. opencode caps at the git root + $HOME — same here:
    //   1. Stop at the first ancestor that contains `.git/` (the
    //      project root for non-trivial cases).
    //   2. Stop at the user's $HOME if no git root found earlier.
    //   3. Hard cap at 16 levels as a defensive cliff.
    // The dedicated global path under `~/.config/dirge/agent/`
    // still loads independently above; that's the "global fallback"
    // the README documents.
    let cwd = std::env::current_dir().ok();
    if let Some(cwd) = cwd {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let mut current = Some(cwd.as_path());
        let mut depth = 0usize;
        const MAX_DEPTH: usize = 16;
        while let Some(dir) = current {
            for name in &["AGENTS.md", "CLAUDE.md"] {
                let path = dir.join(name);
                if let Some(content) = load_file(&path)
                    && !content.trim().is_empty()
                {
                    parts.push(format!("# {} ({})\n{}", name, dir.display(), content));
                }
            }

            // Stop if THIS dir is the git root — project boundary.
            // Checked AFTER loading so the project's own AGENTS.md
            // is included.
            if dir.join(".git").exists() {
                break;
            }
            // Stop if we're at the user's HOME — anything above that
            // is system territory and shouldn't bleed into the
            // agent's context.
            if let Some(ref h) = home
                && dir == h.as_path()
            {
                break;
            }
            depth += 1;
            if depth >= MAX_DEPTH {
                break;
            }
            current = dir.parent();
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

#[cfg(test)]
mod composition_tests {
    //! dirge-x7c8 / dirge-anhw: the `/prompt` and `/agent` layers compose
    //! instead of clobbering. Denies UNION, the agent body overrides the
    //! prompt body, the prompt owns the "mode" name, and `/agent off`
    //! cleanly restores the prompt layer.
    use super::*;
    use crate::context::agent_defs::{
        AgentDefinition, AgentSource, SubagentToolPolicy, ToolPolicy,
    };

    fn empty() -> ContextFiles {
        ContextFiles {
            agents: None,
            prompts: HashMap::new(),
            agent_defs: agent_defs::AgentRegistry::default(),
            current_agent: None,
            current_prompt: None,
            current_prompt_name: None,
            current_prompt_deny_tools: Vec::new(),
            prompt_layer: None,
            agent_layer: None,
            model_before_agent: None,
        }
    }

    fn agent(
        name: &str,
        body: Option<&str>,
        model: Option<&str>,
        tools: ToolPolicy,
    ) -> AgentDefinition {
        AgentDefinition {
            name: name.to_string(),
            prompt: body.map(String::from),
            model: model.map(String::from),
            tools,
            reasoning: None,
            temperature: None,
            description: None,
            subagent: SubagentToolPolicy::default(),
            source: AgentSource::ProjectFile,
        }
    }

    #[test]
    fn prompt_only_sets_effective_fields() {
        let mut c = empty();
        c.set_prompt_layer(
            Some("plan".into()),
            Some("PLAN BODY".into()),
            vec!["edit".into(), "write".into()],
        );
        assert_eq!(c.current_prompt.as_deref(), Some("PLAN BODY"));
        assert_eq!(c.current_prompt_name.as_deref(), Some("plan"));
        assert_eq!(c.current_prompt_deny_tools, vec!["edit", "write"]);
        assert_eq!(c.current_agent, None);
    }

    /// dirge-x7c8: activating an agent must NOT wipe the prompt's denies —
    /// the lists compose. The agent body overrides; the mode name stays.
    #[test]
    fn agent_composes_denies_does_not_clobber_prompt() {
        let mut c = empty();
        c.set_prompt_layer(
            Some("plan".into()),
            Some("PLAN BODY".into()),
            vec!["edit".into(), "write".into()],
        );
        c.set_agent_layer(agent(
            "implementer",
            Some("AGENT BODY"),
            Some("opus-alias"),
            ToolPolicy::Deny(vec!["bash".into()]),
        ));
        // Denies UNION — plan's edit/write survive, agent's bash adds.
        for t in ["edit", "write", "bash"] {
            assert!(
                c.current_prompt_deny_tools.iter().any(|d| d == t),
                "expected {t} in composed denies {:?}",
                c.current_prompt_deny_tools
            );
        }
        // Agent body wins; mode name stays the prompt's; agent marker set.
        assert_eq!(c.current_prompt.as_deref(), Some("AGENT BODY"));
        assert_eq!(c.current_prompt_name.as_deref(), Some("plan"));
        assert_eq!(c.current_agent.as_deref(), Some("implementer"));
    }

    /// `/agent off` pops only the agent layer and restores the prompt's
    /// body + denies (the revert half of dirge-x7c8).
    #[test]
    fn agent_off_restores_prompt_layer() {
        let mut c = empty();
        c.set_prompt_layer(
            Some("plan".into()),
            Some("PLAN BODY".into()),
            vec!["edit".into(), "write".into()],
        );
        c.set_agent_layer(agent(
            "a",
            Some("AGENT"),
            None,
            ToolPolicy::Deny(vec!["bash".into()]),
        ));
        c.clear_agent_layer();
        assert_eq!(c.current_prompt.as_deref(), Some("PLAN BODY"));
        assert_eq!(c.current_prompt_name.as_deref(), Some("plan"));
        assert_eq!(c.current_prompt_deny_tools, vec!["edit", "write"]);
        assert_eq!(c.current_agent, None);
    }

    /// Agent with no active prompt: it owns the mode name and its own
    /// body, and its denies stand alone.
    #[test]
    fn agent_only_falls_back_to_agent_name() {
        let mut c = empty();
        c.set_agent_layer(agent(
            "solo",
            None,
            None,
            ToolPolicy::Deny(vec!["bash".into()]),
        ));
        assert_eq!(c.current_prompt, None, "no prompt body, agent has none");
        assert_eq!(c.current_prompt_name.as_deref(), Some("solo"));
        assert_eq!(c.current_prompt_deny_tools, vec!["bash"]);
    }

    /// An `allow_tools` profile denies the complement of the allow-set
    /// over built-ins, still UNION-ed with the prompt's explicit denies.
    #[test]
    fn allow_policy_denies_complement_and_unions() {
        let mut c = empty();
        c.set_prompt_layer(
            Some("review".into()),
            Some("R".into()),
            vec!["webfetch".into()],
        );
        c.set_agent_layer(agent(
            "reader",
            None,
            None,
            ToolPolicy::Allow(vec!["read".into(), "grep".into()]),
        ));
        // `read`/`grep` allowed → NOT denied; `write` denied (complement);
        // prompt's `webfetch` deny preserved.
        assert!(!c.current_prompt_deny_tools.iter().any(|d| d == "read"));
        assert!(c.current_prompt_deny_tools.iter().any(|d| d == "write"));
        assert!(c.current_prompt_deny_tools.iter().any(|d| d == "webfetch"));
    }

    /// `/prompt default` (clear prompt layer) leaves an active agent's
    /// body + denies intact.
    #[test]
    fn clear_prompt_keeps_agent_layer() {
        let mut c = empty();
        c.set_prompt_layer(
            Some("plan".into()),
            Some("PLAN".into()),
            vec!["edit".into()],
        );
        c.set_agent_layer(agent(
            "a",
            Some("AGENT"),
            None,
            ToolPolicy::Deny(vec!["bash".into()]),
        ));
        c.clear_prompt_layer();
        assert_eq!(c.current_prompt.as_deref(), Some("AGENT"));
        assert_eq!(c.current_prompt_name.as_deref(), Some("a"));
        assert_eq!(c.current_prompt_deny_tools, vec!["bash"]);
        assert!(!c.current_prompt_deny_tools.iter().any(|d| d == "edit"));
    }
}
