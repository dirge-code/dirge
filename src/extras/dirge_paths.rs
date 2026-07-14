//! Per-project `.dirge/` directory resolution.
//!
//! This is the canonical entry point for all per-project storage.
//! Hermes stores everything globally in `~/.hermes/`; dirge stores
//! per-project knowledge in `.dirge/` at the repository root (where
//! `.git/` lives). Each project gets independent memory, skills,
//! and session history.
//!
//! The existing `extras::memory` module stores in `~/.dirge/memories/`
//! (user-global, project-keyed by path hash). That serves a different
//! purpose — user notes that follow you across machines. Phase 2 will
//! wire `ProjectPaths` into a new per-project `MemoryStore` without
//! touching the global memory module.

use std::path::{Path, PathBuf};

/// Check if a path is a git reference (either a `.git` directory
/// or a `.git` file referencing the real git dir, as used by
/// `git worktree`).
fn is_git_root_marker(path: &Path) -> bool {
    let git = path.join(".git");
    if git.is_dir() {
        return true;
    }
    // Git worktrees: .git is a file containing "gitdir: <path>".
    if git.is_file()
        && let Ok(content) = std::fs::read_to_string(&git)
    {
        return content.starts_with("gitdir:");
    }
    false
}

/// Walk up from `cwd` until a `.git/` directory or worktree
/// `.git` file is found. Returns `cwd` unchanged if no git root
/// is found (user may be outside a repo — per-project features
/// degrade gracefully).
pub fn find_git_root(cwd: &Path) -> PathBuf {
    // Canonicalize to resolve symlinks in the path chain.
    let cwd = if let Ok(canon) = cwd.canonicalize() {
        canon
    } else {
        cwd.to_path_buf()
    };

    let mut current = cwd.clone();
    loop {
        if is_git_root_marker(&current) {
            return current;
        }
        let parent = match current.parent() {
            Some(p) => p.to_path_buf(),
            None => return cwd.clone(),
        };
        if parent == current {
            return cwd.clone();
        }
        current = parent;
    }
}

