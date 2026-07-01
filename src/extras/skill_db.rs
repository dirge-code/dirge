//! SQLite-backed reusable-skill store (dirge-70ht).
//!
//! A skill is a named, procedural-like memory with supporting content —
//! authored by `/learn` (dirge-s99m) from source material or a
//! conversation, then reused across sessions. Skills live in the
//! `skills` table of the per-project session DB (created idempotently in
//! [`crate::extras::session_db`]) and reuse the same salience machinery
//! as memories ([`crate::extras::salience`]): reinforce on invoke, decay
//! on disuse, effectiveness from a success/failure record, confidence as
//! a tiebreak. That reuse is the whole point — an unused skill decays out
//! of the prompt, an invoked-but-failing one sinks on negative
//! effectiveness, a working one stays hot — so the library self-prunes
//! instead of growing stale.
//!
//! Where memories carry five kinds, a skill is uniformly procedural, so
//! the effectiveness term is always live (no per-kind gate). `source`
//! separates agent-`learned` skills (DB-resident, subject to curation)
//! from `file`-registered ones (dirge-izju; git-tracked, pinned exempt).
//!
// dirge-a47a: this store is exercised only by its tests until the skill
// tool + curator land next round and give every API a real caller.
// Remove this allow when wiring R3 so genuine dead code resurfaces.
#![allow(dead_code)]

use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::extras::dirge_paths::ProjectPaths;
use crate::extras::salience::{
    DECAY_FLOOR, DEFAULT_CONFIDENCE, DISUSE_DECAY, RECENT_USE_BONUS, RECENT_USE_WINDOW_DAYS,
    USE_REINFORCEMENT, confidence_eviction_bonus, effectiveness_bonus,
};
use crate::extras::session_db::{SessionDb, redact_for_fts};

/// Base salience for a freshly learned skill. Skills are procedural-like,
/// so this matches `default_salience_for_kind(Procedural)` in the memory
/// store — the two stores start a playbook at the same importance.
const SKILL_BASE_SALIENCE: f64 = 0.5;

/// Max results returned by [`SkillStore::search`]. Mirrors the memory
/// store's search cap.
const SEARCH_RESULT_LIMIT: usize = 8;

/// One skill row as callers see it. Field-complete so the tool layer
/// (dirge-a47a) can render list/view/search without re-querying.
#[derive(Debug, Clone)]
pub struct SkillRow {
    pub uid: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub source: String,
    pub skill_path: Option<String>,
    pub status: String,
    pub tier: String,
    pub pinned: bool,
    pub confidence: f64,
    pub salience: f64,
    pub created_at: String,
    pub updated_at: String,
    pub last_used_at: Option<String>,
    pub use_count: i64,
    pub success_count: i64,
    pub failure_count: i64,
    pub last_success_at: Option<String>,
}

impl SkillRow {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(SkillRow {
            uid: row.get("uid")?,
            name: row.get("name")?,
            description: row.get("description")?,
            content: row.get("content")?,
            source: row.get("source")?,
            skill_path: row.get("skill_path")?,
            status: row.get("status")?,
            tier: row.get("tier")?,
            pinned: row.get::<_, i64>("pinned")? != 0,
            confidence: row.get("confidence")?,
            salience: row.get("salience")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            last_used_at: row.get("last_used_at")?,
            use_count: row.get("use_count")?,
            success_count: row.get("success_count")?,
            failure_count: row.get("failure_count")?,
            last_success_at: row.get("last_success_at")?,
        })
    }

    /// Salience folded with the live signals used for ranking and
    /// eviction: recency of use, proven effectiveness, and confidence.
    /// Unlike memories this needs no per-kind gate — every skill is a
    /// playbook, so the effectiveness term always applies.
    pub fn effective_salience(&self, recent_use_cutoff: &str) -> f64 {
        let recent = self
            .last_used_at
            .as_deref()
            .is_some_and(|t| t >= recent_use_cutoff);
        self.salience
            + if recent { RECENT_USE_BONUS } else { 0.0 }
            + effectiveness_bonus(self.success_count, self.failure_count)
            + confidence_eviction_bonus(self.confidence)
    }
}

/// Where a skill came from. `learned` skills are DB-resident and curated;
/// `file` skills are registered from disk (dirge-izju) and pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Learned,
    File,
}

impl SkillSource {
    fn as_str(&self) -> &'static str {
        match self {
            SkillSource::Learned => "learned",
            SkillSource::File => "file",
        }
    }
}

