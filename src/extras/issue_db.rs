//! Native issue tracker — a lightweight, agent-facing kanban stored in the
//! per-project session DB (`.dirge/sessions/state.db`).
//!
//! Issues are a stateful extension of the memory model and come in two buckets:
//! **active** (session-scoped, shown in the right-pane panel and nudged to finish)
//! and **passive backlog** (unassigned, `session_id IS NULL`, filed for later).
//! `issue create` files an issue to the passive backlog (optionally under an
//! epic parent via `epic_id`); `issue start` calls `assign_to_session` to claim
//! one onto the active queue. Closed issues fade like ordinary procedural
//! memory.
//!
//! The store deliberately owns its schema via idempotent `CREATE TABLE IF NOT
//! EXISTS` on open rather than a versioned migration — the session DB's
//! `user_version` chain is feature-gated (v14/v15 exist only under
//! `experimental-graph-search`), so threading a new linear version would either
//! collide with those or break the "enable the feature later" property. An
//! additive, self-owned table sidesteps all of it.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::sync_util::LockExt;

// Lifecycle states (open / in_progress / blocked / done / cancelled) and
// priorities (high / normal / low) are defined by the `normalize_*` vocabulary
// below.

/// Normalize a user/agent-supplied status to a canonical value, or `None` if
/// unrecognized. Accepts a few intuitive aliases. `pending` maps to `open` and
/// `cancelled` is a distinct terminal state — both come in via the bulk
/// `write_todo_list` vocabulary (pending / in_progress / completed / cancelled),
/// which shares this store.
pub fn normalize_status(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "open" | "todo" | "backlog" | "pending" => Some("open"),
        "in_progress" | "in-progress" | "started" | "doing" | "wip" => Some("in_progress"),
        "blocked" | "block" => Some("blocked"),
        "done" | "closed" | "complete" | "completed" | "finished" => Some("done"),
        "cancelled" | "canceled" | "wontfix" | "drop" | "dropped" => Some("cancelled"),
        _ => None,
    }
}

/// Whether a status is terminal (closed) — `done` or `cancelled`. Terminal
/// issues stamp `closed_at`, drop off the live board, and don't count toward
/// the unfinished-work nudge.
pub fn is_terminal_status(status: &str) -> bool {
    status == "done" || status == "cancelled"
}

/// Normalize a priority, or `None` if unrecognized.
pub fn normalize_priority(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "high" | "p0" | "p1" | "urgent" => Some("high"),
        "normal" | "medium" | "med" | "p2" | "" => Some("normal"),
        "low" | "p3" | "p4" | "minor" => Some("low"),
        _ => None,
    }
}

/// Normalize an issue title for matching: trim, lowercase, and collapse
/// internal ASCII whitespace runs to a single space. So `"  Build   AUTH "`
/// normalizes to `"build auth"`.
pub fn normalize_title(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if c.is_ascii_whitespace() {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// Parse an issue id from intuitive forms:
/// - `drg-a1b2` → `drg-a1b2`
/// - `#drg-a1b2` → `drg-a1b2`
/// - bare hex token `a1b2` → `drg-a1b2`
/// - legacy integer `7` / `#7` → `"7"`
/// - `iss-7` / `issue 7` → `"7"`
pub fn parse_issue_id(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let s = s.strip_prefix("issue").map(str::trim).unwrap_or(s);
    let s = s.strip_prefix('#').unwrap_or(s);
    let s = s.strip_prefix("iss-").unwrap_or(s);

    // Already a drg- prefixed id.
    if let Some(rest) = s.strip_prefix("drg-")
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Some(format!("drg-{rest}"));
    }

    // Legacy integer id.
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return Some(s.to_string());
    }

    // Bare short hex token → auto-prefix with drg-.
    let len = s.len();
    if (3..=16).contains(&len) && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(format!("drg-{s}"));
    }

    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub id: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub priority: String,
    pub session_id: Option<String>,
    pub epic_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

impl Issue {
    /// Intuitive one-line rendering for the board / lists: `drg-a1b2 [in_progress]
    /// (high) Build auth middleware`.
    pub fn one_line(&self) -> String {
        format!(
            "{} [{}] ({}) {}",
            self.id, self.status, self.priority, self.title
        )
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Resolve a raw session id to its stable lineage origin via the shared
/// `sessions` table in the same `state.db`. A compaction fold rotates
/// `sessions.id` but keeps a constant `origin_id` (see `Session::effective_origin`
/// / `session_db::resolve_parent`), so scoping the issue board by origin keeps
/// the TODOS panel and the finish-your-work nudge populated across a fold or a
/// resume instead of emptying when the id rotates.
///
/// Falls back to the id unchanged when the `sessions` table or row is absent —
/// a bare issue DB (unit-test fixtures), a `--no-session` run, or a session
/// predating the `origin_id` column — which degrades to the old exact-id
/// scoping. `conn` is borrowed (via deref) from a lock the caller already
/// holds, so this never re-locks.
fn origin_of(conn: &Connection, session_id: &str) -> String {
    conn.query_row(
        "SELECT COALESCE(origin_id, id) FROM sessions WHERE id = ?1",
        params![session_id],
        |r| r.get::<_, String>(0),
    )
    .unwrap_or_else(|_| session_id.to_string())
}

fn row_to_issue(row: &rusqlite::Row<'_>) -> rusqlite::Result<Issue> {
    Ok(Issue {
        id: row.get(0)?,
        title: row.get(1)?,
        body: row.get(2)?,
        status: row.get(3)?,
        priority: row.get(4)?,
        session_id: row.get(5)?,
        epic_id: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        closed_at: row.get(9)?,
    })
}

const COLS: &str =
    "id, title, body, status, priority, session_id, epic_id, created_at, updated_at, closed_at";

/// SQLite-backed issue tracker over the per-project session DB. Mirrors
/// [`super::spec_db::SpecStore`] — holds its own connection.
pub struct IssueStore {
    conn: Mutex<Connection>,
}

impl IssueStore {
    /// Open against a project's session DB, creating the `issues` table if
    /// needed.
    pub fn open(paths: &super::dirge_paths::ProjectPaths) -> Result<Self, String> {
        Self::open_at(&paths.session_db_path())
    }

    /// Open against an explicit DB path (used by tests and callers that
    /// already resolved the path).
    ///
    /// Opens a plain connection rather than going through `SessionDb::open`:
    /// the issues table is self-owned (idempotent `ensure_schema`), so it does
    /// NOT need the session DB's versioned `migrate()` — and crucially avoids
    /// the `BEGIN EXCLUSIVE` migration lock that `migrate()` takes on every
    /// open, which the harness would otherwise pay on every turn + tool call
    /// on the shared `state.db`. A busy timeout lets a concurrent writer (the
    /// session-persistence / memory connections share this file) yield a short
    /// wait instead of an immediate `SQLITE_BUSY`.
    pub fn open_at(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|e| format!("open issue db at {}: {e}", path.display()))?;
        let _ = conn.busy_timeout(Duration::from_secs(5));
        // WAL is a persistent property of the file once any opener sets it
        // (session persistence does); setting it here is a harmless no-op then,
        // and the right default if issues happen to open the file first.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.ensure_schema()?;
        Ok(store)
    }

    fn ensure_schema(&self) -> Result<(), String> {
        let conn = self.conn.lock_ignore_poison();

        // Create the table if it doesn't exist yet (fresh DB).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS issues (
                id          TEXT PRIMARY KEY,
                title       TEXT NOT NULL,
                body        TEXT NOT NULL DEFAULT '',
                status      TEXT NOT NULL DEFAULT 'open',
                priority    TEXT NOT NULL DEFAULT 'normal',
                session_id  TEXT,
                epic_id     TEXT,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                closed_at   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status);",
        )
        .map_err(|e| format!("ensure issues schema: {e}"))?;

