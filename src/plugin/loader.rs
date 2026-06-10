//! Plugin file discovery, loading, and hook registration.
//!
//! Contains `load_plugin` (the single-file / directory-plugin loader)
//! and `HOOK_NAMES` (the centralised hook-name registry). Extracted
//! from `plugin/mod.rs` so the plugin module's public API stays lean.

/// All hook names the host knows about. Plugins define functions with
/// these names (bare or stem-prefixed) and the loader hooks them up.
/// Centralized so the loader and any future telemetry stay in sync.
pub const HOOK_NAMES: &[&str] = &[
    "on-init",
    "on-prompt",
    "on-response",
    "on-turn-start",
    "on-turn-end",
    "on-message-update",
    "on-tool-start",
    "on-tool-end",
    "on-error",
    "on-complete",
    "prepare-next-run",
    // dirge-wqxj: fires once before the agent starts; receives the
    // assembled system prompt; may call harness/append-system-prompt.
    "before-agent-start",
    // dirge-lsoq: fires after the assistant message finalizes; may
    // call harness/rewrite-message to replace the response text.
    "message-end",
    // dirge-264x: fires before each LLM call with the current
    // messages (JSON) in ctx :messages; may call
    // harness/replace-context to prune/inject for that call.
    "transform-context",
    // dirge-jia8: observe-only notification fired before a
    // compaction fold; receives ctx :message-count and :tokens.
    // Cannot cancel the fold.
    "on-before-compact",
    // dirge-jia8: fires when summarizing the middle slice; receives
    // ctx :messages (JSON); may call harness/set-compact-summary to
    // supply a summary instead of the LLM summarizer.
    "on-compact",
];

/// Filter an input candidate list to only paths that exist as
/// directories. Used by plugin directory discovery.
pub fn filter_existing_dirs(candidates: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    candidates.iter().filter(|p| p.is_dir()).cloned().collect()
}

/// Descriptor for a successfully-loaded plugin. Records the stem
/// name, files read, and which hooks were registered so the host
/// can report loading status and the hook dispatcher knows which
/// hooks are available. Files are listed in load order.
#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub stem: String,
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub files: Vec<std::path::PathBuf>,
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub hooks_registered: Vec<String>,
}

/// Discover, evaluate, and register a plugin from `path`.
///
/// `path` may be:
/// - A `*.janet` file — single-file plugin; stem = file stem.
/// - A directory — multi-file plugin; stem = directory name. All
///   `*.janet` files inside are loaded in alphabetical order into the
///   shared Janet env, so split files share state and `harness/*`
///   registrations.
///
/// After eval, any bare hook fns (`on-prompt`, `on-tool-start`, etc.)
/// get a `{stem}-{hook}` alias so they survive subsequent plugin loads
/// that would otherwise overwrite the bare symbol in the shared Janet
/// env. Then `{stem}-{hook}` is what we register for dispatch — that
/// way two plugins both defining `on-tool-start` no longer collide.
///
/// Returns the [`LoadedPlugin`] descriptor (stem + which files were
/// read + which hooks fired). Errors short-circuit: a malformed first
/// file aborts the whole plugin load.
pub fn load_plugin(
    mgr: &mut super::PluginManager,
    path: &std::path::Path,
) -> Result<LoadedPlugin, String> {
    let (stem, files) = if path.is_dir() {
        let dir_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("plugin dir has no name: {}", path.display()))?
            .to_string();
        let mut janet_files: Vec<std::path::PathBuf> = std::fs::read_dir(path)
            .map_err(|e| format!("cannot read plugin dir {}: {}", path.display(), e))?
            .filter_map(|e| e.ok().map(|x| x.path()))
            .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "janet"))
            .collect();
        janet_files.sort();
        if janet_files.is_empty() {
            return Err(format!(
                "plugin dir {} contains no .janet files",
                path.display()
            ));
        }
        (dir_name, janet_files)
    } else {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("plugin file has no stem: {}", path.display()))?
            .to_string();
        (stem, vec![path.to_path_buf()])
    };

    for file in &files {
        mgr.load_file(file)
            .map_err(|e| format!("failed to load {}: {}", file.display(), e))?;
    }

    // Promote any bare hook symbols to stem-prefixed copies so a later
    // plugin redefining the bare name can't shadow ours. We construct
    // the prefixed name at runtime via curenv-mutation because Janet's
    // `def` requires a literal symbol.
    let mut hooks_registered = Vec::new();
    for hook in HOOK_NAMES {
        let prefixed = format!("{}-{}", stem, hook);
        let escaped_hook = super::escape_janet_string(hook);
        let escaped_prefixed = super::escape_janet_string(&prefixed);
        let alias_code = format!(
            r#"(let [env (curenv)
                    bare-sym (symbol "{bare}")
                    prefixed-sym (symbol "{prefixed}")
                    bare-entry (get env bare-sym)]
                 (when (and bare-entry (not (get env prefixed-sym)))
                   (put env prefixed-sym bare-entry)))"#,
            bare = escaped_hook,
            prefixed = escaped_prefixed,
        );
        let _ = mgr.eval(&alias_code);
        if mgr.has_symbol(&prefixed) {
            mgr.register(hook, &prefixed);
            hooks_registered.push(hook.to_string());
        }
    }

    let loaded = LoadedPlugin {
        stem,
        files,
        hooks_registered,
    };
    mgr.push_loaded_plugin(loaded.clone());
    Ok(loaded)
}