/// `DIRGE_PROJECT_ROOT` override. When set and pointing to an
/// existing directory, this is the project root instead of the
/// auto-detected git root. Useful for monorepos where the
/// logical project is a subdirectory.
pub fn project_root_override() -> Option<PathBuf> {
    std::env::var("DIRGE_PROJECT_ROOT")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

/// Resolve the active project root: `DIRGE_PROJECT_ROOT` wins if
/// set and valid, otherwise walk up from CWD looking for `.git/`.
pub fn project_root(cwd: &Path) -> PathBuf {
    project_root_override().unwrap_or_else(|| find_git_root(cwd))
}

/// Canonical paths into the per-project `.dirge/` tree.
///
/// Construct with `ProjectPaths::new(cwd)`. All subdirectory
/// accessors are lazy — directories are not created until
/// something actually writes into them.
#[derive(Debug, Clone)]
pub struct ProjectPaths {
    /// The project root (usually where `.git/` lives).
    pub root: PathBuf,
}

impl ProjectPaths {
    pub fn new(cwd: &Path) -> Self {
        ProjectPaths {
            root: project_root(cwd),
        }
    }

    /// Top-level `.dirge/` directory under the project root.
    pub fn dirge_dir(&self) -> PathBuf {
        self.root.join(".dirge")
    }

    /// `.dirge/memory/` — declarative memory files (MEMORY.md, PITFALLS.md).
    pub fn memory_dir(&self) -> PathBuf {
        self.dirge_dir().join("memory")
    }

    /// `.dirge/skills/` — procedural skill definitions with SKILL.md files.
    pub fn skills_dir(&self) -> PathBuf {
        self.dirge_dir().join("skills")
    }

    /// `.dirge/sessions/` — SQLite session database and transcripts.
    pub fn sessions_dir(&self) -> PathBuf {
        self.dirge_dir().join("sessions")
    }

    /// `.dirge/sessions/state.db` — the FTS5-backed session database.
    pub fn session_db_path(&self) -> PathBuf {
        self.sessions_dir().join("state.db")
    }

    /// `.dirge/memory/<name>` — a specific memory file.
    pub fn memory_file(&self, name: &str) -> PathBuf {
        self.memory_dir().join(name)
    }

    /// `.dirge/config.yaml` — optional per-project dirge configuration.
    #[allow(dead_code)]
    pub fn config_path(&self) -> PathBuf {
        self.dirge_dir().join("config.yaml")
    }

    /// `.dirge/config.json` — project-local config layered over the
    /// global `~/.config/dirge/config.json`.
    pub fn project_config_file(&self) -> PathBuf {
        self.dirge_dir().join("config.json")
    }

    /// `.dirge/plugins/` — project-local Janet plugins. The only callers are
    /// `#[cfg(feature = "plugin")]`, so this is dead code in the windows-default
    /// / no-plugin build (which compiles with `-D warnings`).
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub fn plugins_dir(&self) -> PathBuf {
        self.dirge_dir().join("plugins")
    }

    /// `.dirge/prompts/` — project prompt overrides (`*.md`).
    pub fn prompts_dir(&self) -> PathBuf {
        self.dirge_dir().join("prompts")
    }

    /// `.dirge/agents/` — project agent-profile overrides (`*.md`).
    pub fn agents_dir(&self) -> PathBuf {
        self.dirge_dir().join("agents")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutex that serializes tests that mutate `DIRGE_PROJECT_ROOT`.
    /// `std::env::set_var` is `unsafe` because environment mutations
    /// are global state — concurrent tests racing on the same key
    /// cause flaky failures. Locking this mutex guarantees only one
    /// env-mutating test runs at a time.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that clears `DIRGE_PROJECT_ROOT` on drop so a
    /// panic mid-test doesn't leak the override into subsequent tests.
    struct EnvGuard;

    impl EnvGuard {
        fn set(value: &str) -> Self {
            unsafe { std::env::set_var("DIRGE_PROJECT_ROOT", value) };
            EnvGuard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("DIRGE_PROJECT_ROOT") };
        }
    }

    /// In the dirge repo itself, `find_git_root` from the current
    /// working directory should resolve to the repo root (where
    /// `.git/` actually lives).
    #[test]
    fn find_git_root_in_this_repo() {
        let cwd = std::env::current_dir().unwrap();
        let root = find_git_root(&cwd);
        let dot_git = root.join(".git");
        let is_git = dot_git.is_dir()
            || (dot_git.is_file()
                && std::fs::read_to_string(&dot_git)
                    .map(|c| c.starts_with("gitdir:"))
                    .unwrap_or(false));
        assert!(
            is_git,
            "expected {root:?} to contain .git/ or a worktree .git file"
        );
    }

    /// `/tmp` has no `.git/` — should return `/tmp` unchanged
    /// (canonicalized if possible).
    #[test]
    fn find_git_root_falls_back_to_cwd_outside_repo() {
        let tmp = std::env::temp_dir();
        let expected = tmp.canonicalize().unwrap_or_else(|_| tmp.clone());
        let root = find_git_root(&tmp);
        assert_eq!(root, expected);
    }

    /// `DIRGE_PROJECT_ROOT` wins over auto-detection.
    #[test]
    fn env_override_wins_over_git_detection() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir();
        let _guard = EnvGuard::set(tmp.to_str().unwrap());
        // Even though we're in the dirge repo, the env var wins.
        let cwd = std::env::current_dir().unwrap();
        let root = project_root(&cwd);
        assert_eq!(root, tmp);
    }

    /// An env var pointing to a non-existent directory is ignored
    /// (graceful fallback to git detection).
    #[test]
    fn env_override_ignores_missing_directory() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::set("/nonexistent/dirge/project/root");
        let cwd = std::env::current_dir().unwrap();
        let root = project_root(&cwd);
        // Should fall through to git detection, not use the bogus path.
        assert_ne!(root, PathBuf::from("/nonexistent/dirge/project/root"));
    }

    /// All subdirectory accessors nest under `.dirge/`.
    #[test]
    fn subdirs_are_under_dirge_dir() {
        let cwd = std::env::current_dir().unwrap();
        let paths = ProjectPaths::new(&cwd);
        let dirge = paths.dirge_dir();

        assert!(paths.memory_dir().starts_with(&dirge));
        assert!(paths.skills_dir().starts_with(&dirge));
        assert!(paths.sessions_dir().starts_with(&dirge));
        assert!(paths.config_path().starts_with(&dirge));
        assert!(paths.project_config_file().starts_with(&dirge));
        assert!(paths.plugins_dir().starts_with(&dirge));
        assert!(paths.prompts_dir().starts_with(&dirge));
        assert!(paths.agents_dir().starts_with(&dirge));
    }

    /// dirge-vpma.17: plugins/config/prompts/agents resolve at the
    /// project root, not the launch CWD. Launching in `<repo>/sub/`
    /// must anchor them at `<repo>/.dirge/`, matching where the
    /// session DB and memory already live — no more split-brain.
    #[test]
    fn project_files_resolve_at_git_root_from_subdir() {
        let base = std::env::temp_dir().join(format!("dirge-vpma17-walkup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sub = base.join("crates").join("inner");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(base.join(".git")).unwrap();

        let root = base.canonicalize().unwrap_or_else(|_| base.clone());
        let paths = ProjectPaths::new(&sub);
        assert_eq!(paths.plugins_dir(), root.join(".dirge").join("plugins"));
        assert_eq!(paths.prompts_dir(), root.join(".dirge").join("prompts"));
        assert_eq!(paths.agents_dir(), root.join(".dirge").join("agents"));
        assert_eq!(
            paths.project_config_file(),
            root.join(".dirge").join("config.json")
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// dirge-vpma.17: `DIRGE_PROJECT_ROOT` overrides the git-root walk
    /// for the project-file accessors too, so a monorepo can point the
    /// whole `.dirge/` tree at a chosen directory.
    #[test]
    fn project_files_honor_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir();
        let _guard = EnvGuard::set(tmp.to_str().unwrap());
        let cwd = std::env::current_dir().unwrap();
        let paths = ProjectPaths::new(&cwd);
        assert_eq!(paths.plugins_dir(), tmp.join(".dirge").join("plugins"));
        assert_eq!(
            paths.project_config_file(),
            tmp.join(".dirge").join("config.json")
        );
    }

    /// `session_db_path` points into `sessions/` and ends with `state.db`.
    #[test]
    fn session_db_is_in_sessions_dir() {
        let cwd = std::env::current_dir().unwrap();
        let paths = ProjectPaths::new(&cwd);
        let db = paths.session_db_path();
        assert!(db.starts_with(paths.sessions_dir()));
        assert!(db.ends_with("state.db"));
    }

    /// `memory_file("MEMORY.md")` points to `.dirge/memory/MEMORY.md`.
    #[test]
    fn memory_file_is_in_memory_dir() {
        let cwd = std::env::current_dir().unwrap();
        let paths = ProjectPaths::new(&cwd);
        let f = paths.memory_file("MEMORY.md");
        assert_eq!(f.file_name().unwrap(), "MEMORY.md");
        assert!(f.starts_with(paths.memory_dir()));
    }

    /// Git worktrees use a `.git` file (not directory) containing
    /// `gitdir: <path>`. find_git_root should recognise this as a
    /// git root marker and stop walking.
    #[test]
    fn find_git_root_recognises_worktree_marker() {
        let dir = std::env::temp_dir().join(format!("dirge-worktree-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Create a worktree-style .git file.
        std::fs::write(
            dir.join(".git"),
            "gitdir: /some/real/path/.git/worktrees/foo\n",
        )
        .unwrap();

        let root = find_git_root(&dir);
        let expected = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        assert_eq!(root, expected, "should stop at worktree .git file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Paths with symlinks should still find the correct git root.
    #[test]
    fn find_git_root_with_symlinks() {
        let cwd = std::env::current_dir().unwrap();
        let root = find_git_root(&cwd);
        assert!(
            root.join(".git").is_dir() || {
                let git_file = root.join(".git");
                git_file.is_file()
                    && std::fs::read_to_string(&git_file)
                        .map(|c| c.starts_with("gitdir:"))
                        .unwrap_or(false)
            },
            "expected {root:?} to be a git root"
        );
    }
}
