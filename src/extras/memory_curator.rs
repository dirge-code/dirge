//! Memory entry lifecycle curator. Periodic background pass that
//! tracks MEMORY.md / PITFALLS.md entries via the usage sidecar,
//! identifies stale candidates, runs an LLM consolidation pass
//! over them, and writes audit reports for both stages.
//!
//! dirge-mo0w (audit finding B). Closed across two PRs:
//! - PR-1: mechanical pass — telemetry + state + stale-candidate
//!   identification + `REPORT.md` writer.
//! - PR-2: LLM consolidation pass — `MEMORY_CURATOR_PROMPT`
//!   + memory-only forked runner via
//!     `AnyAgent::spawn_memory_curator_runner` + `LLM_REPORT.md`
//!     writer.
//!
//! Parallel structure to `extras::skills::curator`:
//! - `.dirge/memory/.curator_state` — scheduler state
//! - `.dirge/memory/.curator_reports/{ts}/REPORT.md` — mechanical
//! - `.dirge/memory/.curator_reports/{ts}/LLM_REPORT.md` — LLM
//! - 7-day interval gate, first-run defer
//! - 30-day stale, 90-day archive-candidate thresholds
//!
//! Differences from the skills curator:
//! - Entries aren't named — they're keyed by FNV-1a hash of
//!   content via `memory_usage::MemoryUsageStore`.
//! - LLM pass biases toward KEEPING (skill curator biases toward
//!   restructuring into umbrella classes); a 90-day-old fact may
//!   still be load-bearing.
//! - LLM pass uses a memory-only allow-list — model literally
//!   cannot reach skill-write tools even if its prompt slips.

use crate::extras::dirge_paths::ProjectPaths;

/// dirge-mo0w PR-2: prompt for the memory curator's LLM
/// consolidation pass. Analog of `skills/curator::CURATOR_PROMPT`
/// (dirge-odv3) but adapted for memory entries — the model has
/// only the `memory` tool available (enforced at the registry
/// level via `spawn_memory_curator_runner`'s `&["memory"]`
/// allow-list, not just the prompt).
///
/// Differences from the skills prompt:
/// - Memory entries are facts/pitfalls, not procedural skills.
///   The right action is usually merge (consolidate overlapping
///   facts) or remove (obsolete), not "create umbrella class".
/// - Bias is toward KEEPING entries. A 90-day-old fact may
///   still be load-bearing; the model must show its work for
///   removals.
/// - No `pinned` concept yet — every entry is in scope.
pub const MEMORY_CURATOR_PROMPT: &str = "You are running as dirge's background memory CURATOR. Your job is to consolidate \
the project's MEMORY.md and PITFALLS.md so they stay accurate and compact, NOT to add new facts. \
You have ONLY the `memory` tool available — no read/write/edit/bash/skill tools are loaded. \
\n\n\
The mechanical pass below identified stale candidates: entries first observed ≥ 30 days ago. \
Stale ≠ obsolete; many old facts are still load-bearing. Read each candidate carefully against \
the rest of the memory store before acting. \
\n\n\
Preference order — prefer the earliest that fits:\n\
  1. KEEP. Most entries should be kept untouched. \"Old\" is not a reason to act.\n\
  2. CONSOLIDATE. If two or more entries cover the same fact, merge them into one \
clearer entry using `memory(action='replace', ...)` then `memory(action='remove', ...)` for \
the redundant copies.\n\
  3. RESTRUCTURE. If one entry mixed unrelated concerns, split it via \
`memory(action='replace', ...)` to the cleaner of the two facts, then `memory(action='add', ...)` \
the other. This is rare — only do it when the entry is genuinely two facts wearing one coat.\n\
  4. REMOVE. Only if the entry is clearly obsolete (refers to a deleted file, a renamed binary, \
a long-superseded approach the project no longer uses). Show your reasoning in your thinking before \
removing. Removal ARCHIVES the entry (it can be restored with `memory(action='restore', ...)`), \
so a justified removal is recoverable. The `old_text` argument also accepts the entry's exact \
`urn:ump:...` id from the stale-candidate table below when a substring would be ambiguous.\n\
\n\
PROMOTE durable working memory. `working`-kind entries are transient session scratch and are the \
first to be evicted from context. The promotion-candidates table below lists `working` entries that \
have outlived their session. For any whose content is a durable project fact or convention — a build/test \
command, a design decision, a recurring quirk — and whose Uses count shows it has actually been consulted, \
re-classify it with `memory(action='replace', old_text='<id-or-substring>', content='<same or lightly cleaned text>', \
kind='procedural')` (use `semantic` for a pure fact about the project, `procedural` for a how-to/rule). \
This keeps the knowledge in long-term memory instead of letting it decay. Leave genuinely transient notes \
(\"currently refactoring X\") as `working`; a low or zero Uses count is a reason NOT to promote.\n\
\n\
Do NOT:\n\
  • Add new facts. The curator is for consolidation, not capture. Background review handles capture.\n\
  • Reword for style. Only change wording when consolidating duplicates or fixing a fact that's \
now wrong.\n\
  • Remove pitfalls eagerly. A pitfall surviving 90 days probably caught someone.\n\
\n\
Target shape: the memory file at the end of your pass should have STRICTLY FEWER OR EQUAL entries \
to the start, each one carrying a fact that's still true. \"Nothing to consolidate.\" is a valid \
outcome and is often the right answer.\n\
\n\
Below is the current memory store and the stale candidates the mechanical pass flagged. \
Operate on these only.";

/// Days since `first_seen_at` before an entry counts as stale.
const STALE_AFTER_DAYS: u64 = 30;

/// dirge-26h1: minimum age before a `working`-kind entry is surfaced as
/// a promotion candidate. This only filters out same-session scratch
/// notes; it does NOT decide durability — the LLM pass weighs each
/// candidate's use count and content and decides whether to re-kind it
/// to `procedural`/`semantic`.
const PROMOTE_MIN_AGE_DAYS: u64 = 7;

/// dirge-jyks: hot-tier utilization (%) at or above which the LLM
/// pass is told consolidation of YOUNGER overlapping entries is also
/// in scope for that target.
const BUDGET_PRESSURE_PCT: u32 = 90;

/// Days of staleness before an entry becomes an archive candidate
/// for the LLM pass (PR-2). PR-1 just identifies them.
#[allow(dead_code)]
const ARCHIVE_AFTER_STALE_DAYS: u64 = 90;

