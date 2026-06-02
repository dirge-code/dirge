//! Curator — background skill maintenance.
//!
//! Port of Hermes's `agent/curator.py`. Periodically reviews and
//! maintains agent-created skills: transitions stale skills to
//! archive, consolidates overlapping skills, keeps the skill
//! library healthy.
//!
//! Key design decisions from Hermes preserved:
//! - Automatic transitions (no LLM) for time-based lifecycle
//! - Optional review fork (with LLM) for consolidation
//! - Strict invariants: only agent-created, never delete, pinned bypass
//! - Persistent scheduler state in `.dirge/skills/.curator_state`
//! - Interval gates to avoid running too frequently
//! - Idle check to avoid running during active sessions
//!
//! dirge-odv3: the LLM consolidation pass (`CURATOR_PROMPT` +
//! `render_candidate_list` + `agent::review::spawn_curator_review`)
//! is ported from hermes `curator.py:330-460` (the
//! `CURATOR_REVIEW_PROMPT` + `_render_candidate_list` block) and
//! `curator.py:1369-1555` (the `run_curator_review` loop).

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use crate::extras::dirge_paths::ProjectPaths;

// ── Default configuration ─────────────────────────────

/// Days since last activity to mark a skill as stale.
const STALE_AFTER_DAYS: u64 = 30;

/// Days of staleness before archiving a skill.
const ARCHIVE_AFTER_STALE_DAYS: u64 = 90;

/// Minimum hours between curator runs.
const INTERVAL_HOURS: u64 = 168; // 7 days

/// Minimum hours of idle time before curator runs.
#[allow(dead_code)]
const IDLE_HOURS: u64 = 2;

// ── Curator state ─────────────────────────────────────

/// Persistent scheduler state written to `.dirge/skills/.curator_state`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CuratorState {
    /// Unix timestamp (seconds) of the last curator run.
    /// `None` means never run (different from epoch-0 which
    /// is a valid timestamp on some systems).
    last_run: Option<u64>,
    /// Timestamp when the state was first seeded.
    first_check: u64,
}

impl CuratorState {
    fn new() -> Self {
        let now = now_secs();
        CuratorState {
            last_run: None,
            first_check: now,
        }
    }

    fn load(path: &PathBuf) -> Result<Self, String> {
        if !path.exists() {
            return Ok(CuratorState::new());
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read curator state: {e}"))?;
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse curator state: {e}"))
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create curator state directory: {e}"))?;
        }
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize curator state: {e}"))?;
        crate::fs_atomic::atomic_write_sync(path, content.as_bytes())
            .map_err(|e| format!("Failed to write curator state: {e}"))
    }
}

// ── Curator ───────────────────────────────────────────

/// Skill lifecycle manager. Runs periodic maintenance on
/// agent-created skills in `.dirge/skills/`.
pub struct Curator {
    paths: ProjectPaths,
    state: CuratorState,
    state_path: PathBuf,
}

/// The lifecycle state of a skill, as tracked by the curator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SkillLifecycle {
    Active,
    Stale,
    Archived,
}

impl Curator {
    pub fn new(paths: &ProjectPaths) -> Result<Self, String> {
        let state_path = paths.skills_dir().join(".curator_state");
        let state = CuratorState::load(&state_path)?;
        Ok(Curator {
            paths: paths.clone(),
            state,
            state_path,
        })
    }

    /// Check whether the curator should run now.
    /// 1. First-run deferral — on the first-ever check, SEED the
    ///    clock (persist `last_run = now`) and return false, so the
    ///    pass first runs one interval after install.
    /// 2. Interval gate — afterwards, run when the last run was
    ///    >= INTERVAL_HOURS ago.
    ///
    /// dirge-6js7 fix: this previously returned false on the first
    /// check WITHOUT persisting anything. Since the only code that
    /// sets `last_run` lives inside the gated `apply_automatic_transitions`,
    /// `last_run` stayed `None` forever and the curator DEADLOCKED —
    /// it never ran in production. Seeding here breaks the deadlock.
    /// Takes `&mut self` because seeding is a persisted side effect.
    pub fn should_run_now(&mut self) -> bool {
        match self.state.last_run {
            None => {
                self.state.last_run = Some(now_secs());
                if let Err(e) = self.state.save(&self.state_path) {
                    tracing::debug!(
                        target: "dirge::curator",
                        error = %e,
                        "failed to seed curator state on first check",
                    );
                }
                false
            }
            Some(last) => {
                let elapsed = Duration::from_secs(now_secs().saturating_sub(last));
                elapsed >= Duration::from_secs(INTERVAL_HOURS * 3600)
            }
        }
    }

