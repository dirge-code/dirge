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
    pub current_agent: Option<String>,
    pub current_prompt: Option<String>,
    pub current_prompt_name: Option<String>,
    /// Tools that are denied while `current_prompt_name` is active.
    /// Populated from the active prompt's frontmatter at switch time;
    /// consumed by the permission checker BEFORE rule matching so
    /// prompt-level mode restrictions (e.g. plan mode forbidding
    /// edit/write/apply_patch) are enforced at the security layer,
    /// not just via prose in the system prompt.
    pub current_prompt_deny_tools: Vec<String>,
}

impl ContextFiles {
    #[allow(dead_code)]
    pub fn reload(&mut self) {
        self.agents = load_agents();
        self.prompts = prompts::load();
        if let Some(name) = &self.current_prompt_name {
            if let Some(p) = self.prompts.get(name) {
                self.current_prompt = Some(p.body.clone());
                self.current_prompt_deny_tools = p.deny_tools.clone();
            } else {
                self.current_prompt = None;
                self.current_prompt_deny_tools.clear();
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