/// dirge-bb4y: retention for RETIRED rows (tombstoned/superseded). After this
/// long the curator hard-deletes them to bound DB growth. Generous so the
/// restore affordance and recent audit chain are unaffected in practice.
const PURGE_RETIRED_AFTER_DAYS: u64 = 180;

/// Minimum hours between curator runs.
const INTERVAL_HOURS: u64 = 168; // 7 days

// ── Curator ───────────────────────────────────────────

/// Memory lifecycle manager. Constructed once per run.
pub struct MemoryCurator {
    paths: ProjectPaths,
    /// Shared scheduler clock (dirge-rwrg): session-count first-run
    /// gate + 7-day interval, state at `.dirge/memory/.curator_state`.
    clock: crate::extras::curator_clock::CuratorClock,
    /// dirge-4nix: recurrence-weighted graduation enabled (config
    /// `memory_graduation`). Default true.
    pub graduation_enabled: bool,
}

impl MemoryCurator {
    pub fn new(paths: &ProjectPaths) -> Result<Self, String> {
        let clock = crate::extras::curator_clock::CuratorClock::new(
            paths,
            paths.memory_dir().join(".curator_state"),
            INTERVAL_HOURS,
            crate::extras::curator_clock::DEFAULT_MIN_SESSIONS_FIRST_RUN,
        )?;
        Ok(Self {
            paths: paths.clone(),
            clock,
            graduation_enabled: true,
        })
    }

    /// Should the curator run now? See [`CuratorClock::should_run_now`]
    /// — session-count gate before the first run, interval after.
    pub fn should_run_now(&mut self) -> bool {
        self.clock.should_run_now()
    }

    /// Run the mechanical pass: scan the memories table, identify
    /// stale candidates by row age, write audit report. No LLM call,
    /// no archival. Returns the per-run report so callers (tests,
    /// follow-on LLM pass) can inspect what happened.
    ///
    /// dirge-18ks: entry age comes straight from `created_at` on the
    /// memories row — the `.usage.json` sidecar reconciliation this
    /// pass used to perform is obsolete (and `created_at` survives
    /// `replace`, which the content-hash-keyed sidecar did not).
    pub fn run_mechanical_pass(&mut self) -> Result<MechanicalReport, String> {
        let started_at = chrono::Utc::now();
        let started_at_iso = started_at.to_rfc3339();
        let started_at_filename = started_at.format("%Y%m%d-%H%M%S").to_string();

        // 1. Apply disuse decay BEFORE scanning, so this run's report
        //    reflects post-decay salience (dirge-jyks).
        let store = crate::extras::memory_db::SqliteMemoryStore::load(&self.paths)?;
        let decayed = store
            .apply_disuse_decay(STALE_AFTER_DAYS as i64)
            .unwrap_or_else(|e| {
                tracing::warn!(
                    target: "dirge::memory_curator",
                    error = %e,
                    "disuse decay failed — continuing pass",
                );
                0
            });

        // 1b. dirge-bb4y: GC ancient retired (tombstoned/superseded) rows to
        //     bound DB growth. Conservative retention — long enough that a
        //     purged tombstone is one nobody restored in months.
        let purged = store
            .purge_retired_rows(PURGE_RETIRED_AFTER_DAYS as i64)
            .unwrap_or_else(|e| {
                tracing::warn!(
                    target: "dirge::memory_curator",
                    error = %e,
                    "retired-row purge failed — continuing pass",
                );
                0
            });

        // 2. Scan active entries from the store.
        let entries = store.entries_for_curation()?;
        let total_entries = entries.len();

        // 3. Identify stale candidates: old AND not recently used.
        //    dirge-jyks: an entry the agent expanded within the stale
        //    window is demonstrably load-bearing — age alone no longer
        //    flags it.
        let recent_use_cutoff =
            (started_at - chrono::Duration::days(STALE_AFTER_DAYS as i64)).to_rfc3339();
        let mut stale_candidates: Vec<StaleCandidate> = Vec::new();
        let mut promotion_candidates: Vec<PromotionCandidate> = Vec::new();
        for entry in &entries {
            let Ok(first_seen) = chrono::DateTime::parse_from_rfc3339(&entry.created_at) else {
                continue;
            };
            let age_secs = started_at.timestamp() - first_seen.timestamp();
            let age_days = (age_secs.max(0) as u64) / 86400;
            let recently_used = entry
                .last_used_at
                .as_deref()
                .map(|t| t > recent_use_cutoff.as_str())
                .unwrap_or(false);
            // dirge-26h1: a `working` entry that outlived its session is
            // a promotion candidate (the LLM decides on its use count).
            // Such an entry is also, by usage, NOT stale, so the two
            // lists never overlap.
            if entry.kind == "working" && age_days >= PROMOTE_MIN_AGE_DAYS {
                promotion_candidates.push(PromotionCandidate {
                    target: entry.target.clone(),
                    entry_id: entry.uid.clone(),
                    preview: crate::text::first_line_preview(&entry.content),
                    age_days,
                    use_count: entry.use_count,
                });
            }
            if age_days >= STALE_AFTER_DAYS && !recently_used {
                stale_candidates.push(StaleCandidate {
                    target: entry.target.clone(),
                    entry_id: entry.uid.clone(),
                    preview: crate::text::first_line_preview(&entry.content),
                    age_days,
                    use_count: entry.use_count,
                });
            }
        }
        stale_candidates.sort_by_key(|c| std::cmp::Reverse(c.age_days));
        promotion_candidates.sort_by_key(|c| std::cmp::Reverse(c.use_count));

        // 4. Budget pressure: targets at/over the threshold are
        //    flagged so the LLM pass may consolidate YOUNGER
        //    overlapping entries there too (dirge-jyks).
        let pressure_targets: Vec<String> = ["memory", "pitfalls"]
            .iter()
            .filter(|t| store.hot_usage_pct(t) >= BUDGET_PRESSURE_PCT)
            .map(|t| t.to_string())
            .collect();

        // 4b. dirge-4nix: recurrence-weighted salience graduation.
        //     Detect near-duplicate clusters among active entries and
        //     boost the representative's salience. The ledger prevents
        //     re-boosting the same cluster every run. Fail-open: a
        //     per-cluster DB error logs a warning and continues.
        let mut graduated: Vec<GraduationRecord> = Vec::new();
        if self.graduation_enabled {
            let inputs: Vec<crate::extras::memory_graduation::GraduationInput> = entries
                .iter()
                .map(|e| crate::extras::memory_graduation::GraduationInput {
                    uid: e.uid.clone(),
                    target: e.target.clone(),
                    content: e.content.clone(),
                    use_count: e.use_count,
                    created_at: e.created_at.clone(),
                })
                .collect();
            let clusters = crate::extras::memory_graduation::recurrence_clusters(
                &inputs,
                crate::extras::memory_graduation::MIN_JACCARD,
                crate::extras::memory_graduation::MIN_TOKENS,
                crate::extras::memory_graduation::MIN_CLUSTER_SIZE,
            );
            let now_iso = &started_at_iso;
            for cluster in &clusters {
                if store.is_graduated(&cluster.hash).unwrap_or(false) {
                    continue;
                }
                let boost =
                    crate::extras::salience::RECURRENCE_SALIENCE_STEP * (cluster.size - 1) as f64;
                match store.bump_salience(
                    &cluster.rep_uid,
                    boost,
                    crate::extras::salience::SALIENCE_CAP,
                ) {
                    Ok(()) => {
                        if let Err(e) = store.record_graduation(&cluster.hash, now_iso) {
                            tracing::warn!(
                                target: "dirge::memory_curator",
                                cluster_hash = %cluster.hash,
                                error = %e,
                                "graduation ledger write failed — continuing pass",
                            );
                        }
                        graduated.push(GraduationRecord {
                            target: entries
                                .iter()
                                .find(|e| e.uid == cluster.rep_uid)
                                .map(|e| e.target.clone())
                                .unwrap_or_default(),
                            rep_uid: cluster.rep_uid.clone(),
                            size: cluster.size,
                            boost,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "dirge::memory_curator",
                            rep_uid = %cluster.rep_uid,
                            error = %e,
                            "salience bump failed — continuing pass",
                        );
                    }
                }
            }
        }

        // 5. Update the scheduler clock.
        self.clock.mark_ran()?;

        let report = MechanicalReport {
            started_at_iso: started_at_iso.clone(),
            total_entries,
            decayed,
            purged,
            pressure_targets,
            stale_candidates,
            promotion_candidates,
            graduated,
        };

        // 4. Write audit report.
        let reports_dir = self
            .paths
            .memory_dir()
            .join(".curator_reports")
            .join(&started_at_filename);
        std::fs::create_dir_all(&reports_dir).map_err(|e| format!("create reports dir: {e}"))?;
        let report_path = reports_dir.join("REPORT.md");
        std::fs::write(&report_path, report.to_markdown())
            .map_err(|e| format!("write report: {e}"))?;

        Ok(report)
    }
}

// ── Report ────────────────────────────────────────────

/// Per-run report. Curator returns this so callers (tests
/// today; LLM pass in PR-2) can introspect what the mechanical
/// pass observed. Also rendered as Markdown to disk for human
/// review.
#[derive(Debug, Clone, PartialEq)]
pub struct MechanicalReport {
    pub started_at_iso: String,
    pub total_entries: usize,
    /// Entries whose salience decayed for disuse this run (dirge-jyks).
    pub decayed: usize,
    /// dirge-bb4y: ancient retired (tombstoned/superseded) rows hard-deleted
    /// this run to bound DB growth.
    pub purged: usize,
    /// Targets at/over the hot-budget pressure threshold — the LLM
    /// pass may consolidate younger overlapping entries there.
    pub pressure_targets: Vec<String>,
    pub stale_candidates: Vec<StaleCandidate>,
    /// dirge-26h1: `working`-kind entries old enough to have outlived
    /// their session. The LLM pass decides which have proven durable
    /// and re-kinds those to `procedural`/`semantic`.
    pub promotion_candidates: Vec<PromotionCandidate>,
    /// dirge-4nix: recurrence clusters graduated this pass.
    pub graduated: Vec<GraduationRecord>,
}

/// A `working`-kind entry the curator surfaces for possible promotion.
/// Same shape as [`StaleCandidate`] but a distinct type so the two
/// lists can't be confused — stale entries are removal candidates,
/// these are re-kind candidates (dirge-26h1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionCandidate {
    pub target: String,
    pub entry_id: String,
    pub preview: String,
    pub age_days: u64,
    pub use_count: i64,
}