    /// Run automatic lifecycle transitions on all skills.
    /// No LLM involved — pure time-based rules.
    ///
    /// Returns a list of skills that should be considered for
    /// consolidation review (stale for > ARCHIVE_AFTER_STALE_DAYS
    /// but not yet archived).
    pub fn apply_automatic_transitions(&mut self) -> Result<Vec<String>, String> {
        let now = now_secs();
        let skills_dir = self.paths.skills_dir();

        if !skills_dir.is_dir() {
            self.state.last_run = Some(now);
            self.state.save(&self.state_path)?;
            return Ok(Vec::new());
        }

        // Load usage tracking for pin/activity checks.
        let mut usage = crate::extras::skills::usage::UsageStore::load(&self.paths).ok();

        let mut stale_names: Vec<String> = Vec::new();
        let mut reactivated: Vec<String> = Vec::new();

        for entry in std::fs::read_dir(&skills_dir)
            .map_err(|e| format!("Failed to read skills directory: {e}"))?
        {
            let entry = entry.map_err(|e| format!("Failed to read skill entry: {e}"))?;
            let path = entry.path();

            // Only process directories with SKILL.md.
            if !path.is_dir() || !path.join("SKILL.md").is_file() {
                continue;
            }

            // Skip archived skills (already in .archive/).
            let file_name = path.file_name().and_then(|n| n.to_str());
            if file_name == Some(".archive") {
                continue;
            }

            let name = match file_name {
                Some(n) => n.to_string(),
                None => continue,
            };

            // Skip pinned skills — they're exempt from all auto-transitions.
            if let Some(ref usage) = usage {
                if usage.get(&name).map(|u| u.pinned).unwrap_or(false) {
                    continue;
                }
                // Skip bundled skills (not agent-created).
                if !usage.is_agent_created(&name) {
                    // Bundled skill — skip transition but still track.
                    continue;
                }
            }

            // Get activity age from usage tracking if available,
            // fall back to file modification time.
            let age_seconds = if let Some(ref usage) = usage {
                usage.activity_age_seconds(&name).unwrap_or_else(|| {
                    // Fallback: compute from file modification time.
                    file_mod_age(&path.join("SKILL.md"), now)
                })
            } else {
                file_mod_age(&path.join("SKILL.md"), now)
            };

            let age_days = age_seconds / 86400;

            if age_days >= ARCHIVE_AFTER_STALE_DAYS {
                // Archive this skill.
                self.archive_skill(&name)?;
                // Update usage state if loaded.
                if let Some(ref mut u) = usage {
                    let _ = u.set_state(&name, crate::extras::skills::usage::SkillState::Archived);
                }
            } else if age_days >= STALE_AFTER_DAYS {
                stale_names.push(name.clone());
                if let Some(ref mut u) = usage {
                    let _ = u.set_state(&name, crate::extras::skills::usage::SkillState::Stale);
                }
            } else {
                // Recent activity on a stale skill → reactivate.
                let needs_reactivate = match usage.as_ref() {
                    Some(u) => u
                        .get(&name)
                        .map(|r| matches!(r.state, crate::extras::skills::usage::SkillState::Stale))
                        .unwrap_or(false),
                    None => false,
                };
                if needs_reactivate && let Some(ref mut u) = usage {
                    let _ = u.set_state(&name, crate::extras::skills::usage::SkillState::Active);
                    reactivated.push(name);
                }
            }
        }

        if !reactivated.is_empty() {
            tracing::info!(
                target: "dirge::curator",
                count = %reactivated.len(),
                "Reactivated {} stale skills with recent activity",
                reactivated.len()
            );
        }

        self.state.last_run = Some(now);
        self.state.save(&self.state_path)?;

        Ok(stale_names)
    }