/// Validate a skill name: lowercase-hyphenated slug, ≤64 chars. Same
/// shape as the on-disk skill directory names and Hermes' rule, so a
/// learned skill and a file skill share one namespace.
pub fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err("Skill name must be 1–64 characters".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    {
        return Err(format!(
            "Skill name '{name}' must be lowercase letters, digits, hyphens, or dots"
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("Skill name must not start or end with a hyphen".to_string());
    }
    Ok(())
}

/// Port of the memory store's UMP id: 128 random bits, base32, prefixed.
fn random_skill_id() -> String {
    crate::extras::memory_db::random_entry_id()
}

/// The redacted FTS projection — name + description + body so a skill is
/// findable by title, with secret shapes scrubbed like `memories_fts`.
fn fts_projection(name: &str, description: &str, content: &str) -> String {
    redact_for_fts(&format!("{name}\n{description}\n{content}"))
}

/// SQLite-backed skill store. Holds the live DB connection; unlike the
/// memory store it captures no frozen snapshot here — prompt rendering
/// (dirge-a47a) queries ranked rows on demand.
pub struct SkillStore {
    conn: Mutex<Connection>,
}

impl SkillStore {
    /// Open (and migrate) the per-project session DB and build a store.
    /// Shares `state.db` with sessions and memory; the skills tables are
    /// created idempotently on open.
    pub fn load(paths: &ProjectPaths) -> Result<Self, String> {
        std::fs::create_dir_all(paths.sessions_dir())
            .map_err(|e| format!("Failed to create sessions directory: {e}"))?;
        let db = SessionDb::open(&paths.session_db_path())?;
        Self::from_connection(db.conn)
    }

    /// Build a store from an open, migrated connection. The seam the
    /// tests use with an in-memory or temp DB.
    pub fn from_connection(conn: Connection) -> Result<Self, String> {
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| format!("Failed to set busy timeout: {e}"))?;
        Ok(SkillStore {
            conn: Mutex::new(conn),
        })
    }

    /// Insert a new skill. Validates the name, threat-scans and redacts
    /// the content, and rejects a duplicate name. `learned` skills start
    /// unpinned and curated; `file` skills are pinned (eviction/archival
    /// exempt) since they're intentional and git-tracked.
    pub fn create(
        &self,
        name: &str,
        description: &str,
        content: &str,
        source: SkillSource,
        skill_path: Option<&str>,
    ) -> Result<SkillRow, String> {
        validate_skill_name(name)?;
        let description = description.trim();
        if description.is_empty() {
            return Err("Skill description must not be empty".to_string());
        }
        let content = content.trim();
        if content.is_empty() {
            return Err("Skill content must not be empty".to_string());
        }
        crate::extras::memory_db::scan_for_threats(content)?;
        let content = redact_for_fts(content);

        let conn = self.conn.lock().unwrap();
        if Self::get_locked(&conn, name)?.is_some() {
            return Err(format!("A skill named '{name}' already exists"));
        }

        let uid = random_skill_id();
        let now = chrono::Utc::now().to_rfc3339();
        let pinned = matches!(source, SkillSource::File);
        conn.execute(
            "INSERT INTO skills
                 (uid, name, description, content, source, skill_path, status,
                  tier, pinned, confidence, salience, created_at, updated_at,
                  use_count, success_count, failure_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 'hot', ?7, ?8, ?9, ?10, ?10, 0, 0, 0)",
            params![
                uid,
                name,
                description,
                content,
                source.as_str(),
                skill_path,
                pinned as i64,
                DEFAULT_CONFIDENCE,
                SKILL_BASE_SALIENCE,
                now,
            ],
        )
        .map_err(|e| format!("Failed to insert skill: {e}"))?;
        let rowid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO skills_fts(rowid, content) VALUES (?1, ?2)",
            params![rowid, fts_projection(name, description, &content)],
        )
        .map_err(|e| format!("Failed to index skill: {e}"))?;

        Self::get_locked(&conn, name)?
            .ok_or_else(|| "Skill vanished immediately after insert".to_string())
    }

    /// Fetch a skill by exact name (any status).
    pub fn get(&self, name: &str) -> Result<Option<SkillRow>, String> {
        let conn = self.conn.lock().unwrap();
        Self::get_locked(&conn, name)
    }

    fn get_locked(conn: &Connection, name: &str) -> Result<Option<SkillRow>, String> {
        conn.query_row(
            "SELECT * FROM skills WHERE name = ?1",
            params![name],
            SkillRow::from_row,
        )
        .optional()
        .map_err(|e| format!("Failed to fetch skill: {e}"))
    }

    /// All active skills, highest effective salience first (ties: oldest
    /// first, matching the memory store's stable ordering). This is the
    /// order the prompt index (dirge-a47a) renders and the curator
    /// (dirge-izju) evaluates.
    pub fn list_active(&self) -> Result<Vec<SkillRow>, String> {
        let conn = self.conn.lock().unwrap();
        let mut rows = Self::active_rows(&conn)?;
        let cutoff = recent_use_cutoff();
        rows.sort_by(|a, b| {
            b.effective_salience(&cutoff)
                .partial_cmp(&a.effective_salience(&cutoff))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
        Ok(rows)
    }

    fn active_rows(conn: &Connection) -> Result<Vec<SkillRow>, String> {
        let mut stmt = conn
            .prepare("SELECT * FROM skills WHERE status = 'active' ORDER BY id")
            .map_err(|e| format!("Failed to prepare active-skills query: {e}"))?;
        let rows = stmt
            .query_map([], SkillRow::from_row)
            .map_err(|e| format!("Failed to query skills: {e}"))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Record that a skill was invoked: bump the usage counter, stamp
    /// `last_used_at`, and reinforce salience — being reached for IS the
    /// relevance signal, same as a memory `expand`. Capped at 1.0.
    pub fn invoke(&self, name: &str) -> Result<SkillRow, String> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let changed = conn
            .execute(
                "UPDATE skills
                 SET use_count = use_count + 1, last_used_at = ?1,
                     salience = MIN(1.0, salience + ?2)
                 WHERE name = ?3 AND status = 'active'",
                params![now, USE_REINFORCEMENT, name],
            )
            .map_err(|e| format!("Failed to record skill invocation: {e}"))?;
        if changed == 0 {
            return Err(format!("No active skill named '{name}'"));
        }
        Self::get_locked(&conn, name)?
            .ok_or_else(|| format!("No active skill named '{name}'"))
    }

    /// Record a confirmed outcome for a skill (dirge-ygm3's review pass
    /// is the intended caller). Success bumps `success_count` and stamps
    /// `last_success_at`; failure bumps `failure_count`. Feeds the
    /// effectiveness term so a skill that keeps working outranks one that
    /// keeps failing.
    pub fn record_outcome(&self, name: &str, success: bool) -> Result<SkillRow, String> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let changed = if success {
            conn.execute(
                "UPDATE skills
                 SET success_count = success_count + 1, last_success_at = ?1
                 WHERE name = ?2 AND status = 'active'",
                params![now, name],
            )
        } else {
            conn.execute(
                "UPDATE skills SET failure_count = failure_count + 1
                 WHERE name = ?1 AND status = 'active'",
                params![name],
            )
        }
        .map_err(|e| format!("Failed to record skill outcome: {e}"))?;
        if changed == 0 {
            return Err(format!("No active skill named '{name}'"));
        }
        Self::get_locked(&conn, name)?
            .ok_or_else(|| format!("No active skill named '{name}'"))
    }

    /// Full-text search over active skills, BM25-ranked. Ties break by
    /// proven effectiveness, then salience, then confidence, then
    /// recency — the same ordering the memory search uses, minus the
    /// procedural CASE (every skill carries the outcome signal).
    pub fn search(&self, query: &str) -> Result<Vec<SkillRow>, String> {
        let fts_query = crate::extras::fts::quote_terms(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT s.* FROM skills_fts
                 JOIN skills s ON s.id = skills_fts.rowid
                 WHERE skills_fts MATCH ?1 AND s.status = 'active'
                 ORDER BY rank,
                          (s.success_count - s.failure_count) DESC,
                          s.salience DESC, s.confidence DESC,
                          s.last_used_at DESC
                 LIMIT ?2",
            )
            .map_err(|e| format!("Failed to prepare skill search: {e}"))?;
        let rows = stmt
            .query_map(params![fts_query, SEARCH_RESULT_LIMIT as i64], |r| {
                SkillRow::from_row(r)
            })
            .map_err(|e| format!("Failed to search skills: {e}"))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Decay the salience of stale, unconsulted, unpinned skills — the
    /// curator's mechanical pass (dirge-izju). Mirrors the memory decay:
    /// floor at [`DECAY_FLOOR`], and a skill still working within the
    /// window (`last_success_at >= cutoff`) is exempt so proven
    /// effectiveness outranks mere recency. Pinned (file) skills never
    /// decay. Returns how many rows changed.
    pub fn apply_disuse_decay(&self, cutoff_days: i64) -> Result<usize, String> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(cutoff_days)).to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE skills
             SET salience = MAX(?1, salience - ?2)
             WHERE status = 'active'
               AND pinned = 0
               AND NOT (last_success_at IS NOT NULL AND last_success_at >= ?3)
               AND created_at < ?3
               AND (last_used_at IS NULL OR last_used_at < ?3)
               AND salience > ?1",
            params![DECAY_FLOOR, DISUSE_DECAY, cutoff],
        )
        .map_err(|e| format!("Failed to apply skill disuse decay: {e}"))
    }

    /// Archive a learned skill (soft state — never a hard delete, so it
    /// stays restorable and auditable like a memory tombstone). Pinned
    /// (file) skills are refused: they're git-tracked, so removal belongs
    /// in the repo, not the curator. Returns whether a row changed.
    pub fn archive(&self, name: &str) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let changed = conn
            .execute(
                "UPDATE skills SET status = 'archived', updated_at = ?1
                 WHERE name = ?2 AND status = 'active' AND pinned = 0",
                params![now, name],
            )
            .map_err(|e| format!("Failed to archive skill: {e}"))?;
        Ok(changed > 0)
    }
}

