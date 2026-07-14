//! Native issue tracker — a lightweight, agent-facing kanban stored in the
//! per-project session DB (`.dirge/sessions/state.db`).
//!
//! Issues are a stateful extension of the memory model: open issues are the
//! agent's working board (surfaced by the harness at turn start), and closed
//! issues fade like ordinary procedural memory. The store deliberately owns
//! its schema via idempotent `CREATE TABLE IF NOT EXISTS` on open rather than a
//! versioned migration — the session DB's `user_version` chain is feature-gated
//! (v14/v15 exist only under `experimental-graph-search`), so threading a new
//! linear version would either collide with those or break the "enable the
//! feature later" property. An additive, self-owned table sidesteps all of it.

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

/// Parse an issue id from intuitive forms: `7`, `#7`, `iss-7`, `issue 7`.
pub fn parse_issue_id(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let s = s.strip_prefix("issue").map(str::trim).unwrap_or(s);
    let s = s.strip_prefix('#').unwrap_or(s);
    let s = s.strip_prefix("iss-").unwrap_or(s);
    let s = s.strip_prefix("drg-").unwrap_or(s);
    s.trim().parse::<i64>().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub status: String,
    pub priority: String,
    pub session_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

impl Issue {
    /// Intuitive one-line rendering for the board / lists: `#7 [in_progress]
    /// (high) Build auth middleware`.
    pub fn one_line(&self) -> String {
        format!(
            "#{} [{}] ({}) {}",
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
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        closed_at: row.get(8)?,
    })
}

const COLS: &str =
    "id, title, body, status, priority, session_id, created_at, updated_at, closed_at";

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
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS issues (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                title       TEXT NOT NULL,
                body        TEXT NOT NULL DEFAULT '',
                status      TEXT NOT NULL DEFAULT 'open',
                priority    TEXT NOT NULL DEFAULT 'normal',
                session_id  TEXT,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                closed_at   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status);",
        )
        .map_err(|e| format!("ensure issues schema: {e}"))
    }

    /// Create a new issue. `priority` and `session_id` are optional; an unknown
    /// priority falls back to `normal`.
    pub fn create(
        &self,
        title: &str,
        body: &str,
        priority: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<i64, String> {
        let title = title.trim();
        if title.is_empty() {
            return Err("issue title must not be empty".to_string());
        }
        let priority = priority.and_then(normalize_priority).unwrap_or("normal");
        let conn = self.conn.lock_ignore_poison();
        let now = now();
        // Stamp the lineage origin, not the rotating id, so the row stays on
        // this conversation's board across a later compaction fold.
        let scoped = session_id.map(|s| origin_of(&conn, s));
        conn.execute(
            "INSERT INTO issues (title, body, status, priority, session_id, created_at, updated_at)
             VALUES (?1, ?2, 'open', ?3, ?4, ?5, ?5)",
            params![title, body.trim(), priority, scoped, now],
        )
        .map_err(|e| format!("create issue: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get(&self, id: i64) -> Result<Option<Issue>, String> {
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
    pub fn set_status(&self, id: i64, status: &str) -> Result<bool, String> {
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
    pub fn assign_to_session(&self, id: i64, session_id: Option<&str>) -> Result<bool, String> {
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

    pub fn set_priority(&self, id: i64, priority: &str) -> Result<bool, String> {
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
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT id FROM issues WHERE session_id IS ?1 AND title = ?2 ORDER BY id DESC LIMIT 1",
                    params![scoped_session, title],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| format!("sync todos (lookup): {e}"))?;
            match existing {
                Some(id) => {
                    tx.execute(
                        "UPDATE issues SET status = ?2, priority = ?3, updated_at = ?4, closed_at = ?5 WHERE id = ?1",
                        params![id, status, priority, now, closed],
                    )
                    .map_err(|e| format!("sync todos (update): {e}"))?;
                }
                None => {
                    tx.execute(
                        "INSERT INTO issues (title, body, status, priority, session_id, created_at, updated_at, closed_at)
                         VALUES (?1, '', ?2, ?3, ?4, ?5, ?5, ?6)",
                        params![title, status, priority, scoped_session, now, closed],
                    )
                    .map_err(|e| format!("sync todos (insert): {e}"))?;
                }
            }
            applied += 1;
        }
        tx.commit()
            .map_err(|e| format!("sync todos (commit): {e}"))?;
        Ok(applied)
    }

    /// Count of live issues — used for the "N more" injection hint. Must match
    /// [`Self::board`]'s membership (open / in_progress / blocked) so the
    /// overflow hint is consistent: both terminal states (`done` and
    /// `cancelled`) are excluded, otherwise cancelled issues would inflate the
    /// "and N more" count for rows the model can never list.
    pub fn open_count(&self) -> Result<usize, String> {
        let conn = self.conn.lock_ignore_poison();
        conn.query_row(
            "SELECT COUNT(*) FROM issues WHERE status NOT IN ('done','cancelled')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n as usize)
        .map_err(|e| format!("open_count: {e}"))
    }

    /// Build the harness's turn-start board reminder: the top `top_n` live
    /// issues, wrapped in a `<system-reminder>` block, with a hint to see the
    /// rest when there are more. Returns `None` when the board is empty (so the
    /// caller injects nothing). Token-bounded by `top_n` — the caller owns the
    /// budget the same way the post-compaction snapshot cap does.
    pub fn board_reminder(&self, top_n: usize) -> Result<Option<String>, String> {
        let issues = self.board(Some(top_n))?;
        if issues.is_empty() {
            return Ok(None);
        }
        let total = self.open_count()?;
        let mut s = String::from(
            "<system-reminder>\nIssue board (your persistent kanban — surfaced automatically; you did not ask for it). \
             As you work: `issue` tool with action=start when you begin one, action=close when done, action=create for newly-discovered work.\n",
        );
        for i in &issues {
            s.push_str(&format!("- {}\n", i.one_line()));
        }
        let shown = issues.len();
        if total > shown {
            s.push_str(&format!(
                "… and {} more open issue(s) not shown. Use the `issue` tool (action=list) or /issues to see all.\n",
                total - shown
            ));
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

    #[test]
    fn board_scopes_by_lineage_origin_across_fold() {
        let (s, path) = store_at();
        seed_lineage(&path, "tip", "orig");
        // Created pre-fold under the original id, then started.
        let id = s.create("keep me", "", Some("high"), Some("orig")).unwrap();
        s.set_status(id, "in_progress").unwrap();
        // A compaction fold rotates the session id to "tip"; the board queried
        // under the new id still finds the issue because both normalize to the
        // shared origin.
        let board = s.board_for_session(Some("tip"), None).unwrap();
        assert_eq!(board.len(), 1, "issue must survive the id rotation");
        assert_eq!(board[0].title, "keep me");
        // The stored scope was normalized to the origin, not the raw id.
        assert_eq!(
            s.get(id).unwrap().unwrap().session_id.as_deref(),
            Some("orig")
        );
    }

    #[test]
    fn assign_to_session_claims_foreign_issue_onto_board() {
        // No sessions table here → origin_of falls back to identity, exercising
        // the graceful degradation path too.
        let s = store();
        let id = s
            .create("from elsewhere", "", None, Some("other-sess"))
            .unwrap();
        s.set_status(id, "in_progress").unwrap();
        // Not on our board yet — it belongs to another conversation.
        assert!(
            s.board_for_session(Some("mine"), None).unwrap().is_empty(),
            "foreign issue must not appear before pickup"
        );
        // Picking it up claims it for us.
        assert!(s.assign_to_session(id, Some("mine")).unwrap());
        let board = s.board_for_session(Some("mine"), None).unwrap();
        assert_eq!(board.len(), 1);
        assert_eq!(board[0].title, "from elsewhere");
        // A session-less runner must not blank the scope.
        assert!(!s.assign_to_session(id, None).unwrap());
        assert_eq!(
            s.get(id).unwrap().unwrap().session_id.as_deref(),
            Some("mine")
        );
    }

    #[test]
    fn create_and_get_roundtrip() {
        let s = store();
        let id = s
            .create("Build auth", "use PKCE", Some("high"), Some("sess-1"))
            .unwrap();
        let issue = s.get(id).unwrap().expect("issue exists");
        assert_eq!(issue.title, "Build auth");
        assert_eq!(issue.body, "use PKCE");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.priority, "high");
        assert_eq!(issue.session_id.as_deref(), Some("sess-1"));
        assert!(issue.closed_at.is_none());
    }

    #[test]
    fn empty_title_rejected() {
        let s = store();
        assert!(s.create("   ", "", None, None).is_err());
    }

    #[test]
    fn unknown_priority_falls_back_to_normal() {
        let s = store();
        let id = s.create("x", "", Some("supercritical"), None).unwrap();
        assert_eq!(s.get(id).unwrap().unwrap().priority, "normal");
    }

    #[test]
    fn set_status_done_stamps_closed_at_and_clears_on_reopen() {
        let s = store();
        let id = s.create("x", "", None, None).unwrap();
        assert!(s.set_status(id, "done").unwrap());
        let done = s.get(id).unwrap().unwrap();
        assert_eq!(done.status, "done");
        assert!(done.closed_at.is_some());
        // reopen clears closed_at
        assert!(s.set_status(id, "in_progress").unwrap());
        let reopened = s.get(id).unwrap().unwrap();
        assert_eq!(reopened.status, "in_progress");
        assert!(reopened.closed_at.is_none());
    }

    #[test]
    fn set_status_aliases_normalize() {
        let s = store();
        let id = s.create("x", "", None, None).unwrap();
        assert!(s.set_status(id, "WIP").unwrap());
        assert_eq!(s.get(id).unwrap().unwrap().status, "in_progress");
    }

    #[test]
    fn set_status_unknown_rejected_and_missing_id_is_false() {
        let s = store();
        let id = s.create("x", "", None, None).unwrap();
        assert!(s.set_status(id, "nonsense").is_err());
        assert!(!s.set_status(999, "done").unwrap());
    }

    #[test]
    fn board_orders_in_progress_then_priority_and_excludes_done() {
        let s = store();
        let _low_open = s.create("low open", "", Some("low"), None).unwrap();
        let high_open = s.create("high open", "", Some("high"), None).unwrap();
        let wip = s.create("wip", "", Some("low"), None).unwrap();
        s.set_status(wip, "in_progress").unwrap();
        let done = s.create("done", "", Some("high"), None).unwrap();
        s.set_status(done, "done").unwrap();

        let board = s.board(None).unwrap();
        let ids: Vec<i64> = board.iter().map(|i| i.id).collect();
        // in_progress first regardless of priority, then high open, then low open.
        assert_eq!(ids, vec![wip, high_open, _low_open]);
        // done excluded
        assert!(!ids.contains(&done));
    }

    #[test]
    fn board_limit_caps_rows() {
        let s = store();
        for i in 0..5 {
            s.create(&format!("issue {i}"), "", None, None).unwrap();
        }
        assert_eq!(s.board(Some(2)).unwrap().len(), 2);
        assert_eq!(s.open_count().unwrap(), 5);
    }

    #[test]
    fn search_matches_title_and_body_case_insensitive() {
        let s = store();
        s.create("Refactor auth", "", None, None).unwrap();
        s.create("Other", "touches AUTH layer", None, None).unwrap();
        s.create("Unrelated", "nope", None, None).unwrap();
        let hits = s.search("auth", 10).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn search_treats_like_metacharacters_as_literals() {
        let s = store();
        s.create("100% done", "", None, None).unwrap();
        s.create("foo_bar", "", None, None).unwrap();
        s.create("fooXbar", "", None, None).unwrap();
        s.create("plain", "", None, None).unwrap();
        // `%` must be a literal, not "match anything".
        let pct = s.search("100%", 10).unwrap();
        assert_eq!(pct.len(), 1);
        assert_eq!(pct[0].title, "100% done");
        // `_` must be a literal, not a single-char wildcard.
        let us = s.search("foo_bar", 10).unwrap();
        assert_eq!(us.len(), 1);
        assert_eq!(us[0].title, "foo_bar");
    }

    #[test]
    fn board_reminder_none_when_empty_and_hints_overflow() {
        let s = store();
        assert!(
            s.board_reminder(5).unwrap().is_none(),
            "empty board → no reminder"
        );
        for i in 0..4 {
            s.create(&format!("issue {i}"), "", None, None).unwrap();
        }
        let block = s.board_reminder(2).unwrap().expect("non-empty board");
        assert!(block.starts_with("<system-reminder>"));
        assert!(block.trim_end().ends_with("</system-reminder>"));
        // Only 2 shown, 4 live → overflow hint mentions the remaining 2.
        assert!(block.contains("2 more open issue"), "{block}");
    }

    #[test]
    fn cancelled_is_terminal_stamps_closed_at_and_leaves_board() {
        let s = store();
        let id = s.create("x", "", None, Some("sess-1")).unwrap();
        assert!(s.set_status(id, "cancelled").unwrap());
        let c = s.get(id).unwrap().unwrap();
        assert_eq!(c.status, "cancelled");
        assert!(c.closed_at.is_some(), "cancelled should stamp closed_at");
        // Terminal → off the live board.
        assert!(s.board(None).unwrap().iter().all(|i| i.id != id));
    }

    #[test]
    fn open_count_excludes_both_terminal_states() {
        let s = store();
        let live = s.create("live", "", None, None).unwrap();
        let _ = live;
        let done = s.create("done", "", None, None).unwrap();
        s.set_status(done, "done").unwrap();
        let cancelled = s.create("cancelled", "", None, None).unwrap();
        s.set_status(cancelled, "cancelled").unwrap();
        // Only the one live issue counts — cancelled must not inflate it (it's
        // excluded from board(), so the "N more" hint would otherwise lie).
        assert_eq!(s.open_count().unwrap(), 1);
        assert_eq!(s.board(None).unwrap().len(), 1);
    }

    #[test]
    fn board_for_session_scopes_to_session_and_excludes_terminal() {
        let s = store();
        let mine = s.create("mine open", "", None, Some("sess-1")).unwrap();
        let mine_done = s.create("mine done", "", None, Some("sess-1")).unwrap();
        s.set_status(mine_done, "done").unwrap();
        let _other = s.create("other", "", None, Some("sess-2")).unwrap();

        let board = s.board_for_session(Some("sess-1"), None).unwrap();
        let ids: Vec<i64> = board.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![mine], "only this session's live issues");
    }

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
    fn parse_issue_id_accepts_intuitive_forms() {
        assert_eq!(parse_issue_id("7"), Some(7));
        assert_eq!(parse_issue_id("#7"), Some(7));
        assert_eq!(parse_issue_id("iss-7"), Some(7));
        assert_eq!(parse_issue_id("issue 7"), Some(7));
        assert_eq!(parse_issue_id("nope"), None);
    }
}