    /// Move a skill to the `.archive/` directory.
    pub(crate) fn archive_skill(&self, name: &str) -> Result<(), String> {
        let src = self.paths.skills_dir().join(name);
        if !src.is_dir() {
            return Ok(());
        }

        let archive_dir = self.paths.skills_dir().join(".archive");
        std::fs::create_dir_all(&archive_dir)
            .map_err(|e| format!("Failed to create archive directory: {e}"))?;

        let dest = archive_dir.join(name);
        // If destination already exists, the skill was already
        // archived (possibly by a concurrent curator process).
        // Skip cleanly rather than removing and risking data loss.
        if dest.exists() {
            return Ok(());
        }

        std::fs::rename(&src, &dest)
            .map_err(|e| format!("Failed to archive skill '{}': {}", name, e))?;

        Ok(())
    }

    /// Record a curator run (for callers that want to force-update
    /// state after a manual run).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn record_run(&mut self) -> Result<(), String> {
        self.state.last_run = Some(now_secs());
        self.state.save(&self.state_path)
    }
}

// ── LLM consolidation pass (dirge-odv3) ───────────────

/// Prompt for the curator's umbrella-consolidation LLM pass. Ported
/// from hermes `agent/curator.py:330-460` (`CURATOR_REVIEW_PROMPT`),
/// adapted for dirge's combined `skill` tool (action='patch' /
/// 'create' / 'delete') instead of hermes's separate `skill_view` +
/// `skill_manage` pair. Skill content lives under `.dirge/skills/`,
/// archives under `.dirge/skills/.archive/`.
pub const CURATOR_PROMPT: &str = "You are running as dirge's background skill CURATOR. \
    This is an UMBRELLA-BUILDING consolidation pass, not a passive audit and not a \
    duplicate-finder.\n\n\
    The goal of the skill collection is a LIBRARY OF CLASS-LEVEL INSTRUCTIONS AND \
    EXPERIENTIAL KNOWLEDGE. A collection of hundreds of narrow skills where each one \
    captures one session's specific bug is a FAILURE of the library — not a feature. \
    An agent searching skills matches on descriptions, not on exact names; one broad \
    umbrella skill with labeled subsections beats five narrow siblings for \
    discoverability, not the other way around.\n\n\
    The right target shape is CLASS-LEVEL skills with rich SKILL.md bodies — not \
    one-session-one-skill micro-entries.\n\n\
    Hard rules — do not violate:\n\
    1. DO NOT touch bundled or hub-installed skills. The candidate list below is \
    already filtered to agent-created skills only.\n\
    2. DO NOT call `skill(action='delete', ...)` unless you've ALREADY absorbed the \
    skill's content into an umbrella via `skill(action='patch', ...)`. Deletion \
    moves the directory to `.dirge/skills/.archive/`; archives are recoverable but \
    the content is gone from the live library.\n\
    3. DO NOT touch skills shown as pinned=yes. Skip them entirely.\n\
    4. DO NOT use usage counters as a reason to skip consolidation. The counters are \
    new and often mostly zero. Judge overlap on CONTENT, not on use_count. 'use=0' \
    is not evidence a skill is valuable; it's absence of evidence either way.\n\
    5. DO NOT reject consolidation on the grounds that 'each skill has a distinct \
    trigger'. Pairwise distinctness is the wrong bar. The right bar is: 'would a \
    human maintainer write this as N separate skills, or as one skill with N \
    labeled subsections?' When the answer is the latter, merge.\n\n\
    How to work — not optional:\n\
    1. Scan the full candidate list. Identify PREFIX CLUSTERS (skills sharing a \
    first word or domain keyword).\n\
    2. For each cluster with 2+ members, do NOT ask 'are these pairs overlapping?' — \
    ask 'what is the UMBRELLA CLASS these skills all serve? Would a maintainer name \
    that class and write one skill for it?' If yes, pick (or create) the umbrella \
    and absorb the siblings into it.\n\
    3. Three ways to consolidate — use the right one per cluster:\n\
    \u{0020}  a. MERGE INTO EXISTING UMBRELLA — one skill in the cluster is already \
    broad enough. Use `skill(action='load', name=<umbrella>)` to read it, then \
    `skill(action='patch', name=<umbrella>, old_string=..., new_string=...)` to \
    add a labeled section for each sibling's unique insight, then \
    `skill(action='delete', name=<sibling>)` to archive the siblings.\n\
    \u{0020}  b. CREATE A NEW UMBRELLA SKILL — no existing member is broad enough. \
    Use `skill(action='create', name=<umbrella>, content=...)` to write a new \
    class-level skill whose SKILL.md covers the shared workflow with short \
    labeled subsections. Archive the now-absorbed narrow siblings.\n\
    \u{0020}  c. KEEP NARROW — only if the skill is already a class-level umbrella \
    and none of the proposed merges would improve discoverability.\n\
    4. Also flag skills whose NAME is too narrow (contains a PR number, a feature \
    codename, a specific error string). These almost always belong as a subsection \
    under a class-level umbrella.\n\
    5. Iterate. After one consolidation round, scan the remaining set and look for \
    the NEXT umbrella opportunity. Don't stop after 3 merges.\n\n\
    Your toolset (only the `skill` tool is available):\n\
    \u{0020}  - `skill(action='list')`                       — re-list current skills\n\
    \u{0020}  - `skill(action='load', name=...)`             — read a skill's SKILL.md\n\
    \u{0020}  - `skill(action='patch', name=..., old_string=..., new_string=...)` — \
    add sections to an umbrella\n\
    \u{0020}  - `skill(action='create', name=..., content=...)` — create a new \
    umbrella SKILL.md\n\
    \u{0020}  - `skill(action='delete', name=...)`           — archive a sibling \
    (after absorbing its content elsewhere)\n\n\
    'keep' is a legitimate decision ONLY when the skill is already class-level and \
    none of the proposed merges would improve discoverability. 'This is narrow but \
    distinct from its siblings' is NOT a reason to keep — it's a reason to move it \
    under an umbrella as a subsection.\n\n\
    Candidate list follows. Process it. When done, write a brief summary of what \
    you consolidated and what you left alone.";

