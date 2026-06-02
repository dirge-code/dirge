//! Cross-session insight extraction (dirge-6js7).
//!
//! Background review (review.rs) extracts learnings from the CURRENT
//! session only. This pass fills the one structural gap that
//! single-session review cannot: aggregating SUB-THRESHOLD signals —
//! patterns each individual review discards as a one-off (a weak
//! convention, a recurring-but-minor gotcha) yet that recur across
//! several past sessions and so are genuinely durable.
//!
//! It is deliberately conservative (the skeptic's DEFER analysis was
//! noted; the user opted to build it with safeguards):
//! - 14-day interval gate — STRICTLY longer than the 7-day memory
//!   curator beside it, so it never outpaces consolidation.
//! - watermark + min-new-sessions: only runs when ≥3 prior sessions
//!   have accumulated since the last scan, else zero-cost skip.
//! - LEAN: surfaces candidate themes via the existing FTS index
//!   (`search_messages`) and bundles only matched SNIPPETS — never
//!   reads full transcripts, adds no new session_db method.
//! - ≥2-distinct-session recurrence bar enforced mechanically.
//! - coarse MEMORY.md pre-filter mechanically; the real dedup is the
//!   LLM prompt, which is fed the verbatim memory snapshot and told
//!   to add nothing already covered. (Store `add()` dedup is
//!   exact-match only — memory_store.rs:218 — so it won't catch
//!   paraphrases; the prompt must.)
//!
//! Plugs into the post-session orchestrator as the 4th and LAST
//! stage (after the curators), so the current session's facts are
//! already captured + consolidated before this runs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::extras::dirge_paths::ProjectPaths;

/// Interval between extraction runs. 14 days — strictly longer than
/// the memory curator's 7-day gate.
const INTERVAL_HOURS: u64 = 336;

/// Minimum prior sessions accumulated since the last scan before the
/// pass does any work. Below this, cross-session aggregation has no
/// material and the stage is a zero-cost skip.
const MIN_NEW_SESSIONS: usize = 3;

/// A theme must appear in at least this many DISTINCT prior sessions
/// to count as a recurring (durable) pattern rather than a one-off.
const MIN_RECURRENCE_SESSIONS: usize = 2;

/// Hard ceiling on the assembled snippet bundle handed to the LLM.
/// ~14 KB ≈ 3.5k tokens — bounded cost regardless of how much FTS
/// matches.
const BUNDLE_BUDGET_CHARS: usize = 14_000;

/// Per-snippet cap so one huge message can't dominate the bundle.
const SNIPPET_CHAR_CAP: usize = 400;

/// Candidate themes, each a list of plain seed TERMS. The FTS5
/// query is built mechanically by OR-joining the terms (see
/// `build_fts_or_query`), which also sanitizes them — so a term
/// can never inject FTS5 syntax. These only SURFACE candidates;
/// the ≥2-session bar + the LLM prompt decide what (if anything)
/// becomes a memory entry. Kept aligned with the categories
/// background review targets.
///
/// dirge-6js7 review: terms MUST be plain alphanumerics. An
/// earlier version inlined an FTS5 query string containing
/// `don't`; the bare apostrophe is an FTS5 syntax error, so that
/// whole theme silently never matched (the error was swallowed by
/// `unwrap_or_default`). Term-lists + sanitization prevent the
/// class of bug.
const SEED_THEMES: &[(&str, &[&str])] = &[
    (
        "build/test commands",
        &["build", "compile", "cargo", "pytest", "make", "lint"],
    ),
    (
        "naming / layout conventions",
        &["convention", "naming", "layout", "prefix", "suffix"],
    ),
    (
        "architecture patterns",
        &["architecture", "module", "layer", "boundary", "pattern"],
    ),
    (
        "library quirks",
        &["quirk", "gotcha", "workaround", "caveat", "pitfall"],
    ),
    (
        "user corrections / preferences",
        &["instead", "prefer", "stop", "avoid", "revert", "mistake"],
    ),
    (
        "attempted-and-failed",
        &["failed", "broken", "regression", "wrong"],
    ),
];