/// The RFC3339 cutoff before which a use no longer counts as "recent".
fn recent_use_cutoff() -> String {
    (chrono::Utc::now() - chrono::Duration::days(RECENT_USE_WINDOW_DAYS)).to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SkillStore {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        // The skills tables live outside the version ladder; create them
        // directly the way `ensure_skills_tables` does on a real open.
        conn.execute_batch(
            "CREATE TABLE skills (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 uid TEXT NOT NULL UNIQUE, name TEXT NOT NULL UNIQUE,
                 description TEXT NOT NULL, content TEXT NOT NULL,
                 source TEXT NOT NULL DEFAULT 'learned',
                 skill_path TEXT,
                 status TEXT NOT NULL DEFAULT 'active',
                 tier TEXT NOT NULL DEFAULT 'hot',
                 pinned INTEGER NOT NULL DEFAULT 0,
                 confidence REAL NOT NULL DEFAULT 0.6,
                 salience REAL NOT NULL DEFAULT 0.5,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 last_used_at TEXT, use_count INTEGER NOT NULL DEFAULT 0,
                 success_count INTEGER NOT NULL DEFAULT 0,
                 failure_count INTEGER NOT NULL DEFAULT 0,
                 last_success_at TEXT, superseded_by TEXT, superseded_at TEXT);
             CREATE VIRTUAL TABLE skills_fts USING fts5(content);",
        )
        .expect("create skills tables");
        SkillStore::from_connection(conn).expect("build store")
    }

    #[test]
    fn create_and_get_roundtrip() {
        let s = store();
        let row = s
            .create(
                "deploy-web",
                "Deploy the web app to staging.",
                "# Deploy\n\nRun the deploy script.",
                SkillSource::Learned,
                None,
            )
            .expect("create");
        assert_eq!(row.name, "deploy-web");
        assert_eq!(row.source, "learned");
        assert!(!row.pinned);
        assert!((row.salience - SKILL_BASE_SALIENCE).abs() < 1e-9);
        let fetched = s.get("deploy-web").expect("get").expect("some");
        assert_eq!(fetched.content, "# Deploy\n\nRun the deploy script.");
    }

    #[test]
    fn duplicate_name_is_rejected() {
        let s = store();
        s.create("a-skill", "desc", "body", SkillSource::Learned, None)
            .expect("first");
        let err = s
            .create("a-skill", "other", "body2", SkillSource::Learned, None)
            .expect_err("dup rejected");
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn invalid_name_is_rejected() {
        let s = store();
        assert!(
            s.create("Bad Name", "d", "b", SkillSource::Learned, None)
                .is_err()
        );
        assert!(
            s.create("-lead", "d", "b", SkillSource::Learned, None)
                .is_err()
        );
    }

    #[test]
    fn file_skills_are_pinned() {
        let s = store();
        let row = s
            .create(
                "from-disk",
                "d",
                "b",
                SkillSource::File,
                Some("/repo/.dirge/skills/from-disk/SKILL.md"),
            )
            .expect("create file skill");
        assert!(row.pinned);
        assert_eq!(
            row.skill_path.as_deref(),
            Some("/repo/.dirge/skills/from-disk/SKILL.md")
        );
    }

    #[test]
    fn invoke_reinforces_salience_and_counts_use() {
        let s = store();
        s.create("s", "d", "b", SkillSource::Learned, None)
            .expect("create");
        let after = s.invoke("s").expect("invoke");
        assert_eq!(after.use_count, 1);
        assert!((after.salience - (SKILL_BASE_SALIENCE + USE_REINFORCEMENT)).abs() < 1e-9);
        assert!(after.last_used_at.is_some());
    }

    #[test]
    fn invoke_unknown_skill_errors() {
        let s = store();
        assert!(s.invoke("nope").is_err());
    }

    #[test]
    fn record_outcome_feeds_effectiveness_ordering() {
        let s = store();
        s.create("winner", "d", "b", SkillSource::Learned, None)
            .expect("w");
        s.create("loser", "d", "b", SkillSource::Learned, None)
            .expect("l");
        for _ in 0..5 {
            s.record_outcome("winner", true).expect("success");
        }
        for _ in 0..5 {
            s.record_outcome("loser", false).expect("failure");
        }
        let ranked = s.list_active().expect("list");
        assert_eq!(ranked.first().unwrap().name, "winner");
        assert_eq!(ranked.last().unwrap().name, "loser");
        // Effective salience reflects the record: winner up, loser down.
        let cutoff = recent_use_cutoff();
        let winner = s.get("winner").unwrap().unwrap();
        let loser = s.get("loser").unwrap().unwrap();
        assert!(winner.effective_salience(&cutoff) > SKILL_BASE_SALIENCE);
        assert!(loser.effective_salience(&cutoff) < SKILL_BASE_SALIENCE);
    }

    #[test]
    fn record_outcome_only_success_stamps_last_success_at() {
        let s = store();
        s.create("s", "d", "b", SkillSource::Learned, None)
            .expect("create");
        let ok = s.record_outcome("s", true).expect("success");
        assert_eq!(ok.success_count, 1);
        assert!(ok.last_success_at.is_some());
        let bad = s.record_outcome("s", false).expect("failure");
        assert_eq!(bad.failure_count, 1);
    }

    #[test]
    fn search_finds_by_title_and_body() {
        let s = store();
        s.create(
            "postgres-backup",
            "Back up the production database.",
            "Use pg_dump nightly.",
            SkillSource::Learned,
            None,
        )
        .expect("create");
        // Match on description ("database")…
        let by_desc = s.search("database").expect("search");
        assert_eq!(by_desc.len(), 1);
        assert_eq!(by_desc[0].name, "postgres-backup");
        // …and on body ("pg_dump").
        let by_body = s.search("pg_dump").expect("search");
        assert_eq!(by_body.len(), 1);
    }

    #[test]
    fn search_orders_effective_first() {
        let s = store();
        s.create("plain", "handles widgets", "widget body", SkillSource::Learned, None)
            .expect("plain");
        s.create("proven", "handles widgets", "widget body", SkillSource::Learned, None)
            .expect("proven");
        for _ in 0..3 {
            s.record_outcome("proven", true).expect("ok");
        }
        let hits = s.search("widget").expect("search");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "proven", "proven track record ranks first");
    }

    #[test]
    fn disuse_decay_lowers_stale_unpinned_salience_with_floor() {
        let s = store();
        s.create("stale", "d", "b", SkillSource::Learned, None)
            .expect("create");
        // Backdate creation so it's older than the cutoff window.
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE skills SET created_at = '2000-01-01T00:00:00Z' WHERE name = 'stale'",
                [],
            )
            .unwrap();
        }
        let changed = s.apply_disuse_decay(14).expect("decay");
        assert_eq!(changed, 1);
        let after = s.get("stale").unwrap().unwrap();
        assert!((after.salience - (SKILL_BASE_SALIENCE - DISUSE_DECAY)).abs() < 1e-9);
    }

    #[test]
    fn disuse_decay_exempts_recently_successful_and_pinned() {
        let s = store();
        s.create("proven", "d", "b", SkillSource::Learned, None)
            .expect("proven");
        s.create("pinned", "d", "b", SkillSource::File, Some("/p"))
            .expect("pinned");
        // Both backdated; proven has a fresh success, pinned is a file skill.
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE skills SET created_at = '2000-01-01T00:00:00Z'",
                [],
            )
            .unwrap();
        }
        s.record_outcome("proven", true).expect("recent success");
        let changed = s.apply_disuse_decay(14).expect("decay");
        assert_eq!(changed, 0, "recently-successful and pinned skills are exempt");
    }

    #[test]
    fn archive_soft_removes_learned_but_refuses_pinned() {
        let s = store();
        s.create("learned", "d", "b", SkillSource::Learned, None)
            .expect("learned");
        s.create("filed", "d", "b", SkillSource::File, Some("/p"))
            .expect("filed");
        assert!(s.archive("learned").expect("archive learned"));
        assert_eq!(s.get("learned").unwrap().unwrap().status, "archived");
        assert!(!s.archive("filed").expect("refuse pinned"));
        assert_eq!(s.get("filed").unwrap().unwrap().status, "active");
        // Archived skills drop out of the active listing.
        let active = s.list_active().expect("list");
        assert!(active.iter().all(|r| r.name != "learned"));
    }
}