/// Render the agent-created skill candidate list for the curator
/// review prompt. Port of hermes `_render_candidate_list`
/// (curator.py:~1350). One row per skill with the telemetry fields
/// the curator uses to judge consolidation overlap.
///
/// `usage` is the skill telemetry store. Only entries flagged as
/// agent-created (i.e. `is_agent_created` returns true) appear, and
/// pinned skills are flagged so the LLM can skip them per Hard Rule 3.
pub fn render_candidate_list(usage: &crate::extras::skills::usage::UsageStore) -> String {
    use std::fmt::Write as _;

    let mut rows: Vec<(&String, &crate::extras::skills::usage::SkillUsage)> = usage
        .skill_names()
        .filter(|name| usage.is_agent_created(name))
        .filter_map(|name| usage.get(name).map(|u| (name, u)))
        .collect();
    if rows.is_empty() {
        return String::from("No agent-created skills — curator pass is a no-op.");
    }
    // Sort by last activity (newest first) so the model sees fresh
    // additions at the top of its window. Falls back to name for ties.
    rows.sort_by(|a, b| {
        let key_a = a.1.last_used_at.as_deref().unwrap_or("");
        let key_b = b.1.last_used_at.as_deref().unwrap_or("");
        key_b.cmp(key_a).then_with(|| a.0.cmp(b.0))
    });

    let mut out = String::from("Candidate skills (agent-created, sorted by last activity):\n");
    for (name, u) in rows {
        let activity = u
            .last_used_at
            .as_deref()
            .or(u.last_patched_at.as_deref())
            .or(u.last_viewed_at.as_deref())
            .unwrap_or("never");
        let state = match u.state {
            crate::extras::skills::usage::SkillState::Active => "active",
            crate::extras::skills::usage::SkillState::Stale => "stale",
            crate::extras::skills::usage::SkillState::Archived => "archived",
        };
        let _ = writeln!(
            out,
            "  - {name}  state={state}  pinned={}  use={}  view={}  patches={}  last_activity={activity}",
            if u.pinned { "yes" } else { "no" },
            u.use_count,
            u.view_count,
            u.patch_count,
        );
    }
    out
}

// ── Per-run report (dirge-3m4h) ───────────────────────