/// One entry the curator would propose for archive consideration.
/// PR-1 only identifies these; PR-2's LLM pass decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleCandidate {
    pub target: String,
    pub entry_id: String,
    pub preview: String,
    pub age_days: u64,
    /// Times the agent expanded this entry — 0 means nothing ever
    /// looked it up (dirge-jyks).
    pub use_count: i64,
}

/// dirge-4nix: one recurrence cluster that was graduated this pass.
#[derive(Debug, Clone, PartialEq)]
pub struct GraduationRecord {
    pub target: String,
    pub rep_uid: String,
    pub size: usize,
    pub boost: f64,
}

impl MechanicalReport {
    /// Render as Markdown for `REPORT.md`. Keep it scan-friendly
    /// — the audit report's job is "show me at a glance what
    /// changed this run."
    pub fn to_markdown(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "# Memory curator — mechanical pass\n");
        let _ = writeln!(out, "- Started: {}", self.started_at_iso);
        let _ = writeln!(out, "- Total entries: {}", self.total_entries);
        let _ = writeln!(out, "- Salience decayed (disuse): {}", self.decayed);
        let _ = writeln!(out, "- Retired rows purged: {}", self.purged);
        if !self.pressure_targets.is_empty() {
            let _ = writeln!(
                out,
                "- Budget pressure (≥ {BUDGET_PRESSURE_PCT}%): {}",
                self.pressure_targets.join(", "),
            );
        }
        let _ = writeln!(out, "- Stale candidates: {}", self.stale_candidates.len());
        let _ = writeln!(
            out,
            "- Working promotion candidates: {}",
            self.promotion_candidates.len(),
        );
        let _ = writeln!(
            out,
            "- Recurrence clusters graduated: {}",
            self.graduated.len(),
        );

        if !self.stale_candidates.is_empty() {
            let _ = writeln!(
                out,
                "\n## Stale candidates (≥ {STALE_AFTER_DAYS} days, no recent use)\n",
            );
            let _ = writeln!(out, "| Target | Age (days) | Uses | Entry ID | Preview |");
            let _ = writeln!(out, "|---|---|---|---|---|");
            for c in &self.stale_candidates {
                let _ = writeln!(
                    out,
                    "| `{}` | {} | {} | `{}` | {} |",
                    c.target,
                    c.age_days,
                    c.use_count,
                    c.entry_id,
                    c.preview.replace('|', "\\|"),
                );
            }
        }