        // Detect legacy schema (INTEGER AUTOINCREMENT id OR missing epic_id
        // column) and migrate in one transaction.
        if is_legacy_schema(&conn) {
            conn.execute_batch(
                "BEGIN;
                 CREATE TABLE issues_new (
                     id          TEXT PRIMARY KEY,
                     title       TEXT NOT NULL,
                     body        TEXT NOT NULL DEFAULT '',
                     status      TEXT NOT NULL DEFAULT 'open',
                     priority    TEXT NOT NULL DEFAULT 'normal',
                     session_id  TEXT,
                     epic_id     TEXT,
                     created_at  TEXT NOT NULL,
                     updated_at  TEXT NOT NULL,
                     closed_at   TEXT
                 );
                 INSERT INTO issues_new (id, title, body, status, priority, session_id, epic_id, created_at, updated_at, closed_at)
                     SELECT CAST(id AS TEXT), title, body, status, priority, session_id, NULL, created_at, updated_at, closed_at
                     FROM issues;
                 DROP TABLE issues;
                 ALTER TABLE issues_new RENAME TO issues;
                 CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status);
                 COMMIT;",
            )
            .map_err(|e| format!("migrate legacy issues schema: {e}"))?;
        }

        Ok(())
    }

    /// Create a new issue. Returns the generated id string (e.g. `drg-a1b2`).
    /// `priority`, `session_id`, and `epic` are optional; an unknown priority
    /// falls back to `normal`.
    pub fn create(
        &self,
        title: &str,
        body: &str,
        priority: Option<&str>,
        session_id: Option<&str>,
        epic: Option<&str>,
    ) -> Result<String, String> {
        let title = title.trim();
        if title.is_empty() {
            return Err("issue title must not be empty".to_string());
        }
        let priority = priority.and_then(normalize_priority).unwrap_or("normal");
        let epic_normalized = epic.and_then(parse_issue_id);
        let conn = self.conn.lock_ignore_poison();
        let now = now();
        // Stamp the lineage origin, not the rotating id, so the row stays on
        // this conversation's board across a later compaction fold.
        let scoped = session_id.map(|s| origin_of(&conn, s));

        let id = generate_id(&conn)?;
        conn.execute(
            "INSERT INTO issues (id, title, body, status, priority, session_id, epic_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'open', ?4, ?5, ?6, ?7, ?7)",
            params![id, title, body.trim(), priority, scoped, epic_normalized, now],
        )
        .map_err(|e| format!("create issue: {e}"))?;
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Result<Option<Issue>, String> {
        let conn = self.conn.lock_ignore_poison();
        conn.query_row(
            &format!("SELECT {COLS} FROM issues WHERE id = ?1"),
            params![id],
            row_to_issue,
        )
        .optional()
        .map_err(|e| format!("get issue: {e}"))
    }

    /// Update status (validated). Setting a terminal status (`done` /
    /// `cancelled`) stamps `closed_at`; moving off one clears it. Returns false
    /// if the issue doesn't exist.
    pub fn set_status(&self, id: &str, status: &str) -> Result<bool, String> {
        let status = normalize_status(status).ok_or_else(|| {
            format!("unknown status '{status}' (use open|in_progress|blocked|done|cancelled)")
        })?;
        let conn = self.conn.lock_ignore_poison();
        let now = now();
        let n = if is_terminal_status(status) {
            conn.execute(
                "UPDATE issues SET status = ?2, updated_at = ?3, closed_at = ?3 WHERE id = ?1",
                params![id, status, now],
            )
        } else {
            conn.execute(
                "UPDATE issues SET status = ?2, updated_at = ?3, closed_at = NULL WHERE id = ?1",
                params![id, status, now],
            )
        }
        .map_err(|e| format!("set status: {e}"))?;
        Ok(n > 0)
    }

    /// Re-assign an issue to `session_id`'s lineage (normalized to the stable
    /// origin) so picking up an issue created in another conversation pulls it
    /// onto the current session's board / TODOS panel. `session_id = None` is a
    /// no-op — a runner without a session (e.g. `--no-session`) must not blank
    /// an existing scope. Returns false if the issue doesn't exist.
    pub fn assign_to_session(&self, id: &str, session_id: Option<&str>) -> Result<bool, String> {
        let Some(session_id) = session_id else {
            return Ok(false);
        };
        let conn = self.conn.lock_ignore_poison();
        let scoped = origin_of(&conn, session_id);
        let n = conn
            .execute(
                "UPDATE issues SET session_id = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, scoped, now()],
            )
            .map_err(|e| format!("assign session: {e}"))?;
        Ok(n > 0)
    }

    pub fn set_priority(&self, id: &str, priority: &str) -> Result<bool, String> {
        let priority = normalize_priority(priority)
            .ok_or_else(|| format!("unknown priority '{priority}' (use high|normal|low)"))?;
        let conn = self.conn.lock_ignore_poison();
        let n = conn
            .execute(
                "UPDATE issues SET priority = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, priority, now()],
            )
            .map_err(|e| format!("set priority: {e}"))?;
        Ok(n > 0)
    }

    /// All issues with the given status, newest first.
    pub fn list_by_status(&self, status: &str) -> Result<Vec<Issue>, String> {
        let status =
            normalize_status(status).ok_or_else(|| format!("unknown status '{status}'"))?;
        let conn = self.conn.lock_ignore_poison();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLS} FROM issues WHERE status = ?1 ORDER BY updated_at DESC"
            ))
            .map_err(|e| format!("list: {e}"))?;
        let rows = stmt
            .query_map(params![status], row_to_issue)
            .map_err(|e| format!("list: {e}"))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| format!("list: {e}"))
    }

    /// The live board: open / in_progress / blocked issues ordered the way the
    /// harness surfaces them — in_progress first, then blocked, then open;
    /// within a state, high priority first, then most-recently-touched.
    /// `limit` caps the rows (the caller bounds context); `None` = all.
    pub fn board(&self, limit: Option<usize>) -> Result<Vec<Issue>, String> {
        self.board_query(None, limit)
    }

    /// The live board scoped to one session: open / in_progress / blocked
    /// issues created or last touched under `session_id`, ordered like
    /// [`Self::board`]. This is what the right-pane panel and the
    /// finish-your-work nudge read — both are session-scoped, unlike the
    /// project-wide turn-start reminder. `session_id = None` matches issues
    /// with a NULL session.
    pub fn board_for_session(
        &self,
        session_id: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<Issue>, String> {
        self.board_query(Some(session_id), limit)
    }

    /// Shared live-board query for [`Self::board`] (project-wide) and
    /// [`Self::board_for_session`] (session-scoped), so the column list and the
    /// status/priority ordering can't drift between them. `session = None` means
    /// no session filter; `session = Some(sid)` filters `session_id IS sid`
    /// (where `sid = None` matches a NULL session). A trailing `id DESC` makes
    /// the order total — a single `sync_todos` batch stamps every row with the
    /// same `updated_at`, so without it the within-bucket order would be
    /// nondeterministic.
    fn board_query(
        &self,
        session: Option<Option<&str>>,
        limit: Option<usize>,
    ) -> Result<Vec<Issue>, String> {
        let conn = self.conn.lock_ignore_poison();
        // Normalize the session filter to the lineage origin so the scoped board
        // survives a compaction fold / resume (which rotates `session.id`). The
        // inner `None` (an explicit NULL-session filter) is preserved as-is.
        let scoped: Option<Option<String>> =
            session.map(|inner| inner.map(|sid| origin_of(&conn, sid)));
        let where_session = if scoped.is_some() {
            "session_id IS ?1 AND "
        } else {
            ""
        };
        let sql = format!(
            "SELECT {COLS} FROM issues
             WHERE {where_session}status IN ('open','in_progress','blocked')
             ORDER BY
               CASE status WHEN 'in_progress' THEN 0 WHEN 'blocked' THEN 1 ELSE 2 END,
               CASE priority WHEN 'high' THEN 0 WHEN 'normal' THEN 1 ELSE 2 END,
               updated_at DESC, id DESC
             {}",
            match limit {
                Some(n) => format!("LIMIT {n}"),
                None => String::new(),
            }
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| format!("board: {e}"))?;
        let rows = match &scoped {
            Some(sid) => stmt.query_map(params![sid], row_to_issue),
            None => stmt.query_map([], row_to_issue),
        }
        .map_err(|e| format!("board: {e}"))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| format!("board: {e}"))
    }

    /// Bulk reconcile a `write_todo_list`-style plan into this session's issues,
    /// in one transaction. Each `(title, status, priority)` triple is upserted
    /// by `(session_id, title)`: a matching issue is updated (status + priority,
    /// stamping/clearing `closed_at` as the status crosses the terminal line),
    /// and a missing one is created. Empty titles are skipped.
    ///
    /// Deliberately NOT a full replace: issues absent from the list are left
    /// untouched rather than closed, so a partial plan can't silently wipe work
    /// the model (or the `issue` tool) is still tracking. The model closes an
    /// item by restating it with status `completed` / `cancelled`. Returns the
    /// number of items applied.
    pub fn sync_todos(
        &self,
        session_id: Option<&str>,
        items: &[(&str, &str, &str)],
    ) -> Result<usize, String> {
        let mut conn = self.conn.lock_ignore_poison();
        // Scope the whole batch to the lineage origin (see `origin_of`) so the
        // upsert key and the stored rows match the board query across a fold.
        let scoped_session: Option<String> = session_id.map(|s| origin_of(&conn, s));
        let scoped_session = scoped_session.as_deref();
        let tx = conn
            .transaction()
            .map_err(|e| format!("sync todos (begin): {e}"))?;

        // Build a map of normalized-title → id from all existing rows in this
        // session so a reworded restatement still hits the right row. Later ids
        // overwrite earlier, matching today's `ORDER BY id DESC LIMIT 1`.
        let mut id_by_norm: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        {
            let mut stmt = tx
                .prepare("SELECT id, title FROM issues WHERE session_id IS ?1 ORDER BY id ASC")
                .map_err(|e| format!("sync todos (prefetch): {e}"))?;
            let rows = stmt
                .query_map(params![scoped_session], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| format!("sync todos (prefetch): {e}"))?;
            for row in rows {
                let (rid, rtitle) = row.map_err(|e| format!("sync todos (prefetch): {e}"))?;
                id_by_norm.insert(normalize_title(&rtitle), rid);
            }
        }

        let now = now();
        let mut applied = 0usize;
        for (title, status, priority) in items {
            let title = title.trim();
            if title.is_empty() {
                continue;
            }
            let status = normalize_status(status).unwrap_or("open");
            let priority = normalize_priority(priority).unwrap_or("normal");
            let closed: Option<&str> = is_terminal_status(status).then_some(now.as_str());
            let norm = normalize_title(title);
            let existing = id_by_norm.get(&norm);
            match existing {
                Some(id) => {
                    tx.execute(
                        "UPDATE issues SET status = ?2, priority = ?3, updated_at = ?4, closed_at = ?5 WHERE id = ?1",
                        params![id, status, priority, now, closed],
                    )
                    .map_err(|e| format!("sync todos (update): {e}"))?;
                }
                None => {
                    let new_id = generate_id(&tx)?;
                    tx.execute(
                        "INSERT INTO issues (id, title, body, status, priority, session_id, created_at, updated_at, closed_at)
                         VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?6, ?7)",
                        params![new_id, title, status, priority, scoped_session, now, closed],
                    )
                    .map_err(|e| format!("sync todos (insert): {e}"))?;
                    // Insert into map so a within-batch duplicate matches too.
                    id_by_norm.insert(norm, new_id);
                }
            }
            applied += 1;
        }
        tx.commit()
            .map_err(|e| format!("sync todos (commit): {e}"))?;
        Ok(applied)
    }

    /// The passive backlog: open / in_progress / blocked issues with
    /// `session_id IS NULL`, i.e. unassigned issues filed for later. Terminal
    /// issues are excluded. Ordered high-priority first, then most-recently
    /// touched, then id DESC for total ordering.
    pub fn backlog(&self, limit: Option<usize>) -> Result<Vec<Issue>, String> {
        let conn = self.conn.lock_ignore_poison();
        let sql = format!(
            "SELECT {COLS} FROM issues
             WHERE session_id IS NULL AND status IN ('open','in_progress','blocked')
             ORDER BY
               CASE priority WHEN 'high' THEN 0 WHEN 'normal' THEN 1 ELSE 2 END,
               updated_at DESC, id DESC
             {}",
            match limit {
                Some(n) => format!("LIMIT {n}"),
                None => String::new(),
            }
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| format!("backlog: {e}"))?;
        let rows = stmt
            .query_map([], row_to_issue)
            .map_err(|e| format!("backlog: {e}"))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| format!("backlog: {e}"))
    }

    /// All live children of an epic (the issue whose `epic_id` equals `epic_id`).
    /// Terminal (`done` / `cancelled`) children are excluded. Ordered
    /// in_progress first, then blocked, then open; high-priority first within
    /// each status; then updated_at DESC, id DESC.
    pub fn children_of(&self, epic_id: &str) -> Result<Vec<Issue>, String> {
        let conn = self.conn.lock_ignore_poison();
        let sql = format!(
            "SELECT {COLS} FROM issues
             WHERE epic_id = ?1 AND status IN ('open','in_progress','blocked')
             ORDER BY
               CASE status WHEN 'in_progress' THEN 0 WHEN 'blocked' THEN 1 ELSE 2 END,
               CASE priority WHEN 'high' THEN 0 WHEN 'normal' THEN 1 ELSE 2 END,
               updated_at DESC, id DESC"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("children_of: {e}"))?;
        let rows = stmt
            .query_map(params![epic_id], row_to_issue)
            .map_err(|e| format!("children_of: {e}"))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| format!("children_of: {e}"))
    }

    /// Build the harness's turn-start board reminder as two labeled sections:
    /// "Active work queue" (session-scoped issues to finish) and "Backlog"
    /// (unassigned issues filed for later). Each section is capped independently
    /// (`active_top_n` / `backlog_top_n`) with an overflow hint. A section is
    /// omitted entirely when empty. Returns `None` when BOTH are empty.
    pub fn board_reminder_split(
        &self,
        session_id: Option<&str>,
        active_top_n: usize,
        backlog_top_n: usize,
    ) -> Result<Option<String>, String> {
        let active = self.board_for_session(session_id, Some(active_top_n))?;
        let backlog = self.backlog(Some(backlog_top_n))?;

        if active.is_empty() && backlog.is_empty() {
            return Ok(None);
        }

        let mut s = String::from("<system-reminder>\n");

        // Active work queue section.
        if !active.is_empty() {
            let active_total = self.board_for_session(session_id, None)?.len();
            let in_progress = active.iter().find(|i| i.status == "in_progress");
            s.push_str(
                "Active work queue — your current tasks (keep one item in_progress; mark it completed the moment it's done):\n",
            );
            if let Some(current) = in_progress {
                s.push_str(&format!("Currently in progress: {}\n", current.title));
            }
            for i in &active {
                s.push_str(&format!("- {}\n", i.one_line()));
            }
            let shown = active.len();
            if active_total > shown {
                s.push_str(&format!("… and {} more active.\n", active_total - shown));
            }
        }

        // Backlog section.
        if !backlog.is_empty() {
            let backlog_total = self.backlog(None)?.len();
            s.push_str(
                "\nBacklog (issues filed for later — start one with the `issue` tool to pick it up; not worked automatically):\n",
            );
            for i in &backlog {
                s.push_str(&format!("- {}\n", i.one_line()));
            }
            let shown = backlog.len();
            if backlog_total > shown {
                s.push_str(&format!(
                    "… and {} more in the backlog. Use /issues to see all.\n",
                    backlog_total - shown
                ));
            }
        }

        s.push_str("</system-reminder>");
        Ok(Some(s))
    }

    /// Substring search over title + body (case-insensitive), newest first.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Issue>, String> {
        let conn = self.conn.lock_ignore_poison();
        // Escape LIKE metacharacters (`%`, `_`, and the escape char `\`) so a
        // query like `100%` is a literal substring, not "match anything".
        let like = format!("%{}%", escape_like(query.trim()));
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLS} FROM issues
                 WHERE title LIKE ?1 ESCAPE '\\' COLLATE NOCASE
                    OR body  LIKE ?1 ESCAPE '\\' COLLATE NOCASE
                 ORDER BY updated_at DESC LIMIT ?2"
            ))
            .map_err(|e| format!("search: {e}"))?;
        let rows = stmt
            .query_map(params![like, limit as i64], row_to_issue)
            .map_err(|e| format!("search: {e}"))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| format!("search: {e}"))
    }
}