/// One curator-run audit record. Port of hermes
/// `_write_run_report` (curator.py:970-1146), simplified — dirge
/// stores Markdown only (no JSON sidecar) and a compact diff
/// instead of full SKILL.md snapshots.
#[derive(Debug, Clone)]
pub struct CuratorReport {
    pub started_at_rfc3339: String,
    pub elapsed_secs: f64,
    /// Rendered candidate list BEFORE the LLM pass — the input
    /// the model was given.
    pub before_candidates: String,
    /// Rendered candidate list AFTER the pass — useful to
    /// eyeball what the model actually did.
    pub after_candidates: String,
    /// Tool-call names the model fired, in order. Duplicates
    /// preserved so the reader can see fan-out.
    pub tool_actions: Vec<String>,
    /// Optional error message captured from the agent stream.
    pub error: Option<String>,
}

impl CuratorReport {
    /// Render as Markdown for human consumption. Sections:
    /// metadata header, tool-action histogram, diff
    /// (removed/added/state-transition counts), before/after
    /// candidate dumps. Best-effort — never panics on malformed
    /// inputs.
    pub fn to_markdown(&self) -> String {
        use std::collections::BTreeMap;
        use std::fmt::Write as _;

        let mut out = String::new();
        let _ = writeln!(out, "# Curator run report\n");
        let _ = writeln!(out, "- Started: {}", self.started_at_rfc3339);
        let _ = writeln!(out, "- Elapsed: {:.2}s", self.elapsed_secs);
        let _ = writeln!(
            out,
            "- Outcome: {}",
            if self.error.is_some() {
                "error"
            } else if self.tool_actions.is_empty() {
                "no-op"
            } else {
                "modified skills"
            }
        );
        if let Some(err) = &self.error {
            let _ = writeln!(out, "- Error: `{}`", err);
        }

        // Tool-action histogram.
        let mut histogram: BTreeMap<&str, usize> = BTreeMap::new();
        for action in &self.tool_actions {
            *histogram.entry(action.as_str()).or_insert(0) += 1;
        }
        if !histogram.is_empty() {
            let _ = writeln!(out, "\n## Tool calls\n");
            for (name, count) in &histogram {
                let _ = writeln!(out, "- `{}` × {}", name, count);
            }
        }

        // Diff: extract skill names from each rendered list and
        // compute set deltas. The renderer prefixes each row with
        // "  - <name>  ".
        let before_names = parse_candidate_names(&self.before_candidates);
        let after_names = parse_candidate_names(&self.after_candidates);
        let removed: Vec<&String> = before_names.difference(&after_names).collect();
        let added: Vec<&String> = after_names.difference(&before_names).collect();
        if !removed.is_empty() || !added.is_empty() {
            let _ = writeln!(out, "\n## Skill set delta\n");
            if !removed.is_empty() {
                let _ = writeln!(out, "Archived ({}):", removed.len());
                let mut sorted = removed.clone();
                sorted.sort();
                for name in sorted {
                    let _ = writeln!(out, "- ~~`{}`~~", name);
                }
            }
            if !added.is_empty() {
                let _ = writeln!(out, "\nAdded ({}):", added.len());
                let mut sorted = added.clone();
                sorted.sort();
                for name in sorted {
                    let _ = writeln!(out, "- **`{}`**", name);
                }
            }
        }

        let _ = writeln!(out, "\n## Candidate list — before\n\n```");
        out.push_str(&self.before_candidates);
        if !self.before_candidates.ends_with('\n') {
            out.push('\n');
        }
        let _ = writeln!(out, "```\n");

        let _ = writeln!(out, "## Candidate list — after\n\n```");
        out.push_str(&self.after_candidates);
        if !self.after_candidates.ends_with('\n') {
            out.push('\n');
        }
        let _ = writeln!(out, "```");

        out
    }
}

/// Extract `<name>` tokens from a rendered candidate list row of
/// the form `"  - <name>  state=..."`. Used by the diff logic in
/// `CuratorReport::to_markdown`.
fn parse_candidate_names(rendered: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for line in rendered.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("- ")
            && let Some(name) = rest.split_whitespace().next()
        {
            out.insert(name.to_string());
        }
    }
    out
}