/// dirge-6js7: prompt for the cross-session extraction LLM pass.
/// Lives here (next to the mechanical pass) the way
/// `MEMORY_CURATOR_PROMPT` lives in `memory_curator.rs`. The LLM
/// half (`run_cross_session_extraction`) is in `review.rs`.
pub const CROSS_SESSION_PROMPT: &str = "You are running as dirge's CROSS-SESSION insight extractor. \
Single-session learnings are background review's job — you exist ONLY to promote SUB-THRESHOLD \
patterns that recur across MULTIPLE past sessions and that no single review could see. \
You have ONLY the `memory` tool available. \
\n\n\
Below are the project's CURRENT MEMORY.md and PITFALLS.md (ALREADY KNOWN — never restate or \
paraphrase these), followed by candidate themes the mechanical scan found recurring across past \
sessions. Each theme line states how many DISTINCT sessions it spans, with matched snippets. \
\n\n\
Add a MEMORY (or PITFALLS) entry via `memory(action='add', ...)` ONLY when ALL of these hold:\n\
  1. The theme is evidenced in >= 2 DISTINCT prior sessions (the count is stated — enforce it).\n\
  2. NO existing entry already covers it — check the snapshot above for SEMANTIC overlap, not just \
exact wording. The store only rejects verbatim duplicates, so a paraphrase of an existing fact is \
YOUR job to catch and skip.\n\
  3. It is a durable, project-level fact or pitfall — NOT a single-session detail, a transient \
error, an environment-specific failure (missing binary, unset credential), or a negative claim \
about a tool. Those are explicitly out of scope.\n\
\n\
Cap yourself at 1-2 adds. Most runs should add NOTHING — 'Nothing to add' is the expected, correct \
outcome when the recurring themes are already captured or are too weak to be durable. Quality over \
volume: a noisy MEMORY.md is worse than a sparse one (the budget is ~2200 chars).";

// ── State ─────────────────────────────────────────────

/// Scheduler + watermark state at `.dirge/memory/.cross_session_state`.
/// Mirrors `MemoryCuratorState` plus a watermark of the newest
/// session `last_active` already scanned, so re-runs don't reprocess
/// the same history.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CrossSessionState {
    pub last_run: Option<u64>,
    pub first_check: u64,
    /// RFC3339 `last_active` of the newest session scanned so far.
    /// Empty = nothing scanned yet (everything is "new").
    #[serde(default)]
    pub last_scanned_watermark: String,
}

impl CrossSessionState {
    fn new(now: u64) -> Self {
        Self {
            last_run: None,
            first_check: now,
            last_scanned_watermark: String::new(),
        }
    }

    fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::new(now_secs()));
        }
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("read cross-session state: {e}"))?;
        serde_json::from_str(&content).map_err(|e| format!("parse cross-session state: {e}"))
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create state dir: {e}"))?;
        }
        let content =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize state: {e}"))?;
        crate::fs_atomic::atomic_write_sync(path, content.as_bytes())
            .map_err(|e| format!("write state: {e}"))
    }
}

// ── Extractor ─────────────────────────────────────────

pub struct CrossSessionExtractor {
    paths: ProjectPaths,
    state: CrossSessionState,
    state_path: PathBuf,
}

/// A recurring theme that cleared the ≥2-distinct-session bar and is
/// not obviously covered by MEMORY.md. Carried into the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurringTheme {
    pub label: String,
    pub session_count: usize,
}

/// Output of the mechanical scan when there is material worth an LLM
/// pass: the assembled snippet bundle (the LLM input) plus the
/// themes (for the audit report).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossSessionBundle {
    pub llm_input: String,
    pub themes: Vec<RecurringTheme>,
}

impl CrossSessionExtractor {
    pub fn new(paths: &ProjectPaths) -> Result<Self, String> {
        let state_path = paths.memory_dir().join(".cross_session_state");
        let state = CrossSessionState::load(&state_path)?;
        Ok(Self {
            paths: paths.clone(),
            state,
            state_path,
        })
    }