/// Escape SQL LIKE metacharacters for use with a `\` ESCAPE clause: the
/// escape char first, then `%` and `_`, so they match literally.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Detect whether the current `issues` table uses the legacy INTEGER
/// AUTOINCREMENT id schema (or is missing the `epic_id` column).
fn is_legacy_schema(conn: &Connection) -> bool {
    // Check if the id column is INTEGER type (legacy) or TEXT (current).
    let id_type: Option<String> = conn
        .query_row(
            "SELECT type FROM pragma_table_info('issues') WHERE name = 'id'",
            [],
            |r| r.get(0),
        )
        .ok();
    let has_epic_id: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('issues') WHERE name = 'epic_id'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);

    if !has_epic_id {
        return true;
    }
    if let Some(t) = id_type
        && t.to_ascii_uppercase().contains("INT")
    {
        return true;
    }
    // If the table doesn't exist yet, it's not legacy.
    false
}

/// Generate a beads-style id: `drg-` + 4 lowercase hex chars from a UUID.
/// Retries on collision, widening to 8 hex chars after a few attempts.
fn generate_id(conn: &Connection) -> Result<String, String> {
    let mut hex_len = 4;
    for attempt in 0..20 {
        let raw = uuid::Uuid::new_v4().simple().to_string();
        let hex = if raw.len() > hex_len {
            &raw[..hex_len]
        } else {
            &raw
        };
        let id = format!("drg-{hex}");
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM issues WHERE id = ?1",
                params![id],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        if !exists {
            return Ok(id);
        }
        if attempt >= 4 {
            hex_len = 8;
        }
    }
    Err("failed to generate a unique issue id after 20 attempts".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> IssueStore {
        store_at().0
    }

    /// Like [`store`] but also returns the DB path, so a test can seed the
    /// shared `sessions` table (see [`seed_lineage`]) to exercise origin
    /// normalization.
    fn store_at() -> (IssueStore, std::path::PathBuf) {
        // A process-wide counter guarantees a unique dir even when two tests
        // enter store() within the same clock nanosecond — the SystemTime
        // suffix alone let parallel tests collide on one DB and pollute each
        // other (the source of the flaky board_* failures).
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dirge-issue-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let store = IssueStore::open_at(&path).unwrap();
        (store, path)
    }

    /// Seed a minimal `sessions` table with a folded lineage: `tip` is a
    /// post-fold id whose stable origin is `origin` (the origin row carries a
    /// NULL `origin_id`, matching how `session_db` stamps a never-folded head).
    fn seed_lineage(path: &Path, tip: &str, origin: &str) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, origin_id TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO sessions (id, origin_id) VALUES (?1, NULL)",
            params![origin],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO sessions (id, origin_id) VALUES (?1, ?2)",
            params![tip, origin],
        )
        .unwrap();
    }

    /// Open a DB, drop issues, and create the legacy (integer AUTOINCREMENT
    /// id, no epic_id) schema manually so migration tests have a known shape.
    fn seed_legacy_schema(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "DROP TABLE IF EXISTS issues;
             CREATE TABLE issues (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 title       TEXT NOT NULL,
                 body        TEXT NOT NULL DEFAULT '',
                 status      TEXT NOT NULL DEFAULT 'open',
                 priority    TEXT NOT NULL DEFAULT 'normal',
                 session_id  TEXT,
                 created_at  TEXT NOT NULL,
                 updated_at  TEXT NOT NULL,
                 closed_at   TEXT
             );",
        )
        .unwrap();
    }

    // ── parse_issue_id ───────────────────────────────────────────────────

    #[test]
    fn parse_issue_id_drg_prefixed() {
        assert_eq!(parse_issue_id("drg-a1b2"), Some("drg-a1b2".to_string()));
        assert_eq!(parse_issue_id("#drg-a1b2"), Some("drg-a1b2".to_string()));
    }

    #[test]
    fn parse_issue_id_bare_hex_token() {
        assert_eq!(parse_issue_id("a1b2"), Some("drg-a1b2".to_string()));
        assert_eq!(parse_issue_id("abc123"), Some("drg-abc123".to_string()));
    }

    #[test]
    fn parse_issue_id_legacy_integer() {
        assert_eq!(parse_issue_id("7"), Some("7".to_string()));
        assert_eq!(parse_issue_id("#7"), Some("7".to_string()));
        assert_eq!(parse_issue_id("iss-7"), Some("7".to_string()));
        assert_eq!(parse_issue_id("issue 7"), Some("7".to_string()));
    }

    #[test]
    fn parse_issue_id_invalid() {
        assert_eq!(parse_issue_id("nope"), None);
        assert_eq!(parse_issue_id(""), None);
        assert_eq!(parse_issue_id("drg-"), None);
        // Too short for bare hex (need >= 3 chars)
        assert_eq!(parse_issue_id("ab"), None);
    }

    // ── normalize_title ───────────────────────────────────────────────────

    #[test]
    fn normalize_title_trims_and_collapses_whitespace() {
        assert_eq!(normalize_title("  Build   AUTH "), "build auth");
        assert_eq!(normalize_title("simple"), "simple");
        assert_eq!(normalize_title(""), "");
        assert_eq!(normalize_title("  "), "");
        assert_eq!(
            normalize_title("camelCase\tTitle\nHere"),
            "camelcase title here"
        );
    }

    // ── migration ────────────────────────────────────────────────────────

    #[test]
    fn legacy_schema_migration_preserves_rows_and_adds_epic_id() {
        let (_store, path) = store_at();
        drop(_store);

        // Seed a legacy-shaped table with two rows.
        seed_legacy_schema(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO issues (id, title, body, status, priority, session_id, created_at, updated_at)
             VALUES (1, 'first', '', 'open', 'high', 'sess-1', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO issues (id, title, body, status, priority, session_id, created_at, updated_at)
             VALUES (2, 'second', 'some body', 'in_progress', 'normal', NULL, '2024-01-02T00:00:00Z', '2024-01-02T00:00:00Z')",
            [],
        )
        .unwrap();
        drop(conn);

        // Open the store — ensure_schema should migrate.
        let s = IssueStore::open_at(&path).unwrap();

        let first = s.get("1").unwrap().expect("row 1 survives");
        assert_eq!(first.title, "first");
        assert_eq!(first.priority, "high");
        assert_eq!(first.epic_id, None);

        let second = s.get("2").unwrap().expect("row 2 survives");
        assert_eq!(second.title, "second");
        assert_eq!(second.body, "some body");
        assert_eq!(second.status, "in_progress");
        assert_eq!(second.epic_id, None);

        // New issues get drg- prefixed ids.
        let new_id = s.create("new issue", "", None, None, None).unwrap();
        assert!(
            new_id.starts_with("drg-"),
            "new id should be drg- prefixed, got: {new_id}"
        );
        let new_issue = s.get(&new_id).unwrap().unwrap();
        assert_eq!(new_issue.title, "new issue");
        assert_eq!(new_issue.epic_id, None);

        // Migration is idempotent — reopening doesn't break.
        let s2 = IssueStore::open_at(&path).unwrap();
        assert!(s2.get("1").unwrap().is_some());
        assert!(s2.get(&new_id).unwrap().is_some());
    }

    // ── board scoping ────────────────────────────────────────────────────

    #[test]
    fn board_scopes_by_lineage_origin_across_fold() {
        let (s, path) = store_at();
        seed_lineage(&path, "tip", "orig");
        // Created pre-fold under the original id, then started.
        let id = s
            .create("keep me", "", Some("high"), Some("orig"), None)
            .unwrap();
        s.set_status(&id, "in_progress").unwrap();
        // A compaction fold rotates the session id to "tip"; the board queried
        // under the new id still finds the issue because both normalize to the
        // shared origin.
        let board = s.board_for_session(Some("tip"), None).unwrap();
        assert_eq!(board.len(), 1, "issue must survive the id rotation");
        assert_eq!(board[0].title, "keep me");
        // The stored scope was normalized to the origin, not the raw id.
        assert_eq!(
            s.get(&id).unwrap().unwrap().session_id.as_deref(),
            Some("orig")
        );
    }

    #[test]
    fn assign_to_session_claims_foreign_issue_onto_board() {
        // No sessions table here → origin_of falls back to identity, exercising
        // the graceful degradation path too.
        let s = store();
        let id = s
            .create("from elsewhere", "", None, Some("other-sess"), None)
            .unwrap();
        s.set_status(&id, "in_progress").unwrap();
        // Not on our board yet — it belongs to another conversation.
        assert!(
            s.board_for_session(Some("mine"), None).unwrap().is_empty(),
            "foreign issue must not appear before pickup"
        );
        // Picking it up claims it for us.
        assert!(s.assign_to_session(&id, Some("mine")).unwrap());
        let board = s.board_for_session(Some("mine"), None).unwrap();
        assert_eq!(board.len(), 1);
        assert_eq!(board[0].title, "from elsewhere");
        // A session-less runner must not blank the scope.
        assert!(!s.assign_to_session(&id, None).unwrap());
        assert_eq!(
            s.get(&id).unwrap().unwrap().session_id.as_deref(),
            Some("mine")
        );
    }

    // ── create / get ─────────────────────────────────────────────────────

    #[test]
    fn create_and_get_roundtrip() {
        let s = store();
        let id = s
            .create("Build auth", "use PKCE", Some("high"), Some("sess-1"), None)
            .unwrap();
        assert!(id.starts_with("drg-"), "id should be drg- prefixed: {id}");
        let issue = s.get(&id).unwrap().expect("issue exists");
        assert_eq!(issue.title, "Build auth");
        assert_eq!(issue.body, "use PKCE");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.priority, "high");
        assert_eq!(issue.session_id.as_deref(), Some("sess-1"));
        assert!(issue.closed_at.is_none());
        assert_eq!(issue.epic_id, None);
    }

    #[test]
    fn create_with_epic_parent() {
        let s = store();
        let parent_id = s.create("epic parent", "", None, None, None).unwrap();
        let child_id = s
            .create("child task", "", None, None, Some(&parent_id))
            .unwrap();
        let child = s.get(&child_id).unwrap().unwrap();
        assert_eq!(child.epic_id.as_deref(), Some(parent_id.as_str()));
    }

    #[test]
    fn create_with_epic_parsed_from_drg_form() {
        let s = store();
        let parent_id = s.create("epic", "", None, None, None).unwrap();
        // Pass via drg- prefixed form
        let child_id = s
            .create(
                "child",
                "",
                None,
                None,
                Some(&format!("drg-{}", &parent_id[4..])),
            )
            .unwrap();
        let child = s.get(&child_id).unwrap().unwrap();
        assert_eq!(child.epic_id.as_deref(), Some(parent_id.as_str()));
    }

    #[test]
    fn children_of_returns_live_children_excludes_terminal() {
        let s = store();
        let epic_id = s.create("epic", "", None, None, None).unwrap();
        let c1 = s
            .create("child 1", "", Some("high"), None, Some(&epic_id))
            .unwrap();
        let c2 = s.create("child 2", "", None, None, Some(&epic_id)).unwrap();
        // Terminal child excluded.
        let c_done = s
            .create("child done", "", None, None, Some(&epic_id))
            .unwrap();
        s.set_status(&c_done, "done").unwrap();
        // Non-child issue excluded.
        s.create("unrelated", "", None, None, None).unwrap();

        let kids = s.children_of(&epic_id).unwrap();
        let ids: Vec<&str> = kids.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec![c1.as_str(), c2.as_str()]);
        assert_eq!(kids[0].title, "child 1");
        assert_eq!(kids[1].title, "child 2");
    }

    #[test]
    fn children_of_empty_for_non_epic_id() {
        let s = store();
        assert!(s.children_of("drg-ffff").unwrap().is_empty());
    }

    #[test]
    fn empty_title_rejected() {
        let s = store();
        assert!(s.create("   ", "", None, None, None).is_err());
    }

    #[test]
    fn unknown_priority_falls_back_to_normal() {
        let s = store();
        let id = s
            .create("x", "", Some("supercritical"), None, None)
            .unwrap();
        assert_eq!(s.get(&id).unwrap().unwrap().priority, "normal");
    }

    // ── status ───────────────────────────────────────────────────────────

    #[test]
    fn set_status_done_stamps_closed_at_and_clears_on_reopen() {
        let s = store();
        let id = s.create("x", "", None, None, None).unwrap();
        assert!(s.set_status(&id, "done").unwrap());
        let done = s.get(&id).unwrap().unwrap();
        assert_eq!(done.status, "done");
        assert!(done.closed_at.is_some());
        // reopen clears closed_at
        assert!(s.set_status(&id, "in_progress").unwrap());
        let reopened = s.get(&id).unwrap().unwrap();
        assert_eq!(reopened.status, "in_progress");
        assert!(reopened.closed_at.is_none());
    }

    #[test]
    fn set_status_aliases_normalize() {
        let s = store();
        let id = s.create("x", "", None, None, None).unwrap();
        assert!(s.set_status(&id, "WIP").unwrap());
        assert_eq!(s.get(&id).unwrap().unwrap().status, "in_progress");
    }

    #[test]
    fn set_status_unknown_rejected_and_missing_id_is_false() {
        let s = store();
        let id = s.create("x", "", None, None, None).unwrap();
        assert!(s.set_status(&id, "nonsense").is_err());
        assert!(!s.set_status("drg-ffff", "done").unwrap());
    }

    // ── board ────────────────────────────────────────────────────────────

    #[test]
    fn board_orders_in_progress_then_priority_and_excludes_done() {
        let s = store();
        let _low_open = s.create("low open", "", Some("low"), None, None).unwrap();
        let high_open = s.create("high open", "", Some("high"), None, None).unwrap();
        let wip = s.create("wip", "", Some("low"), None, None).unwrap();
        s.set_status(&wip, "in_progress").unwrap();
        let done = s.create("done", "", Some("high"), None, None).unwrap();
        s.set_status(&done, "done").unwrap();

        let board = s.board(None).unwrap();
        let ids: Vec<&str> = board.iter().map(|i| i.id.as_str()).collect();
        // in_progress first regardless of priority, then high open, then low open.
        assert_eq!(
            ids,
            vec![wip.as_str(), high_open.as_str(), _low_open.as_str()]
        );
        // done excluded
        assert!(!ids.contains(&done.as_str()));
    }

    #[test]
    fn board_limit_caps_rows() {
        let s = store();
        for i in 0..5 {
            s.create(&format!("issue {i}"), "", None, None, None)
                .unwrap();
        }
        assert_eq!(s.board(Some(2)).unwrap().len(), 2);
        assert_eq!(s.board(None).unwrap().len(), 5);
    }

    // ── search ───────────────────────────────────────────────────────────

    #[test]
    fn search_matches_title_and_body_case_insensitive() {
        let s = store();
        s.create("Refactor auth", "", None, None, None).unwrap();
        s.create("Other", "touches AUTH layer", None, None, None)
            .unwrap();
        s.create("Unrelated", "nope", None, None, None).unwrap();
        let hits = s.search("auth", 10).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn search_treats_like_metacharacters_as_literals() {
        let s = store();
        s.create("100% done", "", None, None, None).unwrap();
        s.create("foo_bar", "", None, None, None).unwrap();
        s.create("fooXbar", "", None, None, None).unwrap();
        s.create("plain", "", None, None, None).unwrap();
        // `%` must be a literal, not "match anything".
        let pct = s.search("100%", 10).unwrap();
        assert_eq!(pct.len(), 1);
        assert_eq!(pct[0].title, "100% done");
        // `_` must be a literal, not a single-char wildcard.
        let us = s.search("foo_bar", 10).unwrap();
        assert_eq!(us.len(), 1);
        assert_eq!(us[0].title, "foo_bar");
    }

    // ── board reminder ───────────────────────────────────────────────────

    #[test]
    fn board_reminder_split_hints_backlog_overflow() {
        let s = store();
        // 4 unassigned (passive) issues; cap the backlog section at 2.
        for i in 0..4 {
            s.create(&format!("backlog {i}"), "", None, None, None)
                .unwrap();
        }
        let block = s
            .board_reminder_split(Some("sess-1"), 3, 2)
            .unwrap()
            .expect("non-empty board");
        assert!(block.starts_with("<system-reminder>"));
        assert!(block.trim_end().ends_with("</system-reminder>"));
        // Only 2 of 4 shown → overflow hint mentions the remaining 2.
        assert!(block.contains("2 more in the backlog"), "{block}");
    }

    // ── backlog ───────────────────────────────────────────────────────────

    #[test]
    fn backlog_returns_unassigned_issues_and_excludes_terminal() {
        let s = store();
        // Unassigned (passive) issues.
        let passive_open = s.create("passive open", "", None, None, None).unwrap();
        let passive_blocked = s.create("passive blocked", "", None, None, None).unwrap();
        s.set_status(&passive_blocked, "blocked").unwrap();
        let passive_done = s.create("passive done", "", None, None, None).unwrap();
        s.set_status(&passive_done, "done").unwrap();
        // Assigned (active) issue — must NOT appear in backlog.
        s.create("active wip", "", None, Some("sess-1"), None)
            .unwrap();

        let backlog = s.backlog(None).unwrap();
        let ids: Vec<&str> = backlog.iter().map(|i| i.id.as_str()).collect();
        // blocked is NOT terminal, so both open and blocked appear.
        assert_eq!(
            ids,
            vec![passive_blocked.as_str(), passive_open.as_str()],
            "backlog must contain unassigned open+blocked only"
        );
        // done excluded
        assert!(
            !ids.contains(&passive_done.as_str()),
            "terminal issues excluded from backlog"
        );
    }

    #[test]
    fn backlog_respects_limit() {
        let s = store();
        for i in 0..5 {
            s.create(&format!("backlog {i}"), "", None, None, None)
                .unwrap();
        }
        assert_eq!(s.backlog(Some(2)).unwrap().len(), 2);
    }

    // ── board reminder split ──────────────────────────────────────────────

    #[test]
    fn board_reminder_split_active_section_and_backlog_section() {
        let s = store();
        // Active: session-scoped in_progress issue.
        let active_id = s
            .create("active task", "", Some("high"), Some("sess-1"), None)
            .unwrap();
        s.set_status(&active_id, "in_progress").unwrap();
        // Passive: unassigned open issue.
        let _passive_id = s.create("backlog task", "", None, None, None).unwrap();

        let block = s
            .board_reminder_split(Some("sess-1"), 3, 3)
            .unwrap()
            .expect("non-empty board");
        assert!(block.starts_with("<system-reminder>"));
        assert!(block.trim_end().ends_with("</system-reminder>"));

        // Active section present with correct framing.
        assert!(
            block.contains("Active work queue"),
            "must have Active section: {block}"
        );
        assert!(block.contains("keep one item in_progress"), "{block}");
        // The in_progress item gets a callout.
        assert!(
            block.contains("Currently in progress: active task"),
            "{block}"
        );
        assert!(block.contains("active task"), "{block}");

        // Backlog section present with correct framing.
        assert!(
            block.contains("Backlog"),
            "must have Backlog section: {block}"
        );
        assert!(block.contains("filed for later"), "{block}");
        assert!(
            block.contains("start one with the `issue` tool to pick it up"),
            "{block}"
        );
        assert!(block.contains("backlog task"), "{block}");
    }

    #[test]
    fn board_reminder_split_passive_only_no_active_section() {
        let s = store();
        s.create("only passive", "", None, None, None).unwrap();

        let block = s
            .board_reminder_split(Some("sess-1"), 3, 3)
            .unwrap()
            .expect("non-empty board");
        // Backlog section present.
        assert!(block.contains("Backlog"), "{block}");
        // NO Active section.
        assert!(
            !block.contains("Active work queue"),
            "no active work → no Active section: {block}"
        );
    }

    #[test]
    fn board_reminder_split_currently_in_progress_callout() {
        let s = store();
        let active_id = s
            .create("solo task", "", Some("normal"), Some("sess-1"), None)
            .unwrap();
        s.set_status(&active_id, "in_progress").unwrap();

        let block = s
            .board_reminder_split(Some("sess-1"), 3, 3)
            .unwrap()
            .expect("non-empty board");
        assert!(
            block.contains("Currently in progress: solo task"),
            "in_progress item must have callout: {block}"
        );
    }

    #[test]
    fn board_reminder_split_no_in_progress_no_callout() {
        let s = store();
        // Active but NOT in_progress — all open.
        s.create("open item", "", None, Some("sess-1"), None)
            .unwrap();

        let block = s
            .board_reminder_split(Some("sess-1"), 3, 3)
            .unwrap()
            .expect("non-empty board");
        assert!(block.contains("Active work queue"), "{block}");
        assert!(
            !block.contains("Currently in progress:"),
            "no in_progress → no callout: {block}"
        );
    }

    #[test]
    fn board_reminder_split_none_when_both_empty() {
        let s = store();
        assert!(
            s.board_reminder_split(Some("sess-1"), 5, 5)
                .unwrap()
                .is_none(),
            "both empty → None"
        );
    }

    // ── cancelled / terminal states ──────────────────────────────────────

    #[test]
    fn cancelled_is_terminal_stamps_closed_at_and_leaves_board() {
        let s = store();
        let id = s.create("x", "", None, Some("sess-1"), None).unwrap();
        assert!(s.set_status(&id, "cancelled").unwrap());
        let c = s.get(&id).unwrap().unwrap();
        assert_eq!(c.status, "cancelled");
        assert!(c.closed_at.is_some(), "cancelled should stamp closed_at");
        // Terminal → off the live board.
        let board_ids: Vec<String> = s
            .board(None)
            .unwrap()
            .iter()
            .map(|i| i.id.clone())
            .collect();
        assert!(!board_ids.contains(&id));
    }

    #[test]
    fn board_excludes_both_terminal_states() {
        let s = store();
        let _live = s.create("live", "", None, None, None).unwrap();
        let done = s.create("done", "", None, None, None).unwrap();
        s.set_status(&done, "done").unwrap();
        let cancelled = s.create("cancelled", "", None, None, None).unwrap();
        s.set_status(&cancelled, "cancelled").unwrap();
        // Only the one live issue is on the board — both terminal states
        // (done and cancelled) drop off.
        assert_eq!(s.board(None).unwrap().len(), 1);
    }

    #[test]
    fn board_for_session_scopes_to_session_and_excludes_terminal() {
        let s = store();
        let mine = s
            .create("mine open", "", None, Some("sess-1"), None)
            .unwrap();
        let mine_done = s
            .create("mine done", "", None, Some("sess-1"), None)
            .unwrap();
        s.set_status(&mine_done, "done").unwrap();
        let _other = s.create("other", "", None, Some("sess-2"), None).unwrap();

        let board = s.board_for_session(Some("sess-1"), None).unwrap();
        let ids: Vec<&str> = board.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec![mine.as_str()], "only this session's live issues");
    }

    // ── sync_todos ───────────────────────────────────────────────────────

    #[test]
    fn sync_todos_upserts_by_title_and_does_not_close_omitted() {
        let s = store();
        // First plan: two items.
        let n = s
            .sync_todos(
                Some("sess-1"),
                &[
                    ("Build auth", "pending", "high"),
                    ("Write tests", "in_progress", "low"),
                ],
            )
            .unwrap();
        assert_eq!(n, 2);
        let board = s.board_for_session(Some("sess-1"), None).unwrap();
        assert_eq!(board.len(), 2);

        // Restate with one completed; the other is omitted (must stay live).
        s.sync_todos(Some("sess-1"), &[("Build auth", "completed", "high")])
            .unwrap();
        let board = s.board_for_session(Some("sess-1"), None).unwrap();
        let titles: Vec<&str> = board.iter().map(|i| i.title.as_str()).collect();
        // "Build auth" closed (off the board), "Write tests" untouched (still live).
        assert_eq!(titles, vec!["Write tests"], "omitted item not auto-closed");
        // No duplicate "Build auth" row was created on the second sync.
        assert_eq!(s.search("Build auth", 10).unwrap().len(), 1);
        let done = s.search("Build auth", 10).unwrap().pop().unwrap();
        assert_eq!(done.status, "done");
        assert!(done.closed_at.is_some());
    }

    #[test]
    fn sync_todos_reopen_clears_closed_at() {
        let s = store();
        s.sync_todos(Some("sess-1"), &[("task", "completed", "normal")])
            .unwrap();
        s.sync_todos(Some("sess-1"), &[("task", "in_progress", "normal")])
            .unwrap();
        let issue = s.search("task", 10).unwrap().pop().unwrap();
        assert_eq!(issue.status, "in_progress");
        assert!(issue.closed_at.is_none(), "reopen must clear closed_at");
    }

    #[test]
    fn sync_todos_matches_case_and_whitespace_insensitively() {
        let s = store();
        s.sync_todos(Some("sess-1"), &[("Build auth", "pending", "high")])
            .unwrap();
        // Restate with different case and extra spaces — must match the existing row.
        s.sync_todos(Some("sess-1"), &[("  build  AUTH ", "completed", "high")])
            .unwrap();
        // Only one row exists, and it's now done.
        let hits = s.search("build", 10).unwrap();
        assert_eq!(hits.len(), 1, "must not create a duplicate row");
        assert_eq!(hits[0].title, "Build auth");
        assert_eq!(hits[0].status, "done");
        assert!(hits[0].closed_at.is_some());
    }

    #[test]
    fn sync_todos_exact_title_still_works() {
        let s = store();
        s.sync_todos(Some("sess-1"), &[("Build auth", "pending", "high")])
            .unwrap();
        s.sync_todos(Some("sess-1"), &[("Build auth", "completed", "high")])
            .unwrap();
        let hits = s.search("Build auth", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].status, "done");
    }

    #[test]
    fn sync_todos_different_titles_still_create_two_rows() {
        let s = store();
        s.sync_todos(Some("sess-1"), &[("Build auth", "pending", "high")])
            .unwrap();
        s.sync_todos(Some("sess-1"), &[("Build dashboard", "pending", "normal")])
            .unwrap();
        let board = s.board_for_session(Some("sess-1"), None).unwrap();
        assert_eq!(board.len(), 2);
    }
}