        if !self.graduated.is_empty() {
            let _ = writeln!(out, "\n## Recurrence clusters graduated\n",);
            let _ = writeln!(out, "| Target | Size | Boost | Rep UID |");
            let _ = writeln!(out, "|---|---|---|---|");
            for g in &self.graduated {
                let _ = writeln!(
                    out,
                    "| `{}` | {} | +{:.3} | `{}` |",
                    g.target, g.size, g.boost, g.rep_uid,
                );
            }
        }

        let _ = writeln!(
            out,
            "\n_Mechanical pass only \u{2014} no entries archived. LLM consolidation pass (dirge-mo0w PR-2) decides actual fate._"
        );

        out
    }
}

/// dirge-mo0w PR-2: per-LLM-pass audit record. Parallel to
/// `skills::curator::CuratorReport`. The mechanical pass returns
/// `MechanicalReport`; the LLM pass returns this one. They're
/// written to disk separately so the operator can see which
/// stage produced which change.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmCuratorReport {
    pub started_at_iso: String,
    pub elapsed_secs: f64,
    /// Stale candidates the mechanical pass handed to the LLM.
    /// Same data as `MechanicalReport.stale_candidates` for the
    /// run; copied here so a single report file fully describes
    /// the LLM session.
    pub stale_candidates: Vec<StaleCandidate>,
    /// Sequence of memory-tool actions the LLM fired. Duplicates
    /// preserved.
    pub tool_actions: Vec<String>,
    /// Captured error message if the agent stream surfaced one.
    pub error: Option<String>,
}

impl LlmCuratorReport {
    pub fn to_markdown(&self) -> String {
        use std::collections::BTreeMap;
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "# Memory curator — LLM consolidation pass\n");
        let _ = writeln!(out, "- Started: {}", self.started_at_iso);
        let _ = writeln!(out, "- Elapsed: {:.2}s", self.elapsed_secs);
        let _ = writeln!(
            out,
            "- Outcome: {}",
            if self.error.is_some() {
                "error"
            } else if self.tool_actions.is_empty() {
                "no-op (LLM chose to keep all candidates)"
            } else {
                "modified memory entries"
            }
        );
        if let Some(err) = &self.error {
            let _ = writeln!(out, "- Error: `{err}`");
        }

        let mut histogram: BTreeMap<&str, usize> = BTreeMap::new();
        for action in &self.tool_actions {
            *histogram.entry(action.as_str()).or_insert(0) += 1;
        }
        if !histogram.is_empty() {
            let _ = writeln!(out, "\n## Tool calls\n");
            for (name, count) in &histogram {
                let _ = writeln!(out, "- `{name}` × {count}");
            }
        }

        if !self.stale_candidates.is_empty() {
            let _ = writeln!(out, "\n## Stale candidates given to the LLM\n");
            let _ = writeln!(out, "| Target | Age (days) | Uses | Entry ID | Preview |");
            let _ = writeln!(out, "|---|---|---|---|---|");
            for c in &self.stale_candidates {
                let _ = writeln!(
                    out,
                    "| `{}` | {} | {} | `{}` | {} |",
                    c.target,
                    c.age_days,
                    c.use_count,
                    c.entry_id,
                    c.preview.replace('|', "\\|"),
                );
            }
        }

        out
    }
}

/// Render the input the LLM curator sees: current MEMORY.md /
/// PITFALLS.md (full text) followed by the stale-candidate
/// table from the mechanical pass. This is concatenated AFTER
/// `MEMORY_CURATOR_PROMPT` and handed to the runner.
pub fn render_curator_input(
    report: &MechanicalReport,
    memory_md: &str,
    pitfalls_md: &str,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    // dirge-27py: each entry below is prefixed with its metadata so the
    // pass can weigh usage and target entries precisely, not just guess
    // from content.
    let _ = writeln!(
        out,
        "\nEach entry is prefixed `[kind | N uses | conf C | id]`: its memory kind, how many times \
         the agent has looked it up, its confidence (0–1 truth-likelihood; entries the user \
         contradicted are held lower), and its `urn:ump:…` id (pass that id as `old_text` to act \
         on exactly that entry). Uses is a signal of how load-bearing an entry is — but a low \
         count is NOT on its own a reason to remove; keep facts that are still true. A low \
         confidence is a reason to VERIFY a fact, not to delete it blindly."
    );
    let _ = writeln!(out, "\n## Current MEMORY.md\n");
    if memory_md.trim().is_empty() {
        let _ = writeln!(out, "_(empty)_");
    } else {
        let _ = writeln!(out, "{}", memory_md.trim_end());
    }
    let _ = writeln!(out, "\n## Current PITFALLS.md\n");
    if pitfalls_md.trim().is_empty() {
        let _ = writeln!(out, "_(empty)_");
    } else {
        let _ = writeln!(out, "{}", pitfalls_md.trim_end());
    }
    // dirge-jyks: budget pressure widens the LLM's consolidation
    // scope to younger overlapping entries for the named targets.
    if !report.pressure_targets.is_empty() {
        let _ = writeln!(
            out,
            "\n## Budget pressure\n\nThe following target(s) are at ≥ {BUDGET_PRESSURE_PCT}% of \
             their inline budget: {}. For these, consolidating YOUNGER overlapping or \
             contradictory entries is also in scope — the <30-day rule is relaxed under pressure.",
            report.pressure_targets.join(", "),
        );
    }
    let _ = writeln!(
        out,
        "\n## Stale candidates flagged by mechanical pass ({})\n",
        report.stale_candidates.len(),
    );
    if report.stale_candidates.is_empty() {
        let _ = writeln!(
            out,
            "_None. The mechanical pass found no entries ≥ {STALE_AFTER_DAYS} days old without recent use._"
        );
    } else {
        let _ = writeln!(
            out,
            "Uses = how many times the agent looked the entry up; 0 means nothing has needed it.\n"
        );
        let _ = writeln!(out, "| Target | Age (days) | Uses | Entry ID | Preview |");
        let _ = writeln!(out, "|---|---|---|---|---|");
        for c in &report.stale_candidates {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | `{}` | {} |",
                c.target,
                c.age_days,
                c.use_count,
                c.entry_id,
                c.preview.replace('|', "\\|"),
            );
        }
    }
    // dirge-26h1: working entries that have outlived their session —
    // promote the durable ones (see the PROMOTE rule above).
    if !report.promotion_candidates.is_empty() {
        let _ = writeln!(
            out,
            "\n## Working-memory promotion candidates ({})\n",
            report.promotion_candidates.len(),
        );
        let _ = writeln!(
            out,
            "These `working` entries are older than {PROMOTE_MIN_AGE_DAYS} days. Promote the ones \
             whose content is a durable fact/convention AND whose Uses count shows it's been \
             consulted; leave transient notes as working.\n"
        );
        let _ = writeln!(out, "| Target | Age (days) | Uses | Entry ID | Preview |");
        let _ = writeln!(out, "|---|---|---|---|---|");
        for c in &report.promotion_candidates {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | `{}` | {} |",
                c.target,
                c.age_days,
                c.use_count,
                c.entry_id,
                c.preview.replace('|', "\\|"),
            );
        }
    }
    out
}

