//! SQLite session database with FTS5 full-text search.
//!
//! Port of Hermes's `hermes_state.py`. Persists every session
//! transcript in a per-project SQLite database at
//! `.dirge/sessions/state.db`. Schema mirrors Hermes exactly:
//! sessions table + messages table + FTS5 virtual table with
//! content-sync triggers.
//!
//! Design decisions from Hermes preserved:
//! - WAL mode with fallback to DELETE on NFS/SMB
//! - Session splitting via parent_session_id chain
//! - Source tagging (cli, subagent, review-fork)
//! - Schema versioning with migrations
//! - FTS5 content sync triggers for auto-indexing

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

// Used in migrate() to set user_version pragma. pub(crate) so tests
// assert against the constant instead of a hardcoded number that
// breaks on every migration.
//
// SCHEMA_VERSION is feature-gated: v14 adds entities/relations tables
// behind `experimental-graph-search`. Without the feature, the schema
// caps at v13 so the user_version pragma never outruns the tables that
// actually exist.
#[cfg(feature = "experimental-graph-search")]
pub(crate) const SCHEMA_VERSION: u32 = 15;
#[cfg(not(feature = "experimental-graph-search"))]
pub(crate) const SCHEMA_VERSION: u32 = 13;

/// Thread-safe snapshot of the most recent `SessionDb::open()` failure.
/// Port of Hermes's `_last_init_error` (hermes_state.py:66-67).
/// Slash-command handlers read this to surface the underlying cause.
static LAST_INIT_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Return the most recent session DB init failure, if any.
/// Port of Hermes's `get_last_init_error()` (hermes_state.py:94-100).
#[allow(dead_code)]
pub fn last_init_error() -> Option<String> {
    LAST_INIT_ERROR.lock().unwrap().clone()
}

fn set_last_init_error(msg: Option<String>) {
    if let Ok(mut guard) = LAST_INIT_ERROR.lock() {
        *guard = msg;
    }
}

/// SESS-14: scrub credential-shaped tokens from text before it lands in
/// the FTS5 index. Ported from hermes-agent/agent/redact.py (the
/// `_PREFIX_PATTERNS`, `_DB_CONNSTR_RE`, `_URL_USERINFO_RE`,
/// `_AUTH_HEADER_RE`, `_ENV_ASSIGN_RE` patterns) — same coverage as
/// `sandbox::is_sensitive_env_value`, but applied as a *replace* (not
/// a yes/no test) since we still need a searchable, non-secret
/// projection of the message text.
///
/// Raw content stays in `messages.content` / `messages.tool_calls`;
/// only the searchable projection passed to `messages_fts` and
/// `messages_fts_trigram` is redacted. Anyone reading a transcript
/// back out sees the unredacted original.
///
/// Each match is replaced with `<REDACTED>`. Pre-checks gate each
/// regex on a cheap substring so the common no-secret case stays
/// fast (a single line of plain prose pays for the gate misses only).
pub fn redact_for_fts(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    static VENDOR_PREFIX_RE: OnceLock<Regex> = OnceLock::new();
    static URL_USERINFO_RE: OnceLock<Regex> = OnceLock::new();
    static AUTH_HEADER_RE: OnceLock<Regex> = OnceLock::new();
    static ENV_ASSIGN_RE: OnceLock<Regex> = OnceLock::new();
    static JSON_FIELD_RE: OnceLock<Regex> = OnceLock::new();
    static JWT_RE: OnceLock<Regex> = OnceLock::new();

    let mut out: std::borrow::Cow<'_, str> = text.into();

    // Vendor prefix tokens. Same set as
    // sandbox::is_sensitive_env_value — kept in sync deliberately.
    let has_prefix_gate = out.contains("AKIA")
        || out.contains("ghp_")
        || out.contains("github_pat_")
        || out.contains("gho_")
        || out.contains("ghu_")
        || out.contains("ghs_")
        || out.contains("xox")
        || out.contains("sk-")
        || out.contains("sk_live_")
        || out.contains("sk_test_")
        || out.contains("AIza")
        || out.contains("hf_")
        || out.contains("xai-");
    if has_prefix_gate {
        let re = VENDOR_PREFIX_RE.get_or_init(|| {
            Regex::new(
                r"(?x)
                (?:
                      AKIA[0-9A-Z]{16}
                    | ghp_[A-Za-z0-9]{36}
                    | github_pat_[A-Za-z0-9_]{20,}
                    | gho_[A-Za-z0-9]{30,}
                    | ghu_[A-Za-z0-9]{30,}
                    | ghs_[A-Za-z0-9]{30,}
                    | xox[baprs]-[A-Za-z0-9-]{10,}
                    | sk-[A-Za-z0-9_-]{20,}
                    | sk_live_[A-Za-z0-9]{20,}
                    | sk_test_[A-Za-z0-9]{20,}
                    | AIza[A-Za-z0-9_-]{30,}
                    | hf_[A-Za-z0-9]{30,}
                    | xai-[A-Za-z0-9]{30,}
                )
                ",
            )
            .unwrap()
        });
        if re.is_match(&out) {
            out = re.replace_all(&out, "<REDACTED>").into_owned().into();
        }
    }

    // JWTs (3-part eyJ...) — gate on "eyJ" substring.
    if out.contains("eyJ") {
        let re = JWT_RE.get_or_init(|| {
            Regex::new(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_=-]{4,}").unwrap()
        });
        if re.is_match(&out) {
            out = re.replace_all(&out, "<REDACTED>").into_owned().into();
        }
    }

    // URLs with userinfo: scheme://user:pass@host
    if out.contains("://") {
        let re = URL_USERINFO_RE.get_or_init(|| {
            Regex::new(r"([A-Za-z][A-Za-z0-9+.\-]*://)([^/\s:@]*):([^/\s@]+)@").unwrap()
        });
        if re.is_match(&out) {
            out = re.replace_all(&out, "${1}<REDACTED>@").into_owned().into();
        }
    }

    // Authorization: Bearer <token>
    if out.contains("uthorization") || out.contains("UTHORIZATION") {
        let re = AUTH_HEADER_RE
            .get_or_init(|| Regex::new(r"(?i)(Authorization:\s*Bearer\s+)\S+").unwrap());
        if re.is_match(&out) {
            out = re.replace_all(&out, "${1}<REDACTED>").into_owned().into();
        }
    }

    // KEY=value / TOKEN=value / SECRET=value / PASSWORD=value /
    // CREDENTIAL=value / AUTH=value (env-style)
    if out.contains('=') {
        let re = ENV_ASSIGN_RE.get_or_init(|| {
            Regex::new(
                r#"(?i)([A-Za-z0-9_]*(?:API_?KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|AUTH)[A-Za-z0-9_]*\s*=\s*)['"]?[^\s'"&]+"#,
            )
            .unwrap()
        });
        if re.is_match(&out) {
            out = re.replace_all(&out, "${1}<REDACTED>").into_owned().into();
        }
    }

    // JSON-ish fields: "api_key": "value", "token": "value", …
    if out.contains(':') && out.contains('"') {
        let re = JSON_FIELD_RE.get_or_init(|| {
            Regex::new(
                r#"(?i)("(?:api_?key|token|secret|password|access_token|refresh_token|auth_token|bearer)"\s*:\s*)"[^"]+""#,
            )
            .unwrap()
        });
        if re.is_match(&out) {
            out = re
                .replace_all(&out, "${1}\"<REDACTED>\"")
                .into_owned()
                .into();
        }
    }

    out.into_owned()
}

