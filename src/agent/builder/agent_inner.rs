//! The core agent constructor. Split out of `agent/builder.rs`
//! (dirge-4y4l stage 11c): `build_agent_inner` assembles the rig `Agent`'s
//! preamble (system prompt) and attaches the provider model. Post phase
//! 4.5h-6 it no longer builds the tool registry — the loop dispatches against
//! the `LoopTool` set from `build_loop_tools`, the single source of truth
//! [dirge-tfip]. Preamble-assembly helpers come from the sibling modules via
//! the parent's re-exports.

use rig::agent::{Agent, AgentBuilder};
use rig::completion::CompletionModel;
use std::sync::Arc;

use crate::agent::model_family::resolve_family;
use crate::agent::prompt::PROJECT_SKILLS_PREAMBLE;
use crate::agent::tools::ToolCache;
use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;

use super::{
    append_memory_to_preamble, append_mode_reminder, assemble_base_preamble,
    model_steering_fragment,
};

// Post phase 4.5h-6 the rig `Agent` this builds is retained ONLY for its
// `.preamble` (system prompt) and `.model` — the live loop dispatches through
// the `LoopTool` registry from `build_loop_tools`, which is the single source
// of truth for the tool set. So `build_agent_inner` no longer needs the wide
// tool-wiring signature (permission, channels, managers, …); those now flow
// only to `build_loop_tools` [dirge-tfip].
pub async fn build_agent_inner<M: CompletionModel + 'static>(
    model: M,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
    // The ACTIVE provider + model identifiers (post `/model` / `/agent`
    // swap), used for model-family steering. Passing them explicitly
    // fixes dirge-5db6: `cli.resolve_model`/`resolve_provider` only see
    // the launch-time CLI/config model, so steering would otherwise lag a
    // mid-session swap (false negative switching TO DeepSeek, false
    // positive switching away).
    active_provider: &str,
    active_model: &str,
) -> (
    Agent<M>,
    ToolCache,
    // dirge-7tvq: surface the constructed MemoryProvider so the
    // caller (provider::build_agent) can attach it to AnyAgent for
    // session-lifecycle hook dispatch. `None` when load failed.
    Option<Arc<dyn crate::extras::memory_provider::MemoryProvider>>,
) {
    // The `plan_file`-keyed gate on edit/write/apply_patch was
    // removed: prompt-level tool restrictions now live in the
    // prompt file's frontmatter (`deny_tools: [...]`), enforced
    // at the permission-checker layer. Plan / review modes deny
    // edit/write/apply_patch/bash entirely, so the file-name gate
    // is unnecessary.
    let mut preamble = assemble_base_preamble();
    if let Some(agents) = &context.agents {
        preamble.push_str("\n\n");
        preamble.push_str(agents);
    }

    if let Some(prompt) = &context.current_prompt {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(prompt);
    }

    if let Ok(cwd) = std::env::current_dir() {
        let cwd_str = cwd.display();
        preamble.push_str(&format!("\n\nCurrent working directory: {}", cwd_str));
    }

    preamble.push_str(&format!("\nOS: {}", std::env::consts::OS));

    if let Ok(shell) = std::env::var("SHELL") {
        preamble.push_str(&format!("\nShell: {}", shell));
    }

    // Bounded git lookup. `git rev-parse` can hang for many seconds
    // when the repo's `.git` lives on a wedged NFS mount, the
    // `core.fsmonitor` daemon is stalled, or a `.gitconfig` `[include]`
    // points at a path that itself blocks (e.g. another stalled
    // network mount). 2 s is well over a healthy local `git` (≪ 50 ms)
    // — anything longer is the user's git misbehaving, and we'd
    // rather show the banner without a branch than hang dirge's
    // entire startup.
    let git_branch_fut = tokio::task::spawn_blocking(|| {
        std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|output| {
                if output.status.success() {
                    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !branch.is_empty() {
                        Some(branch)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
    });
    let git_branch =
        match tokio::time::timeout(std::time::Duration::from_secs(2), git_branch_fut).await {
            Ok(Ok(branch)) => branch,
            // spawn_blocking JoinError or wall-clock expiry: degrade
            // gracefully. The spawned thread keeps running in the
            // background until git returns; we simply stop awaiting
            // it. No leak — once the OS kernel reaps the git child,
            // the thread exits naturally.
            _ => None,
        };

    if let Some(branch) = git_branch {
        preamble.push_str(&format!("\nGit branch: {}", branch));
    }

    // Phase 8: inject per-project memory + skills into the system
    // prompt. Frozen snapshots of MEMORY.md and PITFALLS.md become
    // reference material for every turn. Skills from .dirge/skills/
    // and global dirs are listed so the model knows what procedural
    // knowledge is available (it loads them on demand via the
    // `skill` tool).
    let paths = std::env::current_dir()
        .map(|c| crate::extras::dirge_paths::ProjectPaths::new(&c))
        .unwrap_or_else(|_| {
            crate::extras::dirge_paths::ProjectPaths::new(std::path::Path::new("."))
        });
    // dirge-dktb: `SqliteMemoryStore::load` performs synchronous DB
    // I/O (open, migrate, possible legacy-markdown import). On slow
    // filesystems (NFS, network mounts) this blocks the async runtime
    // worker thread during agent construction. Move the synchronous
    // load onto the blocking pool, mirroring the
    // `skill::discover_skills` shape above. `unwrap_or_default()`
    // collapses both a `spawn_blocking` JoinError and a load error
    // into `None`, which matches the previous `Err(_) => None` branch.
    let paths_for_mem = paths.clone();
    let memory_load_result: Result<crate::extras::memory_db::SqliteMemoryStore, String> =
        tokio::task::spawn_blocking(move || {
            crate::extras::memory_db::SqliteMemoryStore::load(&paths_for_mem)
        })
        .await
        .unwrap_or_else(|_| Err("spawn_blocking join failed".to_string()));
    // dirge-fmau: route the preamble snapshot through the
    // `MemoryProvider` trait so a non-default backend's prompt block
    // appears too. The unsizing coercion from `Arc<MemoryToolStore>`
    // to `Arc<dyn MemoryProvider>` is the only call-site change.
    //
    // dirge-4hld: this provider feeds the preamble snapshot and lifecycle
    // hooks ONLY — it is the plain BM25 store, NOT the hybrid-wrapped one. The
    // search-serving instance is built in `build_loop_tools` (the single
    // source of truth for the tool set); `format_for_system_prompt` and the
    // hooks delegate to the inner store either way, so the two are equivalent
    // here. If anything ever calls `search()` on THIS handle it would bypass
    // hybrid — route such callers through the tool instead.
    let memory_store: Option<Arc<dyn crate::extras::memory_provider::MemoryProvider>> =
        match memory_load_result {
            Ok(store) => {
                let provider: Arc<dyn crate::extras::memory_provider::MemoryProvider> =
                    Arc::new(store);
                append_memory_to_preamble(&mut preamble, &provider);
                Some(provider)
            }
            Err(_) => None,
        };
    // Global (cross-project) memory tier — inject its snapshot too, under a
    // distinct header, so durable user preferences reach the prompt
    // regardless of which project this is. Best-effort: a load failure just
    // omits the global block.
    if let Ok(global) =
        tokio::task::spawn_blocking(crate::extras::memory_db::SqliteMemoryStore::load_global)
            .await
            .unwrap_or_else(|_| Err("spawn_blocking join failed".to_string()))
    {
        let global_provider: Arc<dyn crate::extras::memory_provider::MemoryProvider> =
            Arc::new(global);
        crate::agent::builder::preamble::append_global_memory_to_preamble(
            &mut preamble,
            &global_provider,
        );
    }
    // Inject the active spec change (if any) so a resumed or fresh session
    // knows which change it's implementing and where it left off, without
    // first querying the `spec` tool. Best-effort; synchronous DB I/O runs
    // on the blocking pool like the memory load above.
    let paths_for_spec = paths.clone();
    if let Ok(block) = tokio::task::spawn_blocking(move || {
        crate::extras::spec_db::SpecStore::open(&paths_for_spec)
            .map(|s| s.format_active_change_for_prompt())
    })
    .await
    .unwrap_or_else(|_| Err("spawn_blocking join failed".to_string()))
        && !block.trim().is_empty()
    {
        preamble.push_str(&block);
    }
    let skill_manager = crate::extras::skills::manager::SkillManager::new(&paths);
    let mut usage_store = crate::extras::skills::usage::UsageStore::load(&paths).ok();

    // Inject available project skills into the preamble so the
    // model knows what procedural knowledge exists for this project.
    // Skills are listed with name + description; the model loads
    // full content on demand via the `skill` tool.
    // Bumps view counters for each listed skill (best-effort).
    match skill_manager.list() {
        Ok(names) if !names.is_empty() => {
            let mut skill_lines = Vec::new();
            for name in &names {
                if let Ok(content) = skill_manager.read_content(name)
                    && let Some(spec) =
                        crate::extras::skills::format::parse_skill_spec(&content, name)
                {
                    let desc = if spec.description.is_empty() {
                        "(no description)".to_string()
                    } else {
                        spec.description.clone()
                    };
                    skill_lines.push(format!("  - **{name}**: {desc}"));
                }
            }
            if !skill_lines.is_empty() {
                preamble.push_str(PROJECT_SKILLS_PREAMBLE);
                for line in &skill_lines {
                    preamble.push_str(line);
                    preamble.push('\n');
                }
                // Bump view counters for each skill listed in preamble (best-effort).
                if let Some(ref mut u) = usage_store {
                    for name in &names {
                        u.record_view(name);
                    }
                }
            }
        }
        _ => {}
    }

    // Inject mode-specific reminders
    if let Some(prompt_name) = &context.current_prompt_name {
        let plan_exists = std::env::current_dir()
            .unwrap_or_else(|_| ".".into())
            .join("PLAN.md")
            .exists();
        append_mode_reminder(&mut preamble, prompt_name, plan_exists);
    }

    // Model-aware steering. DeepSeek chat models get a research-backed
    // guidance fragment; appended last so it's nearest the action
    // boundary, resisting prompt-distance drift. No-op for other models.
    let family = resolve_family(active_provider, active_model);
    if let Some(fragment) = model_steering_fragment(family) {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(fragment);
    }

    let mut builder = AgentBuilder::new(model).preamble(&preamble);

    let max_tokens = cli.resolve_max_tokens(cfg);
    builder = builder.max_tokens(max_tokens);

    let max_turns = cli.resolve_max_agent_turns(cfg);
    builder = builder.default_max_turns(max_turns);

    // Temperature: CLI > config > unset. Previously only `cli.temperature`
    // was checked, so users couldn't set a default in config.json.
    if let Some(temp) = cli.resolve_temperature(cfg) {
        let clamped = temp.clamp(0.0, 2.0);
        if (clamped - temp).abs() > f64::EPSILON {
            // Warn ONCE per process if the user's value was clamped
            // — previously silent, so a user with `temperature: 3.5`
            // got 2.0 and never knew.
            static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            if WARNED.set(()).is_ok() {
                eprintln!(
                    "warning: temperature {} clamped to {} (valid range 0.0..=2.0)",
                    temp, clamped,
                );
            }
        }
        builder = builder.temperature(clamped);
    }

    // Phase 3 / part 2: install configured inline-output budgets
    // for the disk-backed-output relay. `set_thresholds` writes
    // process-wide statics read by `relay_if_large` on every
    // bash/webfetch call. Done once at builder time — re-calling
    // with the same values is a cheap atomic store.
    crate::agent::tools::output_relay::set_thresholds(
        cfg.tools
            .as_ref()
            .and_then(|t| t.bash_output_inline_max_bytes),
        cfg.tools
            .as_ref()
            .and_then(|t| t.webfetch_output_inline_max_bytes),
        cfg.tools
            .as_ref()
            .and_then(|t| t.task_output_inline_max_bytes),
    );

    // No tools are attached to the rig Agent: the loop dispatches against the
    // `LoopTool` registry from `build_loop_tools` (which independently honors
    // `--no-tools`, collects MCP/semantic tools, and applies plugin hooks).
    // Attaching them here too only duplicated every tool construction and
    // double-collected MCP tools at startup [dirge-tfip].
    (builder.build(), ToolCache::new(), memory_store)
}