/// Persist a curator report to
/// `.dirge/skills/.curator_reports/{timestamp}/REPORT.md`.
/// Returns the run directory path on success. Best-effort — the
/// caller logs and continues if the write fails.
pub fn write_curator_report(
    paths: &ProjectPaths,
    report: &CuratorReport,
) -> Result<PathBuf, String> {
    let root = paths.skills_dir().join(".curator_reports");
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("Failed to create curator reports dir: {e}"))?;

    // Stamp directory: YYYYMMDD-HHMMSS. Append a disambiguator if
    // a rerun lands in the same second.
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let mut run_dir = root.join(&stamp);
    let mut suffix = 1;
    while run_dir.exists() {
        suffix += 1;
        run_dir = root.join(format!("{}-{}", stamp, suffix));
    }
    std::fs::create_dir_all(&run_dir)
        .map_err(|e| format!("Failed to create curator run dir: {e}"))?;

    let md = report.to_markdown();
    let report_path = run_dir.join("REPORT.md");
    std::fs::write(&report_path, md.as_bytes())
        .map_err(|e| format!("Failed to write REPORT.md: {e}"))?;
    Ok(run_dir)
}

// ── Helpers ───────────────────────────────────────────

fn now_secs() -> u64 {
    crate::time_util::now_unix_secs()
}