    /// Interval gate (cheap, no DB). On the first-ever check, SEED
    /// the clock (persist `last_run = now`) and return false, so the
    /// pass first runs one interval after install; afterwards, run
    /// when the last run was >= 14 days ago.
    ///
    /// dirge-6js7: seeding here is what breaks the deadlock the
    /// sibling curators also had — returning false on first check
    /// without persisting left `last_run` `None` forever (the only
    /// seeder, `run_mechanical_scan`, is gated behind this check).
    /// Takes `&mut self` because seeding is a persisted side effect.
    pub fn should_run_now(&mut self) -> bool {
        match self.state.last_run {
            None => {
                self.state.last_run = Some(now_secs());
                if let Err(e) = self.state.save(&self.state_path) {
                    tracing::debug!(
                        target: "dirge::cross_session",
                        error = %e,
                        "failed to seed cross-session state on first check",
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

    /// Mechanical scan. Caller must have checked `should_run_now()`.
    /// Opens the session DB, finds prior sessions newer than the
    /// watermark, and — if ≥ `MIN_NEW_SESSIONS` accumulated —
    /// surfaces recurring themes via FTS, keeps those spanning
    /// ≥2 distinct sessions and not already covered by MEMORY.md,
    /// assembles a bounded snippet bundle, writes a REPORT.md, and
    /// advances state.
    ///
    /// Returns `Ok(None)` when there's nothing worth an LLM pass
    /// (not enough new sessions, no recurring themes, or all covered).
    /// Best-effort: a DB-open failure returns `Ok(None)`, never errors
    /// the orchestrator chain.
    pub fn run_mechanical_scan(
        &mut self,
        current_session_id: &str,
    ) -> Result<Option<CrossSessionBundle>, String> {
        let started = chrono::Utc::now();
        let started_iso = started.to_rfc3339();
        let started_filename = started.format("%Y%m%d-%H%M%S").to_string();

        // Always advance last_run once we attempt a scan, so the
        // 14-day interval is honored between scans regardless of
        // outcome.
        self.state.last_run = Some(now_secs());

        let db = match crate::extras::session_db::SessionDb::open(&self.paths.session_db_path()) {
            Ok(db) => db,
            Err(e) => {
                tracing::debug!(
                    target: "dirge::cross_session",
                    error = %e,
                    "Cannot open session DB — skipping cross-session scan",
                );
                self.state.save(&self.state_path)?;
                return Ok(None);
            }
        };

        // Heuristic bounds (dirge-6js7 review, accepted):
        // - `list_sessions_rich` returns the 50 most-recent sessions.
        //   On a project with >50 sessions between scans the watermark
        //   advances past the oldest unscanned ones — but those are the
        //   least relevant (oldest) and the 14-day cadence makes >50
        //   new sessions per scan unusual.
        // - per-theme recurrence is counted from FTS top-50 hits
        //   (`search_messages` LIMIT 50). This can undercount a very
        //   high-frequency term, but the bar is only ≥2 distinct
        //   sessions, so a genuinely recurring theme clears it easily.
        // Both are fine for a candidate-SURFACER; the LLM + the
        // ≥2-session bar are the real filters.
        let sessions = db.list_sessions_rich(None).unwrap_or_default();
        // New = newer than the watermark, excluding the current
        // (just-ended) session, which background review already
        // handled.
        let new_sessions: Vec<&crate::extras::session_db::SessionSummary> = sessions
            .iter()
            .filter(|s| s.id != current_session_id)
            .filter(|s| s.last_active.as_str() > self.state.last_scanned_watermark.as_str())
            .collect();

        if new_sessions.len() < MIN_NEW_SESSIONS {
            tracing::debug!(
                target: "dirge::cross_session",
                new = %new_sessions.len(),
                min = %MIN_NEW_SESSIONS,
                "Not enough new sessions for cross-session extraction — skipping",
            );
            // Don't advance the watermark — wait for more material.
            self.state.save(&self.state_path)?;
            return Ok(None);
        }

        let recent_ids: std::collections::HashSet<&str> =
            new_sessions.iter().map(|s| s.id.as_str()).collect();
        let newest_watermark = new_sessions
            .iter()
            .map(|s| s.last_active.clone())
            .max()
            .unwrap_or_default();

        // Existing memory (lowercased) for the coarse pre-filter.
        let existing_memory_lc = self.existing_memory_lowercased();

        // Surface candidate themes via FTS, keep recurring + uncovered.
        let mut themes: Vec<RecurringTheme> = Vec::new();
        let mut bundle = String::new();
        for (label, terms) in SEED_THEMES {
            let query = build_fts_or_query(terms);
            if query.is_empty() {
                continue;
            }
            let hits = db.search_messages(&query, None).unwrap_or_default();
            // Restrict to the recent/new session window.
            let recent_hits: Vec<&crate::extras::session_db::SearchResult> = hits
                .iter()
                .filter(|h| recent_ids.contains(h.session_id.as_str()))
                .collect();
            let distinct_sessions: std::collections::HashSet<&str> =
                recent_hits.iter().map(|h| h.session_id.as_str()).collect();
            if distinct_sessions.len() < MIN_RECURRENCE_SESSIONS {
                continue;
            }
            // Coarse pre-filter: skip a theme whose seed keywords are
            // ALL already present in existing memory. The LLM does the
            // real semantic dedup; this just avoids bundling the
            // obviously-covered.
            if theme_already_covered(terms, &existing_memory_lc) {
                continue;
            }
            themes.push(RecurringTheme {
                label: label.to_string(),
                session_count: distinct_sessions.len(),
            });
            append_theme_to_bundle(&mut bundle, label, distinct_sessions.len(), &recent_hits);
            if bundle.len() >= BUNDLE_BUDGET_CHARS {
                // Truncate at the largest char boundary <= budget —
                // `String::truncate` panics on a non-boundary byte
                // index, and snippets can contain multibyte UTF-8
                // (dirge-6js7 review HIGH).
                let mut cut = BUNDLE_BUDGET_CHARS.min(bundle.len());
                while cut > 0 && !bundle.is_char_boundary(cut) {
                    cut -= 1;
                }
                bundle.truncate(cut);
                bundle.push_str("\n[…bundle truncated at budget…]\n");
                break;
            }
        }

        // Scan done — advance the watermark so we don't reprocess.
        self.state.last_scanned_watermark = newest_watermark;
        self.state.save(&self.state_path)?;

        // Write the mechanical audit report.
        self.write_report(&started_iso, &started_filename, new_sessions.len(), &themes)?;

        if themes.is_empty() {
            return Ok(None);
        }
        Ok(Some(CrossSessionBundle {
            llm_input: bundle,
            themes,
        }))
    }

    fn existing_memory_lowercased(&self) -> String {
        let mem = std::fs::read_to_string(self.paths.memory_file("MEMORY.md")).unwrap_or_default();
        let pit =
            std::fs::read_to_string(self.paths.memory_file("PITFALLS.md")).unwrap_or_default();
        format!("{mem}\n{pit}").to_lowercase()
    }

    fn write_report(
        &self,
        started_iso: &str,
        started_filename: &str,
        new_session_count: usize,
        themes: &[RecurringTheme],
    ) -> Result<(), String> {
        use std::fmt::Write as _;
        let mut md = String::new();
        let _ = writeln!(md, "# Cross-session extraction — mechanical scan\n");
        let _ = writeln!(md, "- Started: {started_iso}");
        let _ = writeln!(md, "- New sessions since last scan: {new_session_count}");
        let _ = writeln!(
            md,
            "- Recurring themes (≥{MIN_RECURRENCE_SESSIONS} sessions, not covered): {}",
            themes.len()
        );
        if !themes.is_empty() {
            let _ = writeln!(md, "\n| Theme | Distinct sessions |");
            let _ = writeln!(md, "|---|---|");
            for t in themes {
                let _ = writeln!(md, "| {} | {} |", t.label, t.session_count);
            }
        }
        let _ = writeln!(
            md,
            "\n_Mechanical scan only — themes above were bundled for the LLM pass (LLM_REPORT.md) \
             if any; the LLM decides what becomes a memory entry._"
        );

        let dir = self
            .paths
            .memory_dir()
            .join(".cross_session_reports")
            .join(started_filename);
        std::fs::create_dir_all(&dir).map_err(|e| format!("create report dir: {e}"))?;
        std::fs::write(dir.join("REPORT.md"), md).map_err(|e| format!("write report: {e}"))
    }
}

/// Build a sanitized FTS5 OR-query from plain seed terms. Each term
/// is kept only if it is purely alphanumeric (no FTS5 special chars
/// — apostrophes, quotes, parens, etc.), so the query can never be a
/// syntax error. Returns "" when no term survives (caller skips the
/// theme). dirge-6js7 review HIGH fix.
fn build_fts_or_query(terms: &[&str]) -> String {
    let safe: Vec<&str> = terms
        .iter()
        .copied()
        .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric()))
        .collect();
    safe.join(" OR ")
}

/// True when every seed term already appears in existing memory.
/// Coarse — the LLM does the semantic dedup; this just avoids
/// bundling the obviously-covered.
fn theme_already_covered(terms: &[&str], existing_memory_lc: &str) -> bool {
    let keywords: Vec<String> = terms
        .iter()
        .map(|t| t.to_lowercase())
        .filter(|k| !k.is_empty())
        .collect();
    if keywords.is_empty() {
        return false;
    }
    keywords.iter().all(|k| existing_memory_lc.contains(k))
}

/// Append one theme's header + capped snippets to the bundle.
fn append_theme_to_bundle(
    bundle: &mut String,
    label: &str,
    session_count: usize,
    hits: &[&crate::extras::session_db::SearchResult],
) {
    use std::fmt::Write as _;
    let _ = writeln!(
        bundle,
        "\n## Theme: {label} (recurs across {session_count} sessions)\n"
    );
    // A few representative snippets, one per distinct session where
    // possible, each capped.
    let mut seen_sessions: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for h in hits {
        if !seen_sessions.insert(h.session_id.as_str()) {
            continue; // one snippet per session keeps the bundle lean
        }
        let snippet = cap_snippet(&h.content);
        let _ = writeln!(bundle, "- {snippet}");
        if seen_sessions.len() >= 5 {
            break;
        }
    }
}

fn cap_snippet(s: &str) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= SNIPPET_CHAR_CAP {
        one_line
    } else {
        let cut: String = one_line.chars().take(SNIPPET_CHAR_CAP).collect();
        format!("{cut}…")
    }
}

fn now_secs() -> u64 {
    crate::time_util::now_unix_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::session_db::SessionDb;

    fn temp_project() -> (ProjectPaths, std::path::PathBuf) {
        // Per-call counter so parallel tests never collide on a dir
        // name (pid+nanos alone can clash under high parallelism,
        // and a shared SQLite file fails migration on re-open).
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dirge-cross-session-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            n,
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = ProjectPaths::new(&dir);
        std::fs::create_dir_all(paths.memory_dir()).unwrap();
        std::fs::create_dir_all(paths.sessions_dir()).unwrap();
        (paths, dir)
    }

    fn seed_db(paths: &ProjectPaths) -> SessionDb {
        SessionDb::open(&paths.session_db_path()).unwrap()
    }

    fn add_session(db: &SessionDb, id: &str, last_active: &str, user_msg: &str) {
        db.insert_session(id, "cli", "gpt-5", "openai", last_active)
            .unwrap();
        db.insert_message(id, "user", user_msg, None, None, None, last_active)
            .unwrap();
    }

    #[test]
    fn should_run_now_seeds_and_defers_on_first_check() {
        let (paths, _t) = temp_project();
        let state_path = paths.memory_dir().join(".cross_session_state");
        let mut ext = CrossSessionExtractor::new(&paths).unwrap();
        assert!(!ext.should_run_now(), "first check defers (returns false)");
        // dirge-6js7 deadlock fix: the first check must SEED the
        // clock (persist last_run) — otherwise last_run stays None
        // forever and the pass never runs.
        assert!(state_path.exists(), "first check must persist seeded state");
        let seeded = CrossSessionState::load(&state_path).unwrap();
        assert!(
            seeded.last_run.is_some(),
            "first check must seed last_run so the interval clock starts",
        );
    }

    #[test]
    fn should_run_now_respects_14_day_interval() {
        let (paths, _t) = temp_project();
        let state_path = paths.memory_dir().join(".cross_session_state");
        // Ran 10 days ago — still inside the 14-day gate.
        let ten_days = now_secs().saturating_sub(10 * 24 * 3600);
        CrossSessionState {
            last_run: Some(ten_days),
            first_check: ten_days,
            last_scanned_watermark: String::new(),
        }
        .save(&state_path)
        .unwrap();
        let mut ext = CrossSessionExtractor::new(&paths).unwrap();
        assert!(!ext.should_run_now(), "10 days < 14-day gate");
    }

    #[test]
    fn should_run_now_true_after_15_days() {
        let (paths, _t) = temp_project();
        let state_path = paths.memory_dir().join(".cross_session_state");
        let fifteen_days = now_secs().saturating_sub(15 * 24 * 3600);
        CrossSessionState {
            last_run: Some(fifteen_days),
            first_check: fifteen_days,
            last_scanned_watermark: String::new(),
        }
        .save(&state_path)
        .unwrap();
        let mut ext = CrossSessionExtractor::new(&paths).unwrap();
        assert!(ext.should_run_now(), "15 days > 14-day gate");
    }

    #[test]
    fn scan_skips_when_fewer_than_min_new_sessions() {
        let (paths, _t) = temp_project();
        let db = seed_db(&paths);
        // Only 2 sessions — below MIN_NEW_SESSIONS (3).
        add_session(
            &db,
            "s1",
            "2026-05-01T10:00:00Z",
            "cargo build failed again",
        );
        add_session(
            &db,
            "s2",
            "2026-05-02T10:00:00Z",
            "cargo build still broken",
        );
        drop(db);
        let mut ext = CrossSessionExtractor::new(&paths).unwrap();
        let out = ext.run_mechanical_scan("current").unwrap();
        assert!(out.is_none(), "below MIN_NEW_SESSIONS → no bundle");
    }

    #[test]
    fn scan_surfaces_theme_recurring_across_multiple_sessions() {
        let (paths, _t) = temp_project();
        let db = seed_db(&paths);
        // 3 sessions all mentioning a build-command theme.
        add_session(
            &db,
            "s1",
            "2026-05-01T10:00:00Z",
            "the cargo build command is the real one",
        );
        add_session(
            &db,
            "s2",
            "2026-05-02T10:00:00Z",
            "had to compile with cargo again",
        );
        add_session(
            &db,
            "s3",
            "2026-05-03T10:00:00Z",
            "cargo build then pytest for tests",
        );
        drop(db);
        let mut ext = CrossSessionExtractor::new(&paths).unwrap();
        let out = ext.run_mechanical_scan("current").unwrap();
        let bundle = out.expect("recurring build theme across 3 sessions → bundle");
        assert!(
            bundle.themes.iter().any(|t| t.label.contains("build")),
            "build/test theme must be surfaced: {:?}",
            bundle.themes
        );
        assert!(
            bundle
                .themes
                .iter()
                .all(|t| t.session_count >= MIN_RECURRENCE_SESSIONS),
            "all themes must clear the ≥2-session bar",
        );
        assert!(!bundle.llm_input.is_empty(), "bundle has snippet text");
    }

    #[test]
    fn scan_excludes_current_session() {
        let (paths, _t) = temp_project();
        let db = seed_db(&paths);
        // The recurring theme lives ONLY in the current session +
        // one other → only 1 distinct PRIOR session → below bar.
        add_session(
            &db,
            "current",
            "2026-05-03T10:00:00Z",
            "cargo build cargo build cargo build",
        );
        add_session(&db, "s1", "2026-05-01T10:00:00Z", "cargo build once");
        add_session(
            &db,
            "s2",
            "2026-05-02T10:00:00Z",
            "unrelated chatter about weather",
        );
        add_session(&db, "s3", "2026-05-02T11:00:00Z", "more unrelated chatter");
        drop(db);
        let mut ext = CrossSessionExtractor::new(&paths).unwrap();
        let out = ext.run_mechanical_scan("current").unwrap();
        // The build theme appears in `current` (excluded) + s1 only =
        // 1 prior session, below the ≥2 bar → not surfaced.
        if let Some(b) = out {
            assert!(
                !b.themes.iter().any(|t| t.label.contains("build")),
                "current session must be excluded from the recurrence count",
            );
        }
    }

    #[test]
    fn scan_advances_watermark_so_reruns_dont_reprocess() {
        let (paths, _t) = temp_project();
        let db = seed_db(&paths);
        add_session(&db, "s1", "2026-05-01T10:00:00Z", "cargo build a");
        add_session(&db, "s2", "2026-05-02T10:00:00Z", "cargo build b");
        add_session(&db, "s3", "2026-05-03T10:00:00Z", "cargo build c");
        drop(db);
        let mut ext = CrossSessionExtractor::new(&paths).unwrap();
        let _ = ext.run_mechanical_scan("current").unwrap();
        // Watermark advanced to the newest session's last_active.
        assert_eq!(ext.state.last_scanned_watermark, "2026-05-03T10:00:00Z");
        // A second scan now sees 0 new sessions → no bundle.
        let out2 = ext.run_mechanical_scan("current").unwrap();
        assert!(out2.is_none(), "no new sessions after watermark → skip");
    }

    #[test]
    fn scan_writes_report_to_disk() {
        let (paths, _t) = temp_project();
        let db = seed_db(&paths);
        add_session(&db, "s1", "2026-05-01T10:00:00Z", "cargo build a");
        add_session(&db, "s2", "2026-05-02T10:00:00Z", "cargo build b");
        add_session(&db, "s3", "2026-05-03T10:00:00Z", "cargo build c");
        drop(db);
        let mut ext = CrossSessionExtractor::new(&paths).unwrap();
        let _ = ext.run_mechanical_scan("current").unwrap();
        let reports = paths.memory_dir().join(".cross_session_reports");
        assert!(reports.is_dir(), "report dir created");
        let runs: Vec<_> = std::fs::read_dir(&reports)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(runs.len(), 1);
        let body = std::fs::read_to_string(runs[0].path().join("REPORT.md")).unwrap();
        assert!(body.contains("Cross-session extraction"));
    }

    #[test]
    fn theme_already_covered_filters_known_keywords() {
        // All seed terms present in memory → covered.
        let mem =
            "we always run cargo build and lint and compile and pytest and make".to_lowercase();
        assert!(theme_already_covered(
            &["build", "compile", "cargo", "pytest", "make", "lint"],
            &mem
        ));
        // A term missing → not covered.
        let mem2 = "we run cargo".to_lowercase();
        assert!(!theme_already_covered(&["cargo", "pytest", "make"], &mem2));
    }

    #[test]
    fn build_fts_or_query_sanitizes_and_joins() {
        // Plain terms → OR-joined.
        assert_eq!(build_fts_or_query(&["build", "cargo"]), "build OR cargo");
        // Non-alphanumeric terms (apostrophe, paren) are dropped so
        // the query can never be an FTS5 syntax error.
        assert_eq!(build_fts_or_query(&["don't", "prefer", "(stop)"]), "prefer",);
        // All-unsafe → empty (caller skips the theme).
        assert_eq!(build_fts_or_query(&["don't", "a-b"]), "");
    }

    #[test]
    fn cap_snippet_truncates_long_content() {
        let long = "word ".repeat(200);
        let capped = cap_snippet(&long);
        assert!(capped.chars().count() <= SNIPPET_CHAR_CAP + 1);
        assert!(capped.ends_with('…'));
        assert_eq!(cap_snippet("short text"), "short text");
    }

    #[test]
    fn state_round_trips_through_disk() {
        let (paths, _t) = temp_project();
        let path = paths.memory_dir().join(".cross_session_state");
        let s = CrossSessionState {
            last_run: Some(123),
            first_check: 100,
            last_scanned_watermark: "2026-05-03T10:00:00Z".to_string(),
        };
        s.save(&path).unwrap();
        let loaded = CrossSessionState::load(&path).unwrap();
        assert_eq!(loaded, s);
    }
}