pub struct SessionDb {
    pub(crate) conn: Connection,
}

impl SessionDb {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = match Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        ) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("Failed to open session DB at {}: {e}", path.display());
                set_last_init_error(Some(msg.clone()));
                return Err(msg);
            }
        };

        // WAL mode with fallback
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Ok(_) => {}
            Err(e) => {
                let msg = format!(
                    "WAL mode unavailable for {} — falling back to DELETE journal: {e}",
                    path.display()
                );
                tracing::warn!(
                    target: "dirge::session_db",
                    path = %path.display(),
                    "WAL mode unavailable — falling back to DELETE journal"
                );
                set_last_init_error(Some(msg));
                conn.pragma_update(None, "journal_mode", "DELETE")
                    .map_err(|e| format!("Failed to set DELETE journal mode: {e}"))?;
            }
        }

        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| {
                let msg = format!("Failed to enable foreign keys: {e}");
                set_last_init_error(Some(msg.clone()));
                msg
            })?;

        let db = SessionDb { conn };
        db.migrate()?;
        // Clear the error on successful open.
        set_last_init_error(None);
        Ok(db)
    }

    /// Serialized migration entry point. Several components (session
    /// persistence, memory store, session search) open this DB, and
    /// they can open it CONCURRENTLY on a fresh file — without
    /// serialization both connections read user_version=0 and both
    /// run v1's CREATE TABLE, so the loser errors out (PR #392 CI).
    /// BEGIN EXCLUSIVE makes the loser wait (up to the busy timeout),
    /// then re-read the version the winner committed and skip the
    /// completed migrations.
    fn migrate(&self) -> Result<(), String> {
        let _ = self.conn.busy_timeout(std::time::Duration::from_secs(30));
        self.conn
            .execute_batch("BEGIN EXCLUSIVE")
            .map_err(|e| format!("Failed to lock DB for migration: {e}"))?;
        match self.run_pending_migrations() {
            Ok(()) => self
                .conn
                .execute_batch("COMMIT")
                .map_err(|e| format!("Failed to commit migrations: {e}")),
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Must run under the exclusive transaction `migrate` opened —
    /// the version read below is only race-free while locked.
    fn run_pending_migrations(&self) -> Result<(), String> {
        // Feature-independent, out of the version ladder (see
        // `ensure_skills_tables`). Must run BEFORE the early-return so
        // DBs already at SCHEMA_VERSION still gain the skills tables.
        self.ensure_skills_tables()?;

        let current: u32 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|e| format!("Failed to read schema version: {e}"))?;

        if current >= SCHEMA_VERSION {
            return Ok(());
        }

        if current < 1 {
            self.run_migration_v1()?;
        }

        if current < 2 {
            self.run_migration_v2()?;
        }

        if current < 3 {
            self.run_migration_v3()?;
        }

        if current < 4 {
            self.run_migration_v4()?;
        }

        if current < 5 {
            self.run_migration_v5()?;
        }

        if current < 6 {
            self.run_migration_v6()?;
        }

        if current < 7 {
            self.run_migration_v7()?;
        }

        if current < 8 {
            self.run_migration_v8()?;
        }

        if current < 9 {
            self.run_migration_v9()?;
        }

        if current < 10 {
            self.run_migration_v10()?;
        }

        if current < 11 {
            self.run_migration_v11()?;
        }

        if current < 12 {
            self.run_migration_v12()?;
        }

        if current < 13 {
            self.run_migration_v13()?;
        }

        #[cfg(feature = "experimental-graph-search")]
        if current < 14 {
            self.run_migration_v14()?;
        }

        #[cfg(feature = "experimental-graph-search")]
        if current < 15 {
            self.run_migration_v15()?;
        }

        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| format!("Failed to set schema version: {e}"))?;

        Ok(())
    }

    fn run_migration_v1(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE sessions (
                    id              TEXT PRIMARY KEY,
                    parent_session_id TEXT,
                    source          TEXT NOT NULL DEFAULT 'cli',
                    model           TEXT NOT NULL DEFAULT '',
                    provider        TEXT NOT NULL DEFAULT '',
                    started_at      TEXT NOT NULL,
                    last_active     TEXT NOT NULL,
                    title           TEXT NOT NULL DEFAULT '',
                    message_count   INTEGER NOT NULL DEFAULT 0,
                    input_tokens    INTEGER NOT NULL DEFAULT 0,
                    output_tokens   INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE messages (
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id      TEXT NOT NULL REFERENCES sessions(id),
                    role            TEXT NOT NULL,
                    content         TEXT NOT NULL DEFAULT '',
                    tool_name       TEXT,
                    tool_calls      TEXT,
                    tool_call_id    TEXT,
                    timestamp       TEXT NOT NULL
                );

                CREATE INDEX idx_messages_session ON messages(session_id);
                CREATE INDEX idx_messages_role ON messages(session_id, role);

                CREATE VIRTUAL TABLE messages_fts USING fts5(
                    content,
                    content=messages,
                    content_rowid=id
                );

                -- FTS5 content sync triggers — index content + tool_name + tool_calls
                -- so searches for tool names find their messages.
                -- Port of Hermes's FTS_SQL (hermes_state.py:255-278).
                CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                    INSERT INTO messages_fts(rowid, content) VALUES (
                        new.id,
                        COALESCE(new.content, '') || ' ' ||
                        COALESCE(new.tool_name, '') || ' ' ||
                        COALESCE(new.tool_calls, '')
                    );
                END;

                CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content)
                    VALUES ('delete', old.id, old.content);
                END;

                CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content)
                    VALUES ('delete', old.id, old.content);
                    INSERT INTO messages_fts(rowid, content) VALUES (
                        new.id,
                        COALESCE(new.content, '') || ' ' ||
                        COALESCE(new.tool_name, '') || ' ' ||
                        COALESCE(new.tool_calls, '')
                    );
                END;
                ",
            )
            .map_err(|e| format!("Migration v1 failed: {e}"))?;

        Ok(())
    }

    /// v2: rebuild FTS5 triggers with tool_name/tool_calls in the index
    /// and backfill all existing rows. DBs created by v1 had triggers
    /// that only indexed `new.content` — tool names were invisible to search.
    fn run_migration_v2(&self) -> Result<(), String> {
        // Drop old triggers (IF EXISTS for DBs created after the v1 fix above).
        self.conn
            .execute_batch(
                "
                DROP TRIGGER IF EXISTS messages_ai;
                DROP TRIGGER IF EXISTS messages_ad;
                DROP TRIGGER IF EXISTS messages_au;

                CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                    INSERT INTO messages_fts(rowid, content) VALUES (
                        new.id,
                        COALESCE(new.content, '') || ' ' ||
                        COALESCE(new.tool_name, '') || ' ' ||
                        COALESCE(new.tool_calls, '')
                    );
                END;

                CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content)
                    VALUES ('delete', old.id, old.content);
                END;

                CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content)
                    VALUES ('delete', old.id, old.content);
                    INSERT INTO messages_fts(rowid, content) VALUES (
                        new.id,
                        COALESCE(new.content, '') || ' ' ||
                        COALESCE(new.tool_name, '') || ' ' ||
                        COALESCE(new.tool_calls, '')
                    );
                END;
                ",
            )
            .map_err(|e| format!("Migration v2 triggers failed: {e}"))?;

        // Backfill: delete stale v1 content entries, then re-insert
        // with the composite content + tool_name + tool_calls formula.
        // External-content FTS5 tables don't auto-rebuild with a new
        // formula — the trigger controls what content is indexed.
        self.conn
            .execute("DELETE FROM messages_fts", [])
            .map_err(|e| format!("Migration v2 delete failed: {e}"))?;

        self.conn
            .execute(
                "INSERT INTO messages_fts(rowid, content)
                 SELECT id,
                        COALESCE(content, '') || ' ' ||
                        COALESCE(tool_name, '') || ' ' ||
                        COALESCE(tool_calls, '')
                 FROM messages",
                [],
            )
            .map_err(|e| format!("Migration v2 backfill failed: {e}"))?;

        Ok(())
    }

    /// v3: add trigram FTS5 table for CJK/substring search.
    /// Port of Hermes's FTS_TRIGRAM_SQL (hermes_state.py:284-308).
    /// The default unicode61 tokenizer splits CJK characters into
    /// individual tokens, breaking phrase matching. The trigram
    /// tokenizer creates overlapping 3-character sequences so
    /// substring queries work natively for any script.
    fn run_migration_v3(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts_trigram USING fts5(
                    content,
                    tokenize='trigram'
                );

                CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_insert AFTER INSERT ON messages BEGIN
                    INSERT INTO messages_fts_trigram(rowid, content) VALUES (
                        new.id,
                        COALESCE(new.content, '') || ' ' ||
                        COALESCE(new.tool_name, '') || ' ' ||
                        COALESCE(new.tool_calls, '')
                    );
                END;

                CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_delete AFTER DELETE ON messages BEGIN
                    DELETE FROM messages_fts_trigram WHERE rowid = old.id;
                END;

                CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_update AFTER UPDATE ON messages BEGIN
                    DELETE FROM messages_fts_trigram WHERE rowid = old.id;
                    INSERT INTO messages_fts_trigram(rowid, content) VALUES (
                        new.id,
                        COALESCE(new.content, '') || ' ' ||
                        COALESCE(new.tool_name, '') || ' ' ||
                        COALESCE(new.tool_calls, '')
                    );
                END;
                ",
            )
            .map_err(|e| format!("Migration v3 failed: {e}"))?;

        // Backfill trigram index from existing messages.
        self.conn
            .execute(
                "INSERT INTO messages_fts_trigram(rowid, content)
                 SELECT id,
                        COALESCE(content, '') || ' ' ||
                        COALESCE(tool_name, '') || ' ' ||
                        COALESCE(tool_calls, '')
                 FROM messages
                 WHERE id NOT IN (SELECT rowid FROM messages_fts_trigram)",
                [],
            )
            .map_err(|e| format!("Migration v3 backfill failed: {e}"))?;

        Ok(())
    }

    /// v4: add session lifecycle + cost-tracking columns.
    /// Port of Hermes's sessions schema (hermes_state.py:190-222).
    fn run_migration_v4(&self) -> Result<(), String> {
        for col in &[
            "ended_at TEXT",
            "end_reason TEXT",
            "tool_call_count INTEGER DEFAULT 0",
            "api_call_count INTEGER DEFAULT 0",
        ] {
            if let Err(e) = self
                .conn
                .execute(&format!("ALTER TABLE sessions ADD COLUMN {col}"), [])
            {
                // Duplicate column name is harmless — the column
                // already exists from a partial previous migration.
                if !e.to_string().contains("duplicate column name") {
                    return Err(format!("Migration v4 failed on {col}: {e}"));
                }
            }
        }
        Ok(())
    }

    /// v6: SESS-14 — drop the auto-INSERT / auto-UPDATE FTS triggers so
    /// the application can redact secrets before they land in the
    /// full-text index. The raw text stays in `messages.content` /
    /// `messages.tool_calls`, but `messages_fts` and
    /// `messages_fts_trigram` only receive a redacted projection
    /// supplied by `insert_message`.
    ///
    /// AFTER DELETE triggers stay in place — purging from the FTS
    /// table on a row delete doesn't need any redaction.
    ///
    /// Backfill: re-insert the existing row contents into both FTS
    /// tables after passing them through `redact_for_fts`. Existing
    /// indexes were built from raw content; without this step a
    /// search would still hit pre-v6 secrets.
    fn run_migration_v6(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                DROP TRIGGER IF EXISTS messages_ai;
                DROP TRIGGER IF EXISTS messages_au;
                DROP TRIGGER IF EXISTS messages_fts_trigram_insert;
                DROP TRIGGER IF EXISTS messages_fts_trigram_update;
                ",
            )
            .map_err(|e| format!("Migration v6 trigger drop failed: {e}"))?;

        // Backfill: clear both indexes then re-insert with redacted
        // content row-by-row so the redactor runs on each row.
        self.conn
            .execute("DELETE FROM messages_fts", [])
            .map_err(|e| format!("Migration v6 clear fts failed: {e}"))?;
        self.conn
            .execute("DELETE FROM messages_fts_trigram", [])
            .map_err(|e| format!("Migration v6 clear trigram failed: {e}"))?;

        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, COALESCE(content, ''), COALESCE(tool_name, ''), COALESCE(tool_calls, '')
                 FROM messages",
            )
            .map_err(|e| format!("Migration v6 select failed: {e}"))?;

        let mut dropped = 0usize;
        let rows: Vec<(i64, String, String, String)> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .map_err(|e| format!("Migration v6 query failed: {e}"))?
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    dropped += 1;
                    tracing::warn!(
                        target: "dirge::session_db",
                        error = %e,
                        "migration v6 backfill skipped an undeserializable message row",
                    );
                    None
                }
            })
            .collect();
        drop(stmt);
        if dropped > 0 {
            // dirge-slj2: silence here looked like a complete backfill;
            // skipped rows are simply absent from search results.
            tracing::warn!(
                target: "dirge::session_db",
                dropped,
                "migration v6 backfill skipped {dropped} message row(s) — they will not appear in FTS search",
            );
        }

        for (id, content, tool_name, tool_calls) in rows {
            let combined = format!("{content} {tool_name} {tool_calls}");
            let redacted = redact_for_fts(&combined);
            self.conn
                .execute(
                    "INSERT INTO messages_fts(rowid, content) VALUES (?1, ?2)",
                    params![id, redacted],
                )
                .map_err(|e| format!("Migration v6 fts backfill failed at row {id}: {e}"))?;
            self.conn
                .execute(
                    "INSERT INTO messages_fts_trigram(rowid, content) VALUES (?1, ?2)",
                    params![id, redacted],
                )
                .map_err(|e| format!("Migration v6 trigram backfill failed at row {id}: {e}"))?;
        }
        Ok(())
    }

    /// v5: add message detail columns.
    /// Port of Hermes's messages schema (hermes_state.py:224-242).
    fn run_migration_v5(&self) -> Result<(), String> {
        for col in &["token_count INTEGER", "finish_reason TEXT"] {
            if let Err(e) = self
                .conn
                .execute(&format!("ALTER TABLE messages ADD COLUMN {col}"), [])
                && !e.to_string().contains("duplicate column name")
            {
                return Err(format!("Migration v5 failed on {col}: {e}"));
            }
        }
        Ok(())
    }

    /// v7 (dirge-18ks): per-project long-term memory moves from
    /// MEMORY.md / PITFALLS.md markdown files (+ .meta.json /
    /// .usage.json sidecars) into the session DB, giving one uniform
    /// store. `memories` carries the UMP lifecycle fields that were
    /// previously sidecar-only; `memories_fts` is a STANDALONE FTS5
    /// table (not external-content) so index deletes are exact even
    /// though the indexed text is a redacted projection — the v6
    /// external-content pattern can't delete cleanly when the indexed
    /// text differs from the content column. Sync is app-managed in
    /// `extras::memory_db` (no triggers), matching the post-v6
    /// redact-before-index philosophy.
    fn run_migration_v7(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS memories (
                    id            INTEGER PRIMARY KEY AUTOINCREMENT,
                    uid           TEXT NOT NULL UNIQUE,
                    target        TEXT NOT NULL CHECK(target IN ('memory','pitfalls')),
                    kind          TEXT NOT NULL DEFAULT 'procedural',
                    content       TEXT NOT NULL,
                    status        TEXT NOT NULL DEFAULT 'active',
                    tier          TEXT NOT NULL DEFAULT 'hot',
                    confidence    REAL NOT NULL DEFAULT 0.6,
                    salience      REAL NOT NULL DEFAULT 0.5,
                    created_at    TEXT NOT NULL,
                    updated_at    TEXT NOT NULL,
                    last_used_at  TEXT,
                    use_count     INTEGER NOT NULL DEFAULT 0,
                    superseded_by TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_memories_target_status
                    ON memories(target, status);

                CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                    content
                );
                ",
            )
            .map_err(|e| format!("Migration v7 failed: {e}"))?;
        Ok(())
    }

    /// v8 (dirge-slj2): drop the `messages_ad` AFTER DELETE trigger.
    /// v6 made `messages_fts` hold a REDACTED, CONCATENATED projection
    /// (content + tool_name + tool_calls through `redact_for_fts`),
    /// but this v1/v2-era trigger issued the external-content FTS5
    /// 'delete' command with raw `old.content` — and FTS5 'delete'
    /// requires the EXACT values that were indexed, so any message
    /// delete would have corrupted the index. (Latent: nothing
    /// deleted messages until `delete_session_messages` below, which
    /// recomputes the exact projection.) `messages_fts_trigram_delete`
    /// stays — the trigram table is standalone FTS5 where plain
    /// `DELETE ... WHERE rowid` is well-defined.
    fn run_migration_v8(&self) -> Result<(), String> {
        self.conn
            .execute_batch("DROP TRIGGER IF EXISTS messages_ad;")
            .map_err(|e| format!("Migration v8 failed: {e}"))?;
        Ok(())
    }

    /// v9 (dirge-lerb): drop the `memories.confidence` column. It was
    /// write-only — set to a constant default on insert and echoed in
    /// the tool's view response, but never read by eviction, search,
    /// or curation. The column isn't indexed and has no trigger/view
    /// dependency, so a plain DROP COLUMN is safe. `IF EXISTS`-style
    /// guard via a pre-check so a partially-migrated DB doesn't error.
    fn run_migration_v9(&self) -> Result<(), String> {
        let has_confidence: bool = self
            .conn
            .prepare("SELECT 1 FROM pragma_table_info('memories') WHERE name = 'confidence'")
            .and_then(|mut s| s.exists([]))
            .unwrap_or(false);
        if has_confidence {
            self.conn
                .execute_batch("ALTER TABLE memories DROP COLUMN confidence;")
                .map_err(|e| format!("Migration v9 failed: {e}"))?;
        }
        Ok(())
    }

    /// v10 (feat/session-checkpoint): durable structured session state.
    /// One row per session holding the evolved compaction artifact — the
    /// regenerated multi-section `summary` plus a write-once `intent`
    /// slot that anchors the original goal across folds and resumes. This
    /// replaces nothing yet; it's the store the checkpoint path writes to
    /// instead of the transient index-0 summary message. Kept in SQLite
    /// (not a markdown file) so it shares the per-project state.db.
    fn run_migration_v10(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS session_checkpoints (
                    session_id   TEXT PRIMARY KEY,
                    intent       TEXT NOT NULL DEFAULT '',
                    summary      TEXT NOT NULL DEFAULT '',
                    revision     INTEGER NOT NULL DEFAULT 0,
                    created_at   TEXT NOT NULL,
                    updated_at   TEXT NOT NULL
                );
                ",
            )
            .map_err(|e| format!("Migration v10 failed: {e}"))?;
        Ok(())
    }

    /// v11: spec-driven workflow tracker (OpenSpec-inspired, SQLite-backed).
    /// Living specs (capabilities → requirements → scenarios) are the
    /// current truth; a `spec_changes` row carries `spec_deltas` against
    /// them plus a `spec_tasks` checklist with real status, folded into the
    /// living specs at archive time. Replaces OpenSpec's markdown-folder
    /// tree with queryable rows — no silent parse failures, real task
    /// status, transactional archive.
    fn run_migration_v11(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS spec_capabilities (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    name        TEXT NOT NULL UNIQUE,
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS spec_requirements (
                    id            INTEGER PRIMARY KEY AUTOINCREMENT,
                    capability_id INTEGER NOT NULL
                                  REFERENCES spec_capabilities(id) ON DELETE CASCADE,
                    name          TEXT NOT NULL,
                    text          TEXT NOT NULL,
                    created_at    TEXT NOT NULL,
                    updated_at    TEXT NOT NULL,
                    UNIQUE(capability_id, name)
                );

                CREATE TABLE IF NOT EXISTS spec_scenarios (
                    id             INTEGER PRIMARY KEY AUTOINCREMENT,
                    requirement_id INTEGER NOT NULL
                                   REFERENCES spec_requirements(id) ON DELETE CASCADE,
                    name           TEXT NOT NULL,
                    when_then      TEXT NOT NULL,
                    created_at     TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS spec_changes (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    slug        TEXT NOT NULL UNIQUE,
                    title       TEXT NOT NULL DEFAULT '',
                    why         TEXT NOT NULL DEFAULT '',
                    what        TEXT NOT NULL DEFAULT '',
                    design      TEXT NOT NULL DEFAULT '',
                    status      TEXT NOT NULL DEFAULT 'draft',
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    archived_at TEXT
                );

                CREATE TABLE IF NOT EXISTS spec_deltas (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    change_id   INTEGER NOT NULL
                                REFERENCES spec_changes(id) ON DELETE CASCADE,
                    op          TEXT NOT NULL,
                    capability  TEXT NOT NULL,
                    requirement TEXT NOT NULL,
                    text        TEXT NOT NULL DEFAULT '',
                    scenarios   TEXT NOT NULL DEFAULT '',
                    reason      TEXT NOT NULL DEFAULT '',
                    migration   TEXT NOT NULL DEFAULT '',
                    rename_to   TEXT NOT NULL DEFAULT '',
                    created_at  TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS spec_tasks (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    change_id   INTEGER NOT NULL
                                REFERENCES spec_changes(id) ON DELETE CASCADE,
                    group_no    INTEGER NOT NULL DEFAULT 1,
                    seq         INTEGER NOT NULL DEFAULT 1,
                    text        TEXT NOT NULL,
                    status      TEXT NOT NULL DEFAULT 'pending',
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_spec_deltas_change
                    ON spec_deltas(change_id);
                CREATE INDEX IF NOT EXISTS idx_spec_tasks_change
                    ON spec_tasks(change_id, group_no, seq);
                CREATE INDEX IF NOT EXISTS idx_spec_requirements_cap
                    ON spec_requirements(capability_id);
                CREATE INDEX IF NOT EXISTS idx_spec_scenarios_req
                    ON spec_scenarios(requirement_id);
                ",
            )
            .map_err(|e| format!("Migration v11 failed: {e}"))?;
        Ok(())
    }

    /// v12 (dirge-zygq): procedural effectiveness signal. Procedural
    /// memories are playbooks ("how to do X") whose value is whether
    /// they actually worked, not how recently they were tried — the
    /// Elastic agent-memory point that a decay multiplier rewards
    /// "recently tried" over "recently effective". These counters let
    /// ranking and eviction favor playbooks with a positive track
    /// record; `extras::memory_db` exempts procedural from disuse
    /// decay and folds `success_count - failure_count` into the
    /// effective-salience used for eviction and search ordering.
    /// `last_success_at` records the most recent confirmed success so
    /// a future "recently effective" decay can key off it. Columns are
    /// inert for non-procedural kinds (never written by
    /// `record_outcome`). `ADD COLUMN` is idempotent-guarded by the
    /// duplicate-column check so a partially-migrated DB doesn't error.
    fn run_migration_v12(&self) -> Result<(), String> {
        for col in &[
            "success_count INTEGER NOT NULL DEFAULT 0",
            "failure_count INTEGER NOT NULL DEFAULT 0",
            "last_success_at TEXT",
        ] {
            if let Err(e) = self
                .conn
                .execute(&format!("ALTER TABLE memories ADD COLUMN {col}"), [])
                && !e.to_string().contains("duplicate column name")
            {
                return Err(format!("Migration v12 failed on {col}: {e}"));
            }
        }
        Ok(())
    }

    /// v13 (dirge-fa10): confidence axis + contradiction supersession.
    /// `confidence` returns the truth-likelihood scalar that v9 dropped
    /// (dirge-lerb) as write-only — this time it is READ by eviction,
    /// search ordering, and curation, and WRITTEN with meaning by the
    /// supersession path (a harsh contradiction lands the successor at
    /// reduced confidence). `superseded_at` timestamps when an entry was
    /// retired by a newer fact; the `superseded_by` column already exists
    /// (added inert in v7) and now holds the successor's uid. A
    /// superseded entry is moved to `status='superseded'`, which the
    /// active-only queries already exclude, so it leaves the snapshot,
    /// views, search, and eviction while staying in the table as an
    /// audit record. `ADD COLUMN` is duplicate-guarded so a
    /// partially-migrated DB doesn't error.
    fn run_migration_v13(&self) -> Result<(), String> {
        for col in &["confidence REAL NOT NULL DEFAULT 0.6", "superseded_at TEXT"] {
            if let Err(e) = self
                .conn
                .execute(&format!("ALTER TABLE memories ADD COLUMN {col}"), [])
                && !e.to_string().contains("duplicate column name")
            {
                return Err(format!("Migration v13 failed on {col}: {e}"));
            }
        }
        Ok(())
    }

    /// v14: entity/relation graph tables for PRISM-style recursive CTE
    /// queries over structured tool-output facts (#393). Gated behind
    /// `experimental-graph-search` — if the feature is never enabled
    /// this migration never runs. FTS5 is standalone (no triggers,
    /// app-managed sync via DELETE+INSERT on entity create/update),
    /// matching the v7 memories_fts pattern.
    #[cfg(feature = "experimental-graph-search")]
    fn run_migration_v14(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE entities (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL REFERENCES sessions(id),
                    message_id INTEGER REFERENCES messages(id),
                    kind TEXT NOT NULL,
                    name TEXT NOT NULL,
                    extra TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE INDEX idx_entities_session ON entities(session_id);
                CREATE INDEX idx_entities_kind ON entities(kind);
                CREATE INDEX idx_entities_name ON entities(name);

                CREATE TABLE relations (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    source_id INTEGER NOT NULL REFERENCES entities(id),
                    target_id INTEGER NOT NULL REFERENCES entities(id),
                    rel_type TEXT NOT NULL,
                    session_id TEXT NOT NULL REFERENCES sessions(id),
                    confidence REAL DEFAULT 1.0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE INDEX idx_relations_source ON relations(source_id, rel_type);
                CREATE INDEX idx_relations_target ON relations(target_id, rel_type);
                CREATE INDEX idx_relations_session ON relations(session_id);

                CREATE VIRTUAL TABLE entities_fts USING fts5(
                    name, kind,
                    tokenize='unicode61'
                );
                ",
            )
            .map_err(|e| format!("Migration v14 failed: {e}"))?;

        Ok(())
    }

    /// v15: add schema_version column to entities for PRISM memory schema
    /// (#393). Defaults to 'generic' — PRISM-typed relations (HAS_FACET,
    /// DERIVED_FROM, etc.) only apply when schema_version is 'prism'.
    #[cfg(feature = "experimental-graph-search")]
    fn run_migration_v15(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "ALTER TABLE entities ADD COLUMN schema_version TEXT NOT NULL DEFAULT 'generic';",
            )
            .map_err(|e| format!("Migration v15 failed: {e}"))?;

        Ok(())
    }

    /// Create the `skills` tables (dirge-70ht). Runs UNCONDITIONALLY on
    /// every open, ahead of the version ladder, rather than as a numbered
    /// migration. `SCHEMA_VERSION` is feature-gated (13 without
    /// `experimental-graph-search`, 15 with it) so the graph tables can
    /// be enabled on an existing DB later — an invariant that only holds
    /// while a no-feature DB stays below v14. A feature-independent table
    /// numbered above the gated ones would break that upgrade path (see
    /// the notes on SCHEMA_VERSION), so skills instead follow the
    /// idempotent `IF NOT EXISTS` pattern the memories table already uses
    /// and stay out of the `user_version` namespace entirely.
    ///
    /// Schema mirrors `memories` (dirge-70ht reuses its salience engine):
    /// a skill is a named, procedural-like memory with a success/failure
    /// record. `source` distinguishes agent-`learned` (DB-resident) from
    /// `file`-registered skills; `pinned` marks the eviction/archival
    /// exemptions (file skills, user pins). `skills_fts` mirrors
    /// `memories_fts` — a standalone fts5(content) table fed the redacted
    /// name+description+content projection.
    fn ensure_skills_tables(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS skills (
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    uid             TEXT NOT NULL UNIQUE,
                    name            TEXT NOT NULL UNIQUE,
                    description     TEXT NOT NULL,
                    content         TEXT NOT NULL,
                    source          TEXT NOT NULL DEFAULT 'learned'
                                        CHECK(source IN ('learned','file')),
                    skill_path      TEXT,
                    status          TEXT NOT NULL DEFAULT 'active'
                                        CHECK(status IN ('active','tombstoned','superseded','archived')),
                    tier            TEXT NOT NULL DEFAULT 'hot'
                                        CHECK(tier IN ('hot','breadcrumb')),
                    pinned          INTEGER NOT NULL DEFAULT 0,
                    confidence      REAL NOT NULL DEFAULT 0.6,
                    salience        REAL NOT NULL DEFAULT 0.5,
                    created_at      TEXT NOT NULL,
                    updated_at      TEXT NOT NULL,
                    last_used_at    TEXT,
                    last_viewed_at  TEXT,
                    last_patched_at TEXT,
                    use_count       INTEGER NOT NULL DEFAULT 0,
                    view_count      INTEGER NOT NULL DEFAULT 0,
                    patch_count     INTEGER NOT NULL DEFAULT 0,
                    success_count   INTEGER NOT NULL DEFAULT 0,
                    failure_count   INTEGER NOT NULL DEFAULT 0,
                    last_success_at TEXT,
                    superseded_by   TEXT,
                    superseded_at   TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_skills_status ON skills(status);

                CREATE VIRTUAL TABLE IF NOT EXISTS skills_fts USING fts5(
                    content
                );
                ",
            )
            .map_err(|e| format!("Skills-table creation failed: {e}"))?;
        Ok(())
    }

    /// Delete all of a session's messages, keeping both FTS indexes
    /// consistent (dirge-slj2). This is the ONLY safe way to delete
    /// message rows: `messages_fts` is an external-content FTS5 table
    /// whose 'delete' command needs the exact indexed text, which is
    /// the redacted projection `insert_message` wrote — recomputed
    /// here from the raw row. Returns how many messages were deleted.
    ///
    /// No production caller yet — this exists so the first feature
    /// that deletes messages (session pruning, /clear --purge, …)
    /// has a path that doesn't corrupt the index (raw `DELETE FROM
    /// messages` leaves stale FTS entries).
    #[allow(dead_code)]
    pub fn delete_session_messages(&self, session_id: &str) -> Result<usize, String> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin delete transaction: {e}"))?;

        let rows: Vec<(i64, String, String, String)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT id, content, COALESCE(tool_name, ''), COALESCE(tool_calls, '')
                     FROM messages WHERE session_id = ?1",
                )
                .map_err(|e| format!("Failed to prepare delete scan: {e}"))?;
            stmt.query_map(params![session_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .map_err(|e| format!("Failed to scan session messages: {e}"))?
            .filter_map(|r| r.ok())
            .collect()
        };

        for (id, content, tool_name, tool_calls) in &rows {
            // Mirror insert_message's projection EXACTLY — same
            // format string, same redaction — or the FTS5 'delete'
            // corrupts the index instead of cleaning it.
            let combined = format!("{} {} {}", content, tool_name, tool_calls);
            let redacted = redact_for_fts(&combined);
            tx.execute(
                "INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', ?1, ?2)",
                params![id, redacted],
            )
            .map_err(|e| format!("Failed to remove FTS entry: {e}"))?;
            // Trigram is standalone — plain DML is safe (and the
            // messages_fts_trigram_delete trigger would cover it on
            // row delete anyway; explicit here for clarity).
            tx.execute(
                "DELETE FROM messages_fts_trigram WHERE rowid = ?1",
                params![id],
            )
            .map_err(|e| format!("Failed to remove trigram entry: {e}"))?;
        }

        tx.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(|e| format!("Failed to delete messages: {e}"))?;
        tx.execute(
            "UPDATE sessions SET message_count = 0 WHERE id = ?1",
            params![session_id],
        )
        .map_err(|e| format!("Failed to reset message count: {e}"))?;
        tx.commit()
            .map_err(|e| format!("Failed to commit delete: {e}"))?;
        Ok(rows.len())
    }

    pub fn insert_session(
        &self,
        id: &str,
        source: &str,
        model: &str,
        provider: &str,
        started_at: &str,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO sessions (id, source, model, provider, started_at, last_active)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![id, source, model, provider, started_at],
            )
            .map_err(|e| format!("Failed to insert session: {e}"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        tool_name: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        timestamp: &str,
    ) -> Result<i64, String> {
        self.conn
            .execute(
                "INSERT INTO messages (session_id, role, content, tool_name, tool_calls, tool_call_id, timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![session_id, role, content, tool_name, tool_calls, tool_call_id, timestamp],
            )
            .map_err(|e| format!("Failed to insert message: {e}"))?;

        let row_id = self.conn.last_insert_rowid();

        // SESS-14: redact secrets before they reach the FTS5 index.
        // The auto-insert triggers were dropped in v6 so we own this
        // path explicitly. Raw text stays in `messages` (so callers
        // re-reading a transcript see the original content); only
        // the searchable projection is scrubbed.
        let combined = format!(
            "{} {} {}",
            content,
            tool_name.unwrap_or(""),
            tool_calls.unwrap_or(""),
        );
        let redacted = redact_for_fts(&combined);

        self.conn
            .execute(
                "INSERT INTO messages_fts(rowid, content) VALUES (?1, ?2)",
                params![row_id, redacted],
            )
            .map_err(|e| format!("Failed to insert into messages_fts: {e}"))?;
        self.conn
            .execute(
                "INSERT INTO messages_fts_trigram(rowid, content) VALUES (?1, ?2)",
                params![row_id, redacted],
            )
            .map_err(|e| format!("Failed to insert into messages_fts_trigram: {e}"))?;

        self.conn
            .execute(
                "UPDATE sessions SET message_count = message_count + 1, last_active = ?1 WHERE id = ?2",
                params![timestamp, session_id],
            )
            .map_err(|e| format!("Failed to update session message count: {e}"))?;

        Ok(row_id)
    }
}

pub struct SearchResult {
    /// dirge-uzw4: the matched message's id, so the discovery window can
    /// anchor on the actual FTS hit instead of reconstructing (and missing)
    /// it from the session's first message.
    pub id: i64,
    pub session_id: String,
    pub content: String,
    #[allow(dead_code)] // populated from SQL, not yet read by consumers
    pub role: String,
    // dirge-uzw4: no longer used for anchoring (the FTS join now returns
    // the matched id), but kept in the result shape for callers/tests.
    #[allow(dead_code)]
    pub timestamp: String,
}

pub struct SessionSummary {
    pub id: String,
    pub source: String,
    pub model: String,
    pub title: String,
    pub started_at: String,
    pub last_active: String,
    pub message_count: i64,
}

impl SessionDb {
    pub fn list_sessions_rich(
        &self,
        exclude_sources: Option<&[&str]>,
    ) -> Result<Vec<SessionSummary>, String> {
        fn map_row(row: &rusqlite::Row) -> rusqlite::Result<SessionSummary> {
            Ok(SessionSummary {
                id: row.get(0)?,
                source: row.get(1)?,
                model: row.get(2)?,
                title: row.get(3)?,
                started_at: row.get(4)?,
                last_active: row.get(5)?,
                message_count: row.get(6)?,
            })
        }

        let (sql, has_exclude) = if exclude_sources.is_some_and(|s| !s.is_empty()) {
            let placeholders: Vec<String> = (0..exclude_sources.as_ref().unwrap().len())
                .map(|i| format!("?{}", i + 1))
                .collect();
            (
                format!(
                    "SELECT id, source, model, title, started_at, last_active, message_count
                     FROM sessions
                     WHERE source NOT IN ({})
                     ORDER BY last_active DESC
                     LIMIT 50",
                    placeholders.join(", ")
                ),
                true,
            )
        } else {
            (
                "SELECT id, source, model, title, started_at, last_active, message_count
                 FROM sessions
                 ORDER BY last_active DESC
                 LIMIT 50"
                    .to_string(),
                false,
            )
        };

        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| format!("Failed to prepare list sessions: {e}"))?;

        let results: Vec<SessionSummary> = if has_exclude {
            let sources = exclude_sources.unwrap();
            let refs: Vec<&dyn rusqlite::types::ToSql> = sources
                .iter()
                .map(|s| s as &dyn rusqlite::types::ToSql)
                .collect();
            stmt.query_map(rusqlite::params_from_iter(refs.iter()), map_row)
                .map_err(|e| format!("Failed to list sessions: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map([], map_row)
                .map_err(|e| format!("Failed to list sessions: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        };

        Ok(results)
    }

    /// dirge-9eyo: fetch one session's (source, model, title, started_at)
    /// by id directly. Discovery used to scan `list_sessions_rich(None)`
    /// (LIMIT 50 ORDER BY last_active DESC), so a hit in an older session
    /// silently got blank metadata. Errors if the session doesn't exist.
    pub fn get_session_meta(
        &self,
        session_id: &str,
    ) -> Result<(String, String, String, String), String> {
        self.conn
            .query_row(
                "SELECT source, model, title, started_at FROM sessions WHERE id = ?1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|_| format!("Session '{session_id}' not found"))
    }

    pub fn search_messages(
        &self,
        query: &str,
        role_filter: Option<&str>,
    ) -> Result<Vec<SearchResult>, String> {
        fn map_row(row: &rusqlite::Row) -> rusqlite::Result<SearchResult> {
            Ok(SearchResult {
                id: row.get(0)?,
                session_id: row.get(1)?,
                content: row.get(2)?,
                role: row.get(3)?,
                timestamp: row.get(4)?,
            })
        }

        let (sql, has_role) = if role_filter.is_some() {
            (
                "SELECT m.id, m.session_id, m.content, m.role, m.timestamp
                 FROM messages_fts f
                 JOIN messages m ON f.rowid = m.id
                 WHERE messages_fts MATCH ?1 AND m.role = ?2
                 ORDER BY rank
                 LIMIT 50",
                true,
            )
        } else {
            (
                "SELECT m.id, m.session_id, m.content, m.role, m.timestamp
                 FROM messages_fts f
                 JOIN messages m ON f.rowid = m.id
                 WHERE messages_fts MATCH ?1
                 ORDER BY rank
                 LIMIT 50",
                false,
            )
        };

        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| format!("Failed to prepare search: {e}"))?;

        let results: Vec<SearchResult> = if has_role {
            stmt.query_map(params![query, role_filter.unwrap()], map_row)
                .map_err(|e| format!("FTS5 search failed: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map(params![query], map_row)
                .map_err(|e| format!("FTS5 search failed: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        };

        Ok(results)
    }

    /// Search messages via the trigram FTS5 index (CJK/substring queries).
    /// The trigram tokenizer creates overlapping 3-character sequences,
    /// making substring matching work natively for any script.
    /// Port of Hermes's trigram search path (hermes_state.py:2245-2350).
    pub fn search_messages_trigram(
        &self,
        query: &str,
        role_filter: Option<&str>,
    ) -> Result<Vec<SearchResult>, String> {
        fn map_row(row: &rusqlite::Row) -> rusqlite::Result<SearchResult> {
            Ok(SearchResult {
                id: row.get(0)?,
                session_id: row.get(1)?,
                content: row.get(2)?,
                role: row.get(3)?,
                timestamp: row.get(4)?,
            })
        }

        let (sql, has_role) = if role_filter.is_some() {
            (
                "SELECT m.id, m.session_id, m.content, m.role, m.timestamp
                 FROM messages_fts_trigram f
                 JOIN messages m ON f.rowid = m.id
                 WHERE messages_fts_trigram MATCH ?1 AND m.role = ?2
                 ORDER BY rank
                 LIMIT 50",
                true,
            )
        } else {
            (
                "SELECT m.id, m.session_id, m.content, m.role, m.timestamp
                 FROM messages_fts_trigram f
                 JOIN messages m ON f.rowid = m.id
                 WHERE messages_fts_trigram MATCH ?1
                 ORDER BY rank
                 LIMIT 50",
                false,
            )
        };

        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| format!("Failed to prepare trigram search: {e}"))?;

        let results: Vec<SearchResult> = if has_role {
            stmt.query_map(params![query, role_filter.unwrap()], map_row)
                .map_err(|e| format!("Trigram FTS5 search failed: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map(params![query], map_row)
                .map_err(|e| format!("Trigram FTS5 search failed: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        };

        Ok(results)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_parent_session(&self, session_id: &str, parent_id: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE sessions SET parent_session_id = ?1 WHERE id = ?2",
                params![parent_id, session_id],
            )
            .map_err(|e| format!("Failed to set parent session: {e}"))?;
        Ok(())
    }

    /// Mark a session as ended with the given reason.
    /// No-ops when the session is already ended — the first end_reason
    /// wins (compression splits keep their end_reason).
    /// Port of Hermes's `end_session()` (hermes_state.py:732-748).
    ///
    /// Mark a session as ended with the given reason.
    /// No-ops when the session is already ended — the first end_reason
    /// wins (compression splits keep their end_reason).
    pub fn end_session(&self, session_id: &str, end_reason: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE sessions SET ended_at = ?1, end_reason = ?2 WHERE id = ?3 AND ended_at IS NULL",
                params![chrono::Utc::now().to_rfc3339(), end_reason, session_id],
            )
            .map_err(|e| format!("Failed to end session: {e}"))?;
        Ok(())
    }

    pub fn resolve_parent(&self, session_id: &str) -> Result<String, String> {
        let mut current = session_id.to_string();
        // Walk the parent chain up to root (max 100 hops to prevent
        // infinite loops on corrupted data).
        for _ in 0..100 {
            let parent: Option<String> = self
                .conn
                .query_row(
                    "SELECT parent_session_id FROM sessions WHERE id = ?1",
                    params![current],
                    |row| row.get(0),
                )
                .ok()
                .and_then(|p: Option<String>| p);
            match parent {
                Some(p) if !p.is_empty() => current = p,
                _ => break,
            }
        }
        Ok(current)
    }
}

/// A session's durable structured state (v10). `intent` is the
/// write-once drift anchor; `summary` is the latest regenerated body;
/// `revision` counts folds since insert.
// No production caller yet — the compaction/resume wiring lands in the
// next step (same staging as `delete_session_messages` above).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SessionCheckpoint {
    pub session_id: String,
    pub intent: String,
    pub summary: String,
    pub revision: i64,
}

#[allow(dead_code)] // production caller lands with the compaction wiring step
impl SessionDb {
    /// Write a session checkpoint. The FIRST call for a session inserts
    /// `intent` and `summary` at revision 0. Every later call replaces
    /// `summary` and bumps `revision` but LEAVES `intent` untouched — the
    /// `ON CONFLICT` clause deliberately omits it, so the original goal
    /// can't drift as the body is re-summarized fold after fold. Pass the
    /// best-known intent each time; it's only honored on insert.
    pub fn upsert_checkpoint(
        &self,
        session_id: &str,
        intent: &str,
        summary: &str,
    ) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO session_checkpoints
                     (session_id, intent, summary, revision, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 0, ?4, ?4)
                 ON CONFLICT(session_id) DO UPDATE SET
                     summary    = excluded.summary,
                     revision   = revision + 1,
                     updated_at = excluded.updated_at",
                params![session_id, intent, summary, now],
            )
            .map_err(|e| format!("Failed to upsert checkpoint: {e}"))?;
        Ok(())
    }

    /// Persist the durable checkpoint after a compaction fold. Keyed by
    /// the conversation's stable `origin_id` (see
    /// `Session::effective_origin`), which the fold handler carries
    /// forward unchanged across every rotation — so a resume that
    /// resolves any chain member to its origin finds it. `intent` is the
    /// verbatim first user prompt, honored only on the first write (the
    /// slot is write-once). An empty `summary` is a no-op: a prune-only
    /// pass produced no structured state worth storing.
    pub fn checkpoint_after_fold(&self, origin_id: &str, intent: &str, summary: &str) {
        if summary.is_empty() {
            return;
        }
        if let Err(e) = self.upsert_checkpoint(origin_id, intent, summary) {
            tracing::warn!(
                target: "dirge::session",
                error = %e,
                "failed to persist session checkpoint after fold",
            );
        }
    }

    /// Load a session's checkpoint, or `None` if it has none yet.
    pub fn get_checkpoint(&self, session_id: &str) -> Result<Option<SessionCheckpoint>, String> {
        self.conn
            .query_row(
                "SELECT session_id, intent, summary, revision
                 FROM session_checkpoints WHERE session_id = ?1",
                params![session_id],
                |row| {
                    Ok(SessionCheckpoint {
                        session_id: row.get(0)?,
                        intent: row.get(1)?,
                        summary: row.get(2)?,
                        revision: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|e| format!("Failed to read checkpoint: {e}"))
    }
}

pub struct AnchorView {
    pub messages: Vec<AnchorMessage>,
    pub anchor_index: usize,
    pub before: usize,
    pub after: usize,
}

pub struct AnchorMessage {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub timestamp: String,
}

impl SessionDb {
    pub fn get_anchored_view(
        &self,
        session_id: &str,
        anchor_message_id: i64,
        window: usize,
    ) -> Result<AnchorView, String> {
        // Get the anchor's position (row number within the session).
        let anchor_row: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND id <= ?2",
                params![session_id, anchor_message_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to find anchor position: {e}"))?;

        let total: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to count messages: {e}"))?;

        // dirge-uzw4: clamp at zero BEFORE the cast. A foreign/too-small
        // anchor id makes anchor_row 0, and `0i64 - 1` cast to usize wraps
        // to usize::MAX — which then makes `before`/`anchor_index` exceed
        // the returned window.
        let before = window.min((anchor_row - 1).max(0) as usize);
        let after = window.min((total - anchor_row).max(0) as usize);
        let offset = (anchor_row - before as i64 - 1).max(0);

        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, role, content, timestamp
                 FROM messages
                 WHERE session_id = ?1
                 ORDER BY id
                 LIMIT ?2 OFFSET ?3",
            )
            .map_err(|e| format!("Failed to prepare anchored view: {e}"))?;

        let messages: Vec<AnchorMessage> = stmt
            .query_map(params![session_id, before + 1 + after, offset], |row| {
                Ok(AnchorMessage {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    timestamp: row.get(3)?,
                })
            })
            .map_err(|e| format!("Failed to query anchored view: {e}"))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(AnchorView {
            messages,
            anchor_index: before,
            before,
            after,
        })
    }
}

#[cfg(test)]
#[path = "session_db_tests.rs"]
mod tests;