/// Fallback: compute file modification age in seconds.
fn file_mod_age(path: &std::path::Path, now: u64) -> u64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| now.saturating_sub(d.as_secs()))
        .unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_project() -> (ProjectPaths, std::path::PathBuf) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("dirge-curator-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let paths = ProjectPaths::new(&dir);
        (paths, dir)
    }

    fn create_skill_dir(paths: &ProjectPaths, name: &str) {
        let dir = paths.skills_dir().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: test\n---\n\nbody\n").unwrap();
    }

    // ── CuratorState persistence ───────────────────────

    #[test]
    fn curator_state_round_trips() {
        let (paths, _dir) = temp_project();
        let state_path = paths.skills_dir().join(".curator_state");

        let mut state = CuratorState::new();
        state.last_run = Some(1234567890);
        state.save(&state_path).unwrap();

        let loaded = CuratorState::load(&state_path).unwrap();
        assert_eq!(loaded.last_run, Some(1234567890));
        assert!(loaded.first_check > 0);
    }

    #[test]
    fn missing_state_file_defaults_to_new() {
        let (paths, _dir) = temp_project();
        let state_path = paths.skills_dir().join(".curator_state");
        let state = CuratorState::load(&state_path).unwrap();
        assert_eq!(state.last_run, None);
        assert!(state.first_check > 0);
    }

    // ── should_run_now ─────────────────────────────────

    #[test]
    fn first_run_seeds_and_defers() {
        let (paths, _dir) = temp_project();
        let state_path = paths.skills_dir().join(".curator_state");
        let mut curator = Curator::new(&paths).unwrap();
        assert!(!curator.should_run_now(), "first check should defer");
        // dirge-6js7 deadlock fix: the first check must persist the
        // seeded clock. Previously it returned false without saving,
        // so last_run stayed None forever and the curator never ran.
        assert!(
            state_path.exists(),
            "first check must persist seeded state (deadlock fix)",
        );
        let seeded = CuratorState::load(&state_path).unwrap();
        assert!(
            seeded.last_run.is_some(),
            "first check must seed last_run so the interval clock starts",
        );
    }

    #[test]
    fn runs_after_interval_elapses() {
        let (paths, _dir) = temp_project();
        let state_path = paths.skills_dir().join(".curator_state");

        // Set state as if last run was 8 days ago.
        let past = now_secs() - INTERVAL_HOURS * 3600 - 1;
        let mut state = CuratorState::new();
        state.last_run = Some(past);
        state.save(&state_path).unwrap();

        let mut curator = Curator::new(&paths).unwrap();
        assert!(curator.should_run_now());
    }

    #[test]
    fn does_not_run_within_interval() {
        let (paths, _dir) = temp_project();
        let state_path = paths.skills_dir().join(".curator_state");

        // Set state as if last run was 1 hour ago.
        let recent = now_secs() - 3600;
        let mut state = CuratorState::new();
        state.last_run = Some(recent);
        state.save(&state_path).unwrap();

        let mut curator = Curator::new(&paths).unwrap();
        assert!(!curator.should_run_now());
    }

    // ── archive_skill ─────────────────────────────────

    #[test]
    fn archive_moves_skill_to_archive_dir() {
        let (paths, _dir) = temp_project();
        create_skill_dir(&paths, "old-skill");

        let curator = Curator::new(&paths).unwrap();
        curator.archive_skill("old-skill").unwrap();

        // Original gone.
        assert!(!paths.skills_dir().join("old-skill").is_dir());
        // Present in archive.
        assert!(
            paths
                .skills_dir()
                .join(".archive")
                .join("old-skill")
                .join("SKILL.md")
                .is_file()
        );
    }

    // ── apply_automatic_transitions ────────────────────

    #[test]
    fn empty_skills_dir_is_no_op() {
        let (paths, _dir) = temp_project();
        std::fs::create_dir_all(paths.skills_dir()).unwrap();
        let mut curator = Curator::new(&paths).unwrap();
        let stale = curator.apply_automatic_transitions().unwrap();
        assert!(stale.is_empty());
    }

    #[test]
    fn missing_skills_dir_is_no_op() {
        let (paths, _dir) = temp_project();
        let mut curator = Curator::new(&paths).unwrap();
        let stale = curator.apply_automatic_transitions().unwrap();
        assert!(stale.is_empty());
    }

    #[test]
    fn record_run_updates_timestamp() {
        let (paths, _dir) = temp_project();
        let mut curator = Curator::new(&paths).unwrap();
        let before = curator.state.last_run;
        curator.record_run().unwrap();

        // Reload and verify.
        let curator2 = Curator::new(&paths).unwrap();
        assert!(
            curator2.state.last_run > before,
            "recording a run should update last_run"
        );
    }

    // ── dirge-odv3 — LLM consolidation prompt + candidate rendering ──

    /// The curator prompt must constrain the model to the real `skill`
    /// tool's `patch/create/delete/load/list` actions and never mention
    /// the hermes `skill_manage` / `skill_view` aliases.
    #[test]
    fn curator_prompt_names_real_skill_actions() {
        let p = CURATOR_PROMPT;
        for required in &[
            "action='patch'",
            "action='create'",
            "action='delete'",
            "action='load'",
            "action='list'",
        ] {
            assert!(p.contains(required), "prompt missing {}", required);
        }
        assert!(!p.contains("skill_manage"), "leaked hermes alias");
        assert!(!p.contains("skill_view"), "leaked hermes alias");
        // Anchors that drive the umbrella-building behavior.
        assert!(p.contains("UMBRELLA"), "missing umbrella framing");
        assert!(p.contains("agent-created"), "missing filter constraint");
        assert!(p.contains("pinned"), "missing pinned-skip rule");
    }

    #[test]
    fn render_candidate_list_empty_when_no_agent_skills() {
        let (paths, _dir) = temp_project();
        let store = crate::extras::skills::usage::UsageStore::load(&paths).unwrap();
        let text = render_candidate_list(&store);
        assert!(
            text.contains("No agent-created skills"),
            "expected no-op message: {text}"
        );
    }

    #[test]
    fn render_candidate_list_lists_agent_created_only() {
        let (paths, _dir) = temp_project();
        let mut store = crate::extras::skills::usage::UsageStore::load(&paths).unwrap();
        store.record_create("agent-a", "agent");
        store.record_create("agent-b", "agent");
        // Bundled skill: no `created_by="agent"` flag.
        store.record_view("bundled-x"); // creates entry with no created_by

        let text = render_candidate_list(&store);
        assert!(text.contains("agent-a"), "agent-a should appear: {text}");
        assert!(text.contains("agent-b"), "agent-b should appear");
        assert!(
            !text.contains("bundled-x"),
            "bundled-x must NOT appear (not agent-created): {text}"
        );
        // Telemetry columns must be present so the curator can judge
        // overlap from content + activity instead of usage counters.
        assert!(text.contains("use="), "missing use_count column");
        assert!(text.contains("patches="), "missing patch_count column");
        assert!(
            text.contains("last_activity="),
            "missing last_activity column"
        );
    }

    // ── dirge-3m4h: curator REPORT.md ─────────────────────

    fn sample_report() -> CuratorReport {
        CuratorReport {
            started_at_rfc3339: "2026-05-28T09:00:00Z".into(),
            elapsed_secs: 12.5,
            before_candidates: "Candidate skills (agent-created, sorted by last activity):\n  \
                                - alpha  state=active  pinned=no  use=1  view=2  patches=0  last_activity=never\n  \
                                - beta-narrow  state=stale  pinned=no  use=0  view=0  patches=0  last_activity=never\n"
                .into(),
            after_candidates: "Candidate skills (agent-created, sorted by last activity):\n  \
                               - alpha  state=active  pinned=no  use=1  view=2  patches=1  last_activity=never\n  \
                               - alpha-umbrella  state=active  pinned=no  use=0  view=0  patches=0  last_activity=never\n"
                .into(),
            tool_actions: vec![
                "skill".into(),
                "skill".into(),
                "skill".into(),
            ],
            error: None,
        }
    }

    #[test]
    fn curator_report_markdown_includes_all_sections() {
        let md = sample_report().to_markdown();
        // Metadata header
        assert!(md.contains("# Curator run report"), "missing title");
        assert!(md.contains("2026-05-28T09:00:00Z"), "missing start time");
        assert!(md.contains("12.50s"), "missing elapsed seconds");
        assert!(md.contains("modified skills"), "missing outcome line");

        // Tool histogram
        assert!(md.contains("## Tool calls"), "missing tool section");
        assert!(md.contains("`skill` × 3"), "missing histogram entry");

        // Diff: beta-narrow was archived; alpha-umbrella added.
        assert!(md.contains("## Skill set delta"), "missing delta section");
        assert!(md.contains("Archived (1):"), "missing archived header");
        assert!(md.contains("~~`beta-narrow`~~"), "missing archived entry");
        assert!(md.contains("Added (1):"), "missing added header");
        assert!(md.contains("**`alpha-umbrella`**"), "missing added entry");

        // Before/after dumps
        assert!(
            md.contains("## Candidate list — before"),
            "missing before dump"
        );
        assert!(
            md.contains("## Candidate list — after"),
            "missing after dump"
        );
    }

    #[test]
    fn curator_report_renders_no_op_when_no_tool_calls() {
        let mut r = sample_report();
        r.tool_actions.clear();
        // No diff between before/after either.
        r.after_candidates = r.before_candidates.clone();
        let md = r.to_markdown();
        assert!(md.contains("no-op"), "outcome must show no-op");
        assert!(
            !md.contains("## Tool calls"),
            "no-op runs must omit the tool section"
        );
        assert!(
            !md.contains("## Skill set delta"),
            "no-op runs must omit the delta section"
        );
    }

    #[test]
    fn curator_report_renders_error_outcome() {
        let mut r = sample_report();
        r.error = Some("provider returned 503".into());
        let md = r.to_markdown();
        assert!(md.contains("Outcome: error"), "outcome must show error");
        assert!(md.contains("`provider returned 503`"), "error must appear");
    }

    #[test]
    fn write_curator_report_creates_timestamped_dir() {
        let (paths, dir) = temp_project();
        let report = sample_report();
        let run_dir = write_curator_report(&paths, &report).expect("write");

        assert!(run_dir.exists(), "run dir must exist");
        assert!(
            run_dir.starts_with(paths.skills_dir().join(".curator_reports")),
            "report must live under .dirge/skills/.curator_reports: {}",
            run_dir.display()
        );
        let report_md = run_dir.join("REPORT.md");
        assert!(report_md.exists(), "REPORT.md must be written");
        let body = std::fs::read_to_string(&report_md).expect("read");
        assert!(body.contains("# Curator run report"));
        assert!(body.contains("alpha-umbrella"));

        // Cleanup.
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_curator_report_handles_same_second_reruns() {
        let (paths, dir) = temp_project();
        let report = sample_report();
        let first = write_curator_report(&paths, &report).expect("first write");
        let second = write_curator_report(&paths, &report).expect("second write");
        assert_ne!(
            first, second,
            "back-to-back writes must land in distinct dirs"
        );
        assert!(first.exists() && second.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_candidate_list_flags_pinned() {
        let (paths, _dir) = temp_project();
        let mut store = crate::extras::skills::usage::UsageStore::load(&paths).unwrap();
        store.record_create("pinned-skill", "agent");
        store.set_pinned("pinned-skill", true).unwrap();

        let text = render_candidate_list(&store);
        assert!(
            text.contains("pinned=yes"),
            "pinned skills must be flagged so the LLM can skip them: {text}"
        );
    }
}