// ── Helpers ───────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project() -> (ProjectPaths, std::path::PathBuf) {
        // Uniqueness must not rest on the clock: `process::id()` is constant
        // across a test binary and two parallel tests can read the same
        // nanosecond tick, collide on the dir, and have one test's
        // `remove_dir_all` wipe the other's DB mid-write. A per-process atomic
        // counter guarantees a distinct dir per call.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dirge-memory-curator-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            seq,
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = ProjectPaths::new(&dir);
        std::fs::create_dir_all(paths.memory_dir()).unwrap();
        (paths, dir)
    }

    /// Seed entries through the store API (the only write path now).
    fn seed_memory(paths: &ProjectPaths, target: &str, entries: &[&str]) {
        let store = crate::extras::memory_db::SqliteMemoryStore::load(paths).unwrap();
        for entry in entries {
            store.add_entry(target, entry, None).unwrap();
        }
    }

    /// Backdate an entry's created_at directly in the DB — the test
    /// stand-in for "this entry has been around for N days".
    fn backdate_entry(paths: &ProjectPaths, content: &str, days: i64) {
        let conn = crate::extras::memory_db::raw_conn(paths);
        let then = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
        let changed = conn
            .execute(
                "UPDATE memories SET created_at = ?1 WHERE content = ?2",
                rusqlite::params![then, content],
            )
            .unwrap();
        assert_eq!(changed, 1, "backdate must hit exactly one row");
    }

    /// Seed N sessions into the project DB so the first-run session
    /// gate (dirge-jyks) has material to count.
    fn seed_sessions(paths: &ProjectPaths, n: usize) {
        let db = crate::extras::session_db::SessionDb::open(&paths.session_db_path()).unwrap();
        for i in 0..n {
            db.insert_session(
                &format!("sess-{i}"),
                "cli",
                "gpt-5",
                "openai",
                "2026-05-01T10:00:00Z",
            )
            .unwrap();
        }
    }

    /// dirge-jyks: before the first run, the gate is session count.
    /// A young project with few sessions defers; once enough sessions
    /// accumulate, the first pass fires without waiting 7 days.
    #[test]
    fn first_run_gated_on_session_count_not_calendar() {
        let (paths, _tmp) = temp_project();
        std::fs::create_dir_all(paths.sessions_dir()).unwrap();
        seed_sessions(&paths, 2);
        let mut curator = MemoryCurator::new(&paths).unwrap();
        assert!(
            !curator.should_run_now(),
            "2 sessions — first run still deferred"
        );

        seed_sessions(&paths, 10); // brings the total to 12
        let mut curator = MemoryCurator::new(&paths).unwrap();
        assert!(
            curator.should_run_now(),
            "enough sessions — first run fires without a 7-day wait"
        );
        // The pass itself seeds last_run, closing the dirge-6js7
        // deadlock the old defer-and-seed dance worked around.
        curator.run_mechanical_pass().unwrap();
        assert!(curator.clock.last_run().is_some());
        assert!(!curator.should_run_now(), "interval gate applies after");
    }

    /// Write a legacy-shaped state file with a given last_run.
    fn write_state(paths: &ProjectPaths, last_run: u64) {
        std::fs::create_dir_all(paths.memory_dir()).unwrap();
        std::fs::write(
            paths.memory_dir().join(".curator_state"),
            format!(r#"{{"last_run": {last_run}, "first_check": {last_run}}}"#),
        )
        .unwrap();
    }

    /// After a run, the 7-day interval gate keeps subsequent
    /// checks from re-running immediately.
    #[test]
    fn should_run_now_respects_interval_gate() {
        let (paths, _tmp) = temp_project();
        write_state(&paths, crate::time_util::now_unix_secs());
        let mut curator = MemoryCurator::new(&paths).unwrap();
        assert!(
            !curator.should_run_now(),
            "must respect 7-day interval gate",
        );
    }

    /// After 8 days, the gate opens and the curator should run.
    #[test]
    fn should_run_now_returns_true_after_interval_elapsed() {
        let (paths, _tmp) = temp_project();
        write_state(
            &paths,
            crate::time_util::now_unix_secs().saturating_sub(8 * 24 * 3600),
        );
        let mut curator = MemoryCurator::new(&paths).unwrap();
        assert!(curator.should_run_now(), "after 8 days the gate must open");
    }

    /// Empty memory store: pass runs cleanly, report shows
    /// zero entries, state advances.
    #[test]
    fn run_mechanical_pass_handles_empty_memory_store() {
        let (paths, _tmp) = temp_project();
        let mut curator = MemoryCurator::new(&paths).unwrap();
        let report = curator.run_mechanical_pass().unwrap();
        assert_eq!(report.total_entries, 0);
        assert_eq!(report.stale_candidates.len(), 0);
        // State advanced.
        assert!(curator.clock.last_run().is_some());
    }

    /// Fresh entries appear in the report total but DON'T appear as
    /// stale candidates (they're new).
    #[test]
    fn run_mechanical_pass_records_fresh_entries_without_marking_stale() {
        let (paths, _tmp) = temp_project();
        seed_memory(&paths, "memory", &["fact 1", "fact 2"]);
        seed_memory(&paths, "pitfalls", &["pitfall 1"]);
        let mut curator = MemoryCurator::new(&paths).unwrap();
        let report = curator.run_mechanical_pass().unwrap();
        assert_eq!(report.total_entries, 3);
        assert_eq!(
            report.stale_candidates.len(),
            0,
            "freshly-observed entries can't be stale yet",
        );
    }

    /// Entries created > 30 days ago surface as stale candidates.
    /// Age now comes straight from the row's `created_at`.
    #[test]
    fn run_mechanical_pass_identifies_old_entries_as_stale() {
        let (paths, _tmp) = temp_project();
        seed_memory(&paths, "memory", &["old fact", "new fact"]);
        backdate_entry(&paths, "old fact", 31);

        let mut curator = MemoryCurator::new(&paths).unwrap();
        let report = curator.run_mechanical_pass().unwrap();

        let stale_targets: Vec<&str> = report
            .stale_candidates
            .iter()
            .map(|c| c.preview.as_str())
            .collect();
        assert!(
            stale_targets.contains(&"old fact"),
            "old entry must be marked stale: {stale_targets:?}",
        );
        assert!(
            !stale_targets.contains(&"new fact"),
            "fresh entry must NOT be stale: {stale_targets:?}",
        );
        // Stale candidates are identified by their stable uid now.
        assert!(
            report.stale_candidates[0].entry_id.starts_with("urn:ump:"),
            "candidate id must be the row uid: {:?}",
            report.stale_candidates[0].entry_id,
        );
    }

    /// dirge-jyks: an old entry the agent recently expanded is
    /// load-bearing — it must NOT surface as a stale candidate.
    #[test]
    fn recently_used_old_entries_are_not_stale() {
        let (paths, _tmp) = temp_project();
        seed_memory(&paths, "memory", &["consulted fact", "ignored fact"]);
        backdate_entry(&paths, "consulted fact", 60);
        backdate_entry(&paths, "ignored fact", 60);
        // Expanding records last_used_at = now.
        let store = crate::extras::memory_db::SqliteMemoryStore::load(&paths).unwrap();
        store.expand_entry("consulted fact").unwrap();

        let mut curator = MemoryCurator::new(&paths).unwrap();
        let report = curator.run_mechanical_pass().unwrap();
        let stale: Vec<&str> = report
            .stale_candidates
            .iter()
            .map(|c| c.preview.as_str())
            .collect();
        assert!(
            !stale.contains(&"consulted fact"),
            "recently-used entry must not be stale: {stale:?}"
        );
        assert!(
            stale.contains(&"ignored fact"),
            "unused old entry still flags: {stale:?}"
        );
        // The unused candidate carries its zero use_count for the LLM.
        assert_eq!(report.stale_candidates[0].use_count, 0);
    }

    /// dirge-jyks: the mechanical pass decays salience of old unused
    /// entries (floored), leaving young or used entries alone.
    #[test]
    fn mechanical_pass_applies_disuse_decay() {
        let (paths, _tmp) = temp_project();
        seed_memory(&paths, "memory", &["old unused", "young entry"]);
        backdate_entry(&paths, "old unused", 45);

        let mut curator = MemoryCurator::new(&paths).unwrap();
        let report = curator.run_mechanical_pass().unwrap();
        assert_eq!(report.decayed, 1, "exactly the old unused entry decays");

        let conn = crate::extras::memory_db::raw_conn(&paths);
        let (old_sal, young_sal): (f64, f64) = (
            conn.query_row(
                "SELECT salience FROM memories WHERE content = 'old unused'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            conn.query_row(
                "SELECT salience FROM memories WHERE content = 'young entry'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
        );
        assert!(old_sal < 0.5, "default 0.5 must have decayed: {old_sal}");
        assert!(
            (young_sal - 0.5).abs() < 1e-9,
            "young entry untouched: {young_sal}"
        );
    }

    /// dirge-jyks: a target at >= 90% hot-budget utilization is
    /// flagged as a pressure target.
    #[test]
    fn mechanical_pass_flags_budget_pressure() {
        let (paths, _tmp) = temp_project();
        // ~2060 of 2200 chars ≈ 93%.
        let big_one = format!("one {}", "a".repeat(1024));
        let big_two = format!("two {}", "b".repeat(1024));
        seed_memory(&paths, "memory", &[&big_one, &big_two]);
        let mut curator = MemoryCurator::new(&paths).unwrap();
        let report = curator.run_mechanical_pass().unwrap();
        assert_eq!(
            report.pressure_targets,
            vec!["memory".to_string()],
            "memory target under pressure, pitfalls not"
        );
    }

    /// REPORT.md is written under `.dirge/memory/.curator_reports/{ts}/`.
    #[test]
    fn run_mechanical_pass_writes_audit_report_to_disk() {
        let (paths, _tmp) = temp_project();
        seed_memory(&paths, "memory", &["one fact"]);
        let mut curator = MemoryCurator::new(&paths).unwrap();
        curator.run_mechanical_pass().unwrap();
        let reports_root = paths.memory_dir().join(".curator_reports");
        assert!(reports_root.is_dir(), "reports root must exist");
        let entries: Vec<_> = std::fs::read_dir(&reports_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one run directory per run");
        let report_md = entries[0].path().join("REPORT.md");
        assert!(report_md.is_file(), "REPORT.md must be written");
        let body = std::fs::read_to_string(&report_md).unwrap();
        assert!(body.contains("# Memory curator"));
        assert!(body.contains("Total entries: 1"));
    }

    /// A removed entry stops appearing in subsequent passes — no
    /// sidecar to reconcile, the row is simply gone.
    #[test]
    fn run_mechanical_pass_reflects_removed_entries() {
        let (paths, _tmp) = temp_project();
        seed_memory(&paths, "memory", &["doomed fact"]);
        let mut curator = MemoryCurator::new(&paths).unwrap();
        let report = curator.run_mechanical_pass().unwrap();
        assert_eq!(report.total_entries, 1);

        let store = crate::extras::memory_db::SqliteMemoryStore::load(&paths).unwrap();
        store.remove_entry("memory", "doomed fact").unwrap();

        let mut curator2 = MemoryCurator::new(&paths).unwrap();
        let report = curator2.run_mechanical_pass().unwrap();
        assert_eq!(report.total_entries, 0, "removed entry must disappear");
    }

    /// State persistence: a fresh curator instance loads the
    /// last_run timestamp the previous instance wrote.
    #[test]
    fn run_mechanical_pass_persists_last_run_timestamp() {
        let (paths, _tmp) = temp_project();
        seed_memory(&paths, "memory", &["whatever"]);
        let mut curator = MemoryCurator::new(&paths).unwrap();
        curator.run_mechanical_pass().unwrap();
        let last_run = curator.clock.last_run();
        let curator2 = MemoryCurator::new(&paths).unwrap();
        assert_eq!(
            curator2.clock.last_run(),
            last_run,
            "state must round-trip through disk",
        );
    }

    /// Report markdown contains a "no entries archived" disclaimer
    /// so the operator knows PR-1 is mechanical-only.
    #[test]
    fn report_markdown_disclaims_actual_archival() {
        let report = MechanicalReport {
            started_at_iso: "2026-05-28T12:00:00Z".to_string(),
            total_entries: 5,
            decayed: 0,
            purged: 0,
            pressure_targets: vec![],
            stale_candidates: vec![],
            promotion_candidates: vec![],
            graduated: vec![],
        };
        let md = report.to_markdown();
        assert!(
            md.contains("no entries archived"),
            "PR-1 must disclaim mechanical-only scope: {md}",
        );
    }

    // ── PR-2: render_curator_input + LlmCuratorReport ──

    fn make_report(stale: Vec<StaleCandidate>) -> MechanicalReport {
        MechanicalReport {
            started_at_iso: "2026-05-28T12:00:00Z".to_string(),
            total_entries: stale.len(),
            decayed: 0,
            purged: 0,
            pressure_targets: vec![],
            stale_candidates: stale,
            promotion_candidates: vec![],
            graduated: vec![],
        }
    }

    /// Render shows current memory + pitfalls verbatim, then a
    /// table of stale candidates. The LLM sees this and decides
    /// what to consolidate / remove.
    #[test]
    fn render_curator_input_includes_memory_pitfalls_and_stale_table() {
        let report = make_report(vec![StaleCandidate {
            target: "memory".to_string(),
            entry_id: "abc123".to_string(),
            preview: "old fact".to_string(),
            age_days: 45,
            use_count: 3,
        }]);
        let out = render_curator_input(&report, "fact A\n§\nfact B", "pitfall X");
        assert!(out.contains("## Current MEMORY.md"));
        assert!(out.contains("fact A"));
        assert!(out.contains("## Current PITFALLS.md"));
        assert!(out.contains("pitfall X"));
        assert!(out.contains("## Stale candidates"));
        assert!(out.contains("abc123"));
        assert!(out.contains("old fact"));
        assert!(out.contains("45"));
        assert!(out.contains("Uses"), "usage column rendered: {out}");
    }

    /// dirge-jyks: pressure targets render a scope-widening note in
    /// the LLM input.
    #[test]
    fn render_curator_input_includes_pressure_note() {
        let mut report = make_report(vec![]);
        report.pressure_targets = vec!["memory".to_string()];
        let out = render_curator_input(&report, "fact A", "");
        assert!(out.contains("## Budget pressure"), "{out}");
        assert!(out.contains("YOUNGER"), "{out}");
    }

    /// Empty memory store renders the `_(empty)_` sentinel
    /// instead of leaving the section blank — keeps the prompt
    /// readable when the project hasn't accumulated facts yet.
    #[test]
    fn render_curator_input_marks_empty_stores_explicitly() {
        let report = make_report(vec![]);
        let out = render_curator_input(&report, "", "");
        assert!(out.contains("## Current MEMORY.md"));
        assert!(out.contains("_(empty)_"));
        assert!(out.contains("## Current PITFALLS.md"));
        assert!(out.contains("None. The mechanical pass found no entries"));
    }

    // ── Working-memory promotion (dirge-26h1) ────────────────────

    #[test]
    fn curator_prompt_instructs_working_promotion() {
        let p = MEMORY_CURATOR_PROMPT.to_lowercase();
        assert!(p.contains("promote"), "prompt must mention promotion");
        assert!(p.contains("working"), "prompt must name the working kind");
        assert!(
            p.contains("procedural") || p.contains("semantic"),
            "prompt must name the promotion target kind"
        );
    }

    #[test]
    fn mechanical_pass_collects_durable_working_candidates() {
        let (paths, _tmp) = temp_project();
        let store = crate::extras::memory_db::SqliteMemoryStore::load(&paths).unwrap();
        store
            .add_entry(
                "memory",
                "build: cargo test --bin dirge",
                Some(crate::extras::memory_db::MemoryKind::Working),
            )
            .unwrap();
        store
            .add_entry(
                "memory",
                "fresh scratch note",
                Some(crate::extras::memory_db::MemoryKind::Working),
            )
            .unwrap();
        store
            .add_entry(
                "memory",
                "a durable project fact",
                Some(crate::extras::memory_db::MemoryKind::Semantic),
            )
            .unwrap();
        // Age only the first working entry past the promotion gate.
        backdate_entry(&paths, "build: cargo test --bin dirge", 10);
        drop(store);

        let mut curator = MemoryCurator::new(&paths).unwrap();
        let report = curator.run_mechanical_pass().unwrap();

        let previews: Vec<&str> = report
            .promotion_candidates
            .iter()
            .map(|c| c.preview.as_str())
            .collect();
        assert!(
            previews.iter().any(|p| p.contains("cargo test")),
            "aged working entry is a promotion candidate: {previews:?}"
        );
        assert!(
            !previews.iter().any(|p| p.contains("fresh scratch")),
            "a same-session working note is not yet a candidate: {previews:?}"
        );
        assert!(
            !previews.iter().any(|p| p.contains("durable project fact")),
            "non-working entries are never promotion candidates: {previews:?}"
        );
    }

    #[test]
    fn render_curator_input_lists_promotion_candidates() {
        let mut report = make_report(vec![]);
        report.promotion_candidates = vec![PromotionCandidate {
            target: "memory".to_string(),
            entry_id: "urn:ump:abc".to_string(),
            preview: "build: cargo test --bin dirge".to_string(),
            age_days: 12,
            use_count: 3,
        }];
        let out = render_curator_input(&report, "[working] build: cargo test --bin dirge", "");
        assert!(
            out.contains("promotion candidates"),
            "render must include the promotion section: {out}"
        );
        assert!(
            out.contains("build: cargo test --bin dirge"),
            "candidate preview must appear: {out}"
        );
    }

    #[test]
    fn render_curator_input_explains_entry_metadata() {
        // dirge-27py / dirge-fa10: the bulk dump entries carry
        // `[kind | N uses | conf C | id]` prefixes, so the input must
        // explain how to read them (confidence included).
        let out = render_curator_input(
            &make_report(vec![]),
            "[procedural | 2 uses | conf 0.60 | urn:ump:x]\nsome durable fact",
            "",
        );
        assert!(
            out.contains("[kind | N uses | conf C | id]"),
            "input must explain the per-entry metadata prefix: {out}"
        );
        assert!(
            out.contains("urn:ump:x") && out.contains("some durable fact"),
            "annotated entry passes through: {out}"
        );
    }

    /// LLM report markdown captures elapsed, tool actions
    /// histogram, and the stale candidate table the LLM was
    /// given.
    #[test]
    fn llm_curator_report_markdown_includes_actions_and_candidates() {
        let r = LlmCuratorReport {
            started_at_iso: "2026-05-28T12:00:00Z".to_string(),
            elapsed_secs: 4.2,
            stale_candidates: vec![StaleCandidate {
                target: "pitfalls".to_string(),
                entry_id: "deadbeef00000000".to_string(),
                preview: "stale pitfall".to_string(),
                age_days: 100,
                use_count: 0,
            }],
            tool_actions: vec!["memory".to_string(), "memory".to_string()],
            error: None,
        };
        let md = r.to_markdown();
        assert!(md.contains("# Memory curator — LLM consolidation pass"));
        assert!(md.contains("Outcome: modified memory entries"));
        assert!(md.contains("`memory` × 2"));
        assert!(md.contains("deadbeef00000000"));
        assert!(md.contains("stale pitfall"));
    }

    /// LLM report flags no-op runs distinctly so the operator
    /// can tell "LLM chose to keep everything" from "LLM crashed."
    #[test]
    fn llm_curator_report_markdown_flags_noop_outcome() {
        let r = LlmCuratorReport {
            started_at_iso: "2026-05-28T12:00:00Z".to_string(),
            elapsed_secs: 0.5,
            stale_candidates: vec![],
            tool_actions: vec![],
            error: None,
        };
        let md = r.to_markdown();
        assert!(md.contains("no-op (LLM chose to keep all candidates)"));
    }

    /// LLM report markdown surfaces error messages so failures
    /// are visible in the audit trail without scraping logs.
    #[test]
    fn llm_curator_report_markdown_surfaces_errors() {
        let r = LlmCuratorReport {
            started_at_iso: "2026-05-28T12:00:00Z".to_string(),
            elapsed_secs: 0.1,
            stale_candidates: vec![],
            tool_actions: vec![],
            error: Some("model timed out".to_string()),
        };
        let md = r.to_markdown();
        assert!(md.contains("Outcome: error"));
        assert!(md.contains("model timed out"));
    }

    /// Preview helper: short entries pass through verbatim;
    /// long entries get truncated with an ellipsis marker.
    #[test]
    fn preview_truncates_long_lines_with_ellipsis() {
        let short = crate::text::first_line_preview("short and sweet");
        assert_eq!(short, "short and sweet");
        let long = crate::text::first_line_preview(&"x".repeat(120));
        assert!(
            long.ends_with("..."),
            "long preview must end with '...': {long:?}",
        );
        assert!(long.len() <= 80, "preview must cap length: {}", long.len());
    }
    // ── dirge-4nix: recurrence graduation integration ──

    #[test]
    fn graduation_boosts_representative_and_ledger_dedups() {
        let (paths, _tmp) = temp_project();
        std::fs::create_dir_all(paths.sessions_dir()).unwrap();
        // Ensure the session-count gate is satisfied.
        seed_sessions(&paths, 12);

        let store = crate::extras::memory_db::SqliteMemoryStore::load(&paths).unwrap();
        // Seed 3 near-identical entries in the same target.
        let base = "dirge uses SQLite for long term persistent memory storage and retrieval";
        store.add_entry("memory", base, None).unwrap();
        store
            .add_entry(
                "memory",
                "dirge uses SQLite for long term persistent memory storage and retrieval system",
                None,
            )
            .unwrap();
        store
            .add_entry(
                "memory",
                "dirge uses SQLite for long term persistent memory storage and retrieval approach",
                None,
            )
            .unwrap();
        // Add one unrelated entry.
        store
            .add_entry("memory", "completely different fact about the sky", None)
            .unwrap();
        drop(store);

        // Run mechanical pass with graduation enabled.
        let mut curator = MemoryCurator::new(&paths).unwrap();
        curator.graduation_enabled = true;
        let report = curator.run_mechanical_pass().unwrap();

        // Expect one cluster of size 3.
        assert!(
            !report.graduated.is_empty(),
            "should graduate at least one cluster"
        );
        let g = &report.graduated[0];
        assert_eq!(g.size, 3);
        assert!((g.boost - 0.1).abs() < 1e-9, "boost = 0.05 * 2 = 0.1");

        // Check that the representative's salience was boosted.
        let conn = crate::extras::memory_db::raw_conn(&paths);
        let rep_salience: f64 = conn
            .query_row(
                "SELECT salience FROM memories WHERE uid = ?1",
                rusqlite::params![g.rep_uid],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            rep_salience > 0.5,
            "rep salience should be > default 0.5 after boost"
        );

        // Check the unrelated entry was untouched.
        let unrelated_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM graduation_ledger", [], |r| r.get(0))
            .unwrap();
        assert_eq!(unrelated_count, 1, "exactly one ledger row");

        // Second pass: should NOT boost again (ledger dedup).
        let mut curator2 = MemoryCurator::new(&paths).unwrap();
        curator2.graduation_enabled = true;
        let report2 = curator2.run_mechanical_pass().unwrap();
        assert!(
            report2.graduated.is_empty(),
            "second pass must not re-graduate same cluster"
        );
    }

    #[test]
    fn graduation_disabled_skips_entirely() {
        let (paths, _tmp) = temp_project();
        std::fs::create_dir_all(paths.sessions_dir()).unwrap();
        seed_sessions(&paths, 12);

        let store = crate::extras::memory_db::SqliteMemoryStore::load(&paths).unwrap();
        let base = "dirge uses SQLite for long term persistent memory storage and retrieval";
        store.add_entry("memory", base, None).unwrap();
        store
            .add_entry(
                "memory",
                "dirge uses SQLite for long term persistent memory storage and retrieval system",
                None,
            )
            .unwrap();
        drop(store);

        let mut curator = MemoryCurator::new(&paths).unwrap();
        curator.graduation_enabled = false;
        let report = curator.run_mechanical_pass().unwrap();
        assert!(report.graduated.is_empty(), "disabled → no graduation");

        let conn = crate::extras::memory_db::raw_conn(&paths);
        let ledger_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM graduation_ledger", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ledger_rows, 0, "disabled → no ledger rows");
    }
}
