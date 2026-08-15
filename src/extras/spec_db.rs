//! SQLite-backed spec-driven workflow tracker (OpenSpec-inspired).
//!
//! OpenSpec (github.com/Fission-AI/OpenSpec) models feature work as a small
//! artifact graph — proposal → specs → design → tasks → implement → archive
//! — where *living specs* (capability → requirement → scenario) are the
//! current truth and a *change* carries deltas (ADDED/MODIFIED/REMOVED/
//! RENAMED requirements) that are folded into the living specs when the
//! change is archived. OpenSpec stores all of this as a tree of markdown
//! files and parses them back with regexes.
//!
//! dirge keeps the same model but stores it as rows in the per-project
//! session DB (migration v11), reusing the same SQLite the memory store
//! lives in. The wins over the markdown tree:
//! - No silent parse failures (OpenSpec warns that a task with the wrong
//!   checkbox, or a scenario with 3 `#` instead of 4, "fails silently").
//! - Real task status (`pending|in_progress|done|blocked`) as a column,
//!   queryable for progress — not regex over `- [ ]`.
//! - Queryable specs and a transactional archive fold.
//!
//! This is the data model and ops; the agent drives it through the `spec`
//! tool ([`crate::agent::tools::SpecTool`]), and the active change is
//! injected into the preamble ([`SpecStore::format_active_change_for_prompt`])
//! so a resumed session knows what it's implementing.

use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::extras::dirge_paths::ProjectPaths;
use crate::extras::session_db::SessionDb;
use crate::sync_util::LockExt;

/// A WHEN/THEN behavior example attached to a requirement. Stored in a
/// delta as a JSON array; promoted to `spec_scenarios` rows on archive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Scenario {
    pub name: String,
    pub when_then: String,
}

/// A proposed delta against the living specs, recorded on a change and
/// applied at archive time. `op` is one of `added|modified|removed|renamed`.
#[derive(Debug, Clone, Serialize)]
pub struct Delta {
    pub id: i64,
    pub op: String,
    pub capability: String,
    pub requirement: String,
    pub text: String,
    pub scenarios: Vec<Scenario>,
    pub reason: String,
    pub migration: String,
    pub rename_to: String,
}

/// An implementation task with real status tracking.
#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: i64,
    pub group_no: i64,
    pub seq: i64,
    pub text: String,
    pub status: String,
}

/// A change in flight (the OpenSpec "proposal" plus its design body).
#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub why: String,
    pub what: String,
    pub design: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub archived_at: Option<String>,
}

/// A living-spec requirement with its scenarios (current truth).
#[derive(Debug, Clone, Serialize)]
pub struct Requirement {
    pub capability: String,
    pub name: String,
    pub text: String,
    pub scenarios: Vec<Scenario>,
}

/// What an `archive` fold did, for reporting back to the caller.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ArchiveReport {
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub renamed: usize,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// SQLite-backed spec tracker over the per-project session DB. Holds its
/// own connection (mirrors `SqliteMemoryStore`), so opening it runs the
/// schema migration that creates the `spec_*` tables.
pub struct SpecStore {
    conn: Mutex<Connection>,
}

impl SpecStore {
    /// Open the tracker against a project's session DB, creating the
    /// `spec_*` tables via migration if needed.
    pub fn open(paths: &ProjectPaths) -> Result<Self, String> {
        Self::open_at(&paths.session_db_path())
    }

    /// Open against an explicit DB path (used by tests).
    pub fn open_at(path: &std::path::Path) -> Result<Self, String> {
        let db = SessionDb::open(path)?;
        // Foreign-key cascade (delta/task/scenario cleanup) is off by
        // default in SQLite — enable it per connection.
        let _ = db.conn.execute_batch("PRAGMA foreign_keys = ON;");
        Ok(Self {
            conn: Mutex::new(db.conn),
        })
    }

    // ----- changes ------------------------------------------------------

    /// Create a new change. Fails if `slug` already exists.
    pub fn create_change(
        &self,
        slug: &str,
        title: &str,
        why: &str,
        what: &str,
    ) -> Result<i64, String> {
        let conn = self.conn.lock_ignore_poison();
        let now = now();
        conn.execute(
            "INSERT INTO spec_changes (slug, title, why, what, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'draft', ?5, ?5)",
            params![slug, title, why, what, now],
        )
        .map_err(|e| format!("create_change: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    /// Look up a change by its slug.
    pub fn get_change(&self, slug: &str) -> Result<Option<Change>, String> {
        let conn = self.conn.lock_ignore_poison();
        conn.query_row(
            "SELECT id, slug, title, why, what, design, status, created_at, updated_at, archived_at
             FROM spec_changes WHERE slug = ?1",
            params![slug],
            row_to_change,
        )
        .optional()
        .map_err(|e| format!("get_change: {e}"))
    }

    /// List changes, optionally filtered by status, newest first.
    pub fn list_changes(&self, status: Option<&str>) -> Result<Vec<Change>, String> {
        let conn = self.conn.lock_ignore_poison();
        let mut out = Vec::new();
        if let Some(s) = status {
            let mut stmt = conn
                .prepare(
                    "SELECT id, slug, title, why, what, design, status, created_at, updated_at, archived_at
                     FROM spec_changes WHERE status = ?1 ORDER BY id DESC",
                )
                .map_err(|e| format!("list_changes: {e}"))?;
            let rows = stmt
                .query_map(params![s], row_to_change)
                .map_err(|e| format!("list_changes: {e}"))?;
            for r in rows {
                out.push(r.map_err(|e| format!("list_changes: {e}"))?);
            }
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT id, slug, title, why, what, design, status, created_at, updated_at, archived_at
                     FROM spec_changes ORDER BY id DESC",
                )
                .map_err(|e| format!("list_changes: {e}"))?;
            let rows = stmt
                .query_map([], row_to_change)
                .map_err(|e| format!("list_changes: {e}"))?;
            for r in rows {
                out.push(r.map_err(|e| format!("list_changes: {e}"))?);
            }
        }
        Ok(out)
    }

    /// Update a single text field of a change (`title|why|what|design`).
    pub fn set_change_field(&self, slug: &str, field: &str, value: &str) -> Result<(), String> {
        let column = match field {
            "title" => "title",
            "why" => "why",
            "what" => "what",
            "design" => "design",
            other => return Err(format!("set_change_field: unknown field '{other}'")),
        };
        let conn = self.conn.lock_ignore_poison();
        let sql = format!("UPDATE spec_changes SET {column} = ?2, updated_at = ?3 WHERE slug = ?1");
        let n = conn
            .execute(&sql, params![slug, value, now()])
            .map_err(|e| format!("set_change_field: {e}"))?;
        if n == 0 {
            return Err(format!("set_change_field: no change '{slug}'"));
        }
        Ok(())
    }

    /// Move a change to a new lifecycle status.
    pub fn set_change_status(&self, slug: &str, status: &str) -> Result<(), String> {
        let conn = self.conn.lock_ignore_poison();
        let n = conn
            .execute(
                "UPDATE spec_changes SET status = ?2, updated_at = ?3 WHERE slug = ?1",
                params![slug, status, now()],
            )
            .map_err(|e| format!("set_change_status: {e}"))?;
        if n == 0 {
            return Err(format!("set_change_status: no change '{slug}'"));
        }
        Ok(())
    }

    fn change_id(conn: &Connection, slug: &str) -> Result<i64, String> {
        conn.query_row(
            "SELECT id FROM spec_changes WHERE slug = ?1",
            params![slug],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| format!("change_id: {e}"))?
        .ok_or_else(|| format!("no change '{slug}'"))
    }

    // ----- tasks --------------------------------------------------------

    /// Append a task to a change. Returns the new task id.
    pub fn add_task(&self, slug: &str, group_no: i64, seq: i64, text: &str) -> Result<i64, String> {
        let conn = self.conn.lock_ignore_poison();
        let cid = Self::change_id(&conn, slug)?;
        let now = now();
        conn.execute(
            "INSERT INTO spec_tasks (change_id, group_no, seq, text, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?5)",
            params![cid, group_no, seq, text, now],
        )
        .map_err(|e| format!("add_task: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    /// Set a task's status (`pending|in_progress|done|blocked`).
    pub fn set_task_status(&self, task_id: i64, status: &str) -> Result<(), String> {
        if !matches!(status, "pending" | "in_progress" | "done" | "blocked") {
            return Err(format!("set_task_status: bad status '{status}'"));
        }
        let conn = self.conn.lock_ignore_poison();
        let n = conn
            .execute(
                "UPDATE spec_tasks SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![task_id, status, now()],
            )
            .map_err(|e| format!("set_task_status: {e}"))?;
        if n == 0 {
            return Err(format!("set_task_status: no task {task_id}"));
        }
        Ok(())
    }

    /// List a change's tasks in (group, seq) order.
    pub fn list_tasks(&self, slug: &str) -> Result<Vec<Task>, String> {
        let conn = self.conn.lock_ignore_poison();
        let cid = Self::change_id(&conn, slug)?;
        let mut stmt = conn
            .prepare(
                "SELECT id, group_no, seq, text, status FROM spec_tasks
                 WHERE change_id = ?1 ORDER BY group_no, seq, id",
            )
            .map_err(|e| format!("list_tasks: {e}"))?;
        let rows = stmt
            .query_map(params![cid], |row| {
                Ok(Task {
                    id: row.get(0)?,
                    group_no: row.get(1)?,
                    seq: row.get(2)?,
                    text: row.get(3)?,
                    status: row.get(4)?,
                })
            })
            .map_err(|e| format!("list_tasks: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("list_tasks: {e}"))?);
        }
        Ok(out)
    }

    /// Return `(done, total)` task counts for a change.
    pub fn task_progress(&self, slug: &str) -> Result<(usize, usize), String> {
        let conn = self.conn.lock_ignore_poison();
        let cid = Self::change_id(&conn, slug)?;
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM spec_tasks WHERE change_id = ?1",
                params![cid],
                |r| r.get(0),
            )
            .map_err(|e| format!("task_progress: {e}"))?;
        let done: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM spec_tasks WHERE change_id = ?1 AND status = 'done'",
                params![cid],
                |r| r.get(0),
            )
            .map_err(|e| format!("task_progress: {e}"))?;
        Ok((done as usize, total as usize))
    }

    // ----- deltas -------------------------------------------------------

    /// Record a delta on a change. `scenarios` is promoted to living-spec
    /// scenario rows when the change is archived.
    #[allow(clippy::too_many_arguments)]
    pub fn add_delta(
        &self,
        slug: &str,
        op: &str,
        capability: &str,
        requirement: &str,
        text: &str,
        scenarios: &[Scenario],
        reason: &str,
        migration: &str,
        rename_to: &str,
    ) -> Result<i64, String> {
        if !matches!(op, "added" | "modified" | "removed" | "renamed") {
            return Err(format!("add_delta: bad op '{op}'"));
        }
        let scenarios_json =
            serde_json::to_string(scenarios).map_err(|e| format!("add_delta: {e}"))?;
        let conn = self.conn.lock_ignore_poison();
        let cid = Self::change_id(&conn, slug)?;
        conn.execute(
            "INSERT INTO spec_deltas
                (change_id, op, capability, requirement, text, scenarios, reason, migration, rename_to, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                cid, op, capability, requirement, text, scenarios_json, reason, migration,
                rename_to, now()
            ],
        )
        .map_err(|e| format!("add_delta: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    /// List a change's deltas in insertion order.
    pub fn list_deltas(&self, slug: &str) -> Result<Vec<Delta>, String> {
        let conn = self.conn.lock_ignore_poison();
        let cid = Self::change_id(&conn, slug)?;
        let mut stmt = conn
            .prepare(
                "SELECT id, op, capability, requirement, text, scenarios, reason, migration, rename_to
                 FROM spec_deltas WHERE change_id = ?1 ORDER BY id",
            )
            .map_err(|e| format!("list_deltas: {e}"))?;
        let rows = stmt
            .query_map(params![cid], row_to_delta)
            .map_err(|e| format!("list_deltas: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("list_deltas: {e}"))?);
        }
        Ok(out)
    }

    // ----- living specs (current truth) --------------------------------

    /// List capability names (living specs), alphabetically.
    pub fn list_capabilities(&self) -> Result<Vec<String>, String> {
        let conn = self.conn.lock_ignore_poison();
        let mut stmt = conn
            .prepare("SELECT name FROM spec_capabilities ORDER BY name")
            .map_err(|e| format!("list_capabilities: {e}"))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| format!("list_capabilities: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("list_capabilities: {e}"))?);
        }
        Ok(out)
    }

    /// All living requirements for a capability, with their scenarios.
    pub fn capability_requirements(&self, capability: &str) -> Result<Vec<Requirement>, String> {
        let conn = self.conn.lock_ignore_poison();
        let cap_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM spec_capabilities WHERE name = ?1",
                params![capability],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| format!("capability_requirements: {e}"))?;
        let Some(cap_id) = cap_id else {
            return Ok(Vec::new());
        };
        let reqs: Vec<(i64, String, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, text FROM spec_requirements
                     WHERE capability_id = ?1 ORDER BY name",
                )
                .map_err(|e| format!("capability_requirements: {e}"))?;
            let rows = stmt
                .query_map(params![cap_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .map_err(|e| format!("capability_requirements: {e}"))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r.map_err(|e| format!("capability_requirements: {e}"))?);
            }
            v
        };
        let mut out = Vec::new();
        for (rid, name, text) in reqs {
            out.push(Requirement {
                capability: capability.to_string(),
                name,
                text,
                scenarios: scenarios_for(&conn, rid)?,
            });
        }
        Ok(out)
    }

    // ----- context injection -------------------------------------------

    /// A preamble block describing the active change (status `active`,
    /// newest if several), or an empty string when there is none. Injected
    /// at agent-build time so a resumed or fresh session knows which change
    /// it's implementing — and where it left off — without first querying
    /// the `spec` tool. Best-effort: any DB error yields an empty block.
    pub fn format_active_change_for_prompt(&self) -> String {
        let change = match self.list_changes(Some("active")) {
            Ok(mut v) => match v.drain(..).next() {
                Some(c) => c,
                None => return String::new(),
            },
            Err(_) => return String::new(),
        };
        let tasks = self.list_tasks(&change.slug).unwrap_or_default();
        let deltas = self.list_deltas(&change.slug).unwrap_or_default();

        let mut s = String::new();
        s.push_str("\n\n## Active spec change\n");
        let heading = if change.title.is_empty() {
            change.slug.clone()
        } else {
            format!("{} ({})", change.title, change.slug)
        };
        s.push_str(&format!("**{heading}**\n"));
        if !change.why.trim().is_empty() {
            s.push_str(&format!("Why: {}\n", change.why));
        }
        if !change.what.trim().is_empty() {
            s.push_str(&format!("What: {}\n", change.what));
        }
        if !change.design.trim().is_empty() {
            s.push_str(&format!("Design: {}\n", change.design));
        }
        if !deltas.is_empty() {
            let names: Vec<String> = deltas
                .iter()
                .map(|d| format!("{} {}:{}", d.op, d.capability, d.requirement))
                .collect();
            s.push_str(&format!("Requirement deltas: {}\n", names.join("; ")));
        }
        if !tasks.is_empty() {
            let done = tasks.iter().filter(|t| t.status == "done").count();
            s.push_str(&format!("Tasks ({done}/{} done):\n", tasks.len()));
            for t in &tasks {
                let mark = match t.status.as_str() {
                    "done" => "x",
                    "in_progress" => "~",
                    "blocked" => "!",
                    _ => " ",
                };
                s.push_str(&format!(
                    "- [{mark}] {}.{} {} (#{})\n",
                    t.group_no, t.seq, t.text, t.id
                ));
            }
        }
        s.push_str(
            "Update this with the `spec` tool as you work (set_task, add_delta) and `archive` when every task is done.\n",
        );
        s
    }

    // ----- archive (fold deltas into living specs) ---------------------

    /// Archive a change: fold its deltas into the living specs in a single
    /// transaction, then mark it archived. Returns a count of each op
    /// applied. Idempotent only in the sense that archiving an
    /// already-archived change re-applies its deltas — callers should not
    /// re-archive.
    pub fn archive_change(&self, slug: &str) -> Result<ArchiveReport, String> {
        let mut conn = self.conn.lock_ignore_poison();
        let tx = conn
            .transaction()
            .map_err(|e| format!("archive: begin: {e}"))?;
        let cid = Self::change_id(&tx, slug)?;

        let deltas: Vec<Delta> = {
            let mut stmt = tx
                .prepare(
                    "SELECT id, op, capability, requirement, text, scenarios, reason, migration, rename_to
                     FROM spec_deltas WHERE change_id = ?1 ORDER BY id",
                )
                .map_err(|e| format!("archive: load deltas: {e}"))?;
            let rows = stmt
                .query_map(params![cid], row_to_delta)
                .map_err(|e| format!("archive: load deltas: {e}"))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r.map_err(|e| format!("archive: load deltas: {e}"))?);
            }
            v
        };

        let mut report = ArchiveReport::default();
        for d in &deltas {
            match d.op.as_str() {
                "added" | "modified" => {
                    apply_upsert(&tx, d)?;
                    if d.op == "added" {
                        report.added += 1;
                    } else {
                        report.modified += 1;
                    }
                }
                "removed" => {
                    apply_remove(&tx, d)?;
                    report.removed += 1;
                }
                "renamed" => {
                    apply_rename(&tx, d)?;
                    report.renamed += 1;
                }
                other => return Err(format!("archive: unknown op '{other}'")),
            }
        }

        let now = now();
        tx.execute(
            "UPDATE spec_changes SET status = 'archived', archived_at = ?2, updated_at = ?2
             WHERE id = ?1",
            params![cid, now],
        )
        .map_err(|e| format!("archive: mark: {e}"))?;

        tx.commit().map_err(|e| format!("archive: commit: {e}"))?;
        Ok(report)
    }
}

// ----- row mappers + transactional helpers (free fns over &Connection) --

fn row_to_change(row: &rusqlite::Row) -> rusqlite::Result<Change> {
    Ok(Change {
        id: row.get(0)?,
        slug: row.get(1)?,
        title: row.get(2)?,
        why: row.get(3)?,
        what: row.get(4)?,
        design: row.get(5)?,
        status: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        archived_at: row.get(9)?,
    })
}

fn row_to_delta(row: &rusqlite::Row) -> rusqlite::Result<Delta> {
    let scenarios_json: String = row.get(5)?;
    let scenarios: Vec<Scenario> = serde_json::from_str(&scenarios_json).unwrap_or_default();
    Ok(Delta {
        id: row.get(0)?,
        op: row.get(1)?,
        capability: row.get(2)?,
        requirement: row.get(3)?,
        text: row.get(4)?,
        scenarios,
        reason: row.get(6)?,
        migration: row.get(7)?,
        rename_to: row.get(8)?,
    })
}

fn scenarios_for(conn: &Connection, requirement_id: i64) -> Result<Vec<Scenario>, String> {
    let mut stmt = conn
        .prepare("SELECT name, when_then FROM spec_scenarios WHERE requirement_id = ?1 ORDER BY id")
        .map_err(|e| format!("scenarios_for: {e}"))?;
    let rows = stmt
        .query_map(params![requirement_id], |row| {
            Ok(Scenario {
                name: row.get(0)?,
                when_then: row.get(1)?,
            })
        })
        .map_err(|e| format!("scenarios_for: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("scenarios_for: {e}"))?);
    }
    Ok(out)
}

fn ensure_capability(conn: &Connection, name: &str) -> Result<i64, String> {
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM spec_capabilities WHERE name = ?1",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| format!("ensure_capability: {e}"))?
    {
        return Ok(id);
    }
    let now = now();
    conn.execute(
        "INSERT INTO spec_capabilities (name, created_at, updated_at) VALUES (?1, ?2, ?2)",
        params![name, now],
    )
    .map_err(|e| format!("ensure_capability: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// ADDED / MODIFIED: ensure the capability and requirement exist, set the
/// requirement text, and replace its scenarios with the delta's.
fn apply_upsert(conn: &Connection, d: &Delta) -> Result<(), String> {
    let cap_id = ensure_capability(conn, &d.capability)?;
    let now = now();
    conn.execute(
        "INSERT INTO spec_requirements (capability_id, name, text, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(capability_id, name)
         DO UPDATE SET text = excluded.text, updated_at = excluded.updated_at",
        params![cap_id, d.requirement, d.text, now],
    )
    .map_err(|e| format!("apply_upsert: {e}"))?;
    let req_id: i64 = conn
        .query_row(
            "SELECT id FROM spec_requirements WHERE capability_id = ?1 AND name = ?2",
            params![cap_id, d.requirement],
            |r| r.get(0),
        )
        .map_err(|e| format!("apply_upsert: req id: {e}"))?;
    // Replace scenarios wholesale (MODIFIED must carry full content).
    conn.execute(
        "DELETE FROM spec_scenarios WHERE requirement_id = ?1",
        params![req_id],
    )
    .map_err(|e| format!("apply_upsert: clear scenarios: {e}"))?;
    for s in &d.scenarios {
        conn.execute(
            "INSERT INTO spec_scenarios (requirement_id, name, when_then, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![req_id, s.name, s.when_then, now],
        )
        .map_err(|e| format!("apply_upsert: scenario: {e}"))?;
    }
    Ok(())
}

/// REMOVED: drop the requirement (scenarios cascade).
fn apply_remove(conn: &Connection, d: &Delta) -> Result<(), String> {
    conn.execute(
        "DELETE FROM spec_requirements
         WHERE name = ?2 AND capability_id =
            (SELECT id FROM spec_capabilities WHERE name = ?1)",
        params![d.capability, d.requirement],
    )
    .map_err(|e| format!("apply_remove: {e}"))?;
    Ok(())
}

/// RENAMED: change the requirement's name only.
fn apply_rename(conn: &Connection, d: &Delta) -> Result<(), String> {
    if d.rename_to.trim().is_empty() {
        return Err("apply_rename: empty rename_to".to_string());
    }
    conn.execute(
        "UPDATE spec_requirements SET name = ?3, updated_at = ?4
         WHERE name = ?2 AND capability_id =
            (SELECT id FROM spec_capabilities WHERE name = ?1)",
        params![d.capability, d.requirement, d.rename_to, now()],
    )
    .map_err(|e| format!("apply_rename: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Unique temp DB per test (mirrors session_db_tests). Returned dir is
    /// kept alive by the caller binding so it isn't dropped early.
    fn store() -> (SpecStore, std::path::PathBuf) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        // The name has to be unique across RUNS, not just within one. Keyed on
        // `pid-counter` alone it was not: the counter restarts at 0 every run
        // and the OS recycles pids, so a later run could reopen a previous
        // run's `state.db` — already carrying change "c" and its tasks — and
        // fail on counts that look impossible. Nothing deletes these
        // directories either (the returned PathBuf is not a drop guard), so
        // thousands accumulate and the odds of a collision only grow. Stamp
        // with the clock and clear the directory first, as the session-db and
        // memory-db fixtures already do.
        let dir = std::env::temp_dir().join(format!(
            "dirge-specdb-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let s = SpecStore::open_at(&path).unwrap();
        (s, dir)
    }

    fn sc(name: &str, wt: &str) -> Scenario {
        Scenario {
            name: name.to_string(),
            when_then: wt.to_string(),
        }
    }

    #[test]
    fn create_and_fetch_change() {
        let (s, _d) = store();
        s.create_change("add-dark-mode", "Dark mode", "users want it", "add toggle")
            .unwrap();
        let c = s.get_change("add-dark-mode").unwrap().unwrap();
        assert_eq!(c.slug, "add-dark-mode");
        assert_eq!(c.why, "users want it");
        assert_eq!(c.status, "draft");
        assert!(s.get_change("nope").unwrap().is_none());
    }

    #[test]
    fn duplicate_slug_rejected() {
        let (s, _d) = store();
        s.create_change("x", "", "", "").unwrap();
        assert!(s.create_change("x", "", "", "").is_err());
    }

    #[test]
    fn tasks_track_real_status_and_progress() {
        let (s, _d) = store();
        s.create_change("c", "", "", "").unwrap();
        let t1 = s.add_task("c", 1, 1, "first").unwrap();
        let _t2 = s.add_task("c", 1, 2, "second").unwrap();
        assert_eq!(s.task_progress("c").unwrap(), (0, 2));
        s.set_task_status(t1, "done").unwrap();
        assert_eq!(s.task_progress("c").unwrap(), (1, 2));
        let tasks = s.list_tasks("c").unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].status, "done");
        assert!(s.set_task_status(t1, "bogus").is_err());
    }

    #[test]
    fn archive_folds_added_delta_into_living_specs() {
        let (s, _d) = store();
        s.create_change("c", "", "", "").unwrap();
        s.add_delta(
            "c",
            "added",
            "user-auth",
            "User can log in",
            "The system SHALL authenticate users.",
            &[sc("happy path", "WHEN valid creds THEN session starts")],
            "",
            "",
            "",
        )
        .unwrap();
        // Not in living specs until archived.
        assert!(s.list_capabilities().unwrap().is_empty());

        let report = s.archive_change("c").unwrap();
        assert_eq!(report.added, 1);
        assert_eq!(s.list_capabilities().unwrap(), vec!["user-auth"]);
        let reqs = s.capability_requirements("user-auth").unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].name, "User can log in");
        assert_eq!(reqs[0].scenarios.len(), 1);
        assert_eq!(reqs[0].scenarios[0].name, "happy path");
        // Change is now archived.
        assert_eq!(s.get_change("c").unwrap().unwrap().status, "archived");
    }

    #[test]
    fn archive_modified_replaces_text_and_scenarios() {
        let (s, _d) = store();
        // Seed a baseline via a first change.
        s.create_change("c1", "", "", "").unwrap();
        s.add_delta(
            "c1",
            "added",
            "export",
            "Export data",
            "old text",
            &[sc("a", "WHEN x THEN y")],
            "",
            "",
            "",
        )
        .unwrap();
        s.archive_change("c1").unwrap();

        // Second change modifies it.
        s.create_change("c2", "", "", "").unwrap();
        s.add_delta(
            "c2",
            "modified",
            "export",
            "Export data",
            "new text",
            &[sc("b", "WHEN p THEN q"), sc("c", "WHEN r THEN s")],
            "",
            "",
            "",
        )
        .unwrap();
        let report = s.archive_change("c2").unwrap();
        assert_eq!(report.modified, 1);
        let reqs = s.capability_requirements("export").unwrap();
        assert_eq!(reqs.len(), 1, "still one requirement, not duplicated");
        assert_eq!(reqs[0].text, "new text");
        assert_eq!(reqs[0].scenarios.len(), 2, "scenarios replaced wholesale");
    }

    #[test]
    fn archive_removed_and_renamed() {
        let (s, _d) = store();
        s.create_change("c1", "", "", "").unwrap();
        s.add_delta("c1", "added", "cap", "Keep me", "t", &[], "", "", "")
            .unwrap();
        s.add_delta("c1", "added", "cap", "Drop me", "t", &[], "", "", "")
            .unwrap();
        s.archive_change("c1").unwrap();
        assert_eq!(s.capability_requirements("cap").unwrap().len(), 2);

        s.create_change("c2", "", "", "").unwrap();
        s.add_delta(
            "c2",
            "removed",
            "cap",
            "Drop me",
            "",
            &[],
            "obsolete",
            "use X",
            "",
        )
        .unwrap();
        s.add_delta("c2", "renamed", "cap", "Keep me", "", &[], "", "", "Kept")
            .unwrap();
        let report = s.archive_change("c2").unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.renamed, 1);
        let reqs = s.capability_requirements("cap").unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].name, "Kept");
    }

    #[test]
    fn list_changes_filters_by_status() {
        let (s, _d) = store();
        s.create_change("a", "", "", "").unwrap();
        s.create_change("b", "", "", "").unwrap();
        s.set_change_status("b", "active").unwrap();
        assert_eq!(s.list_changes(None).unwrap().len(), 2);
        let active = s.list_changes(Some("active")).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].slug, "b");
    }

    #[test]
    fn active_change_block_is_empty_without_active_change() {
        let (s, _d) = store();
        assert_eq!(s.format_active_change_for_prompt(), "");
        // A draft (not active) change still yields no block.
        s.create_change("c", "", "w", "x").unwrap();
        assert_eq!(s.format_active_change_for_prompt(), "");
    }

    #[test]
    fn active_change_block_summarizes_change_tasks_and_deltas() {
        let (s, _d) = store();
        s.create_change("add-x", "Add X", "need x", "build x")
            .unwrap();
        s.set_change_status("add-x", "active").unwrap();
        s.add_delta(
            "add-x",
            "added",
            "xcap",
            "Do X",
            "SHALL do X",
            &[],
            "",
            "",
            "",
        )
        .unwrap();
        let t = s.add_task("add-x", 1, 1, "wire it").unwrap();
        s.set_task_status(t, "in_progress").unwrap();

        let block = s.format_active_change_for_prompt();
        assert!(block.contains("Active spec change"));
        assert!(block.contains("Add X (add-x)"));
        assert!(block.contains("need x"));
        assert!(block.contains("added xcap:Do X"));
        assert!(block.contains("0/1 done"));
        assert!(
            block.contains("[~] 1.1 wire it"),
            "in_progress marker: {block}"
        );
    }

    #[test]
    fn set_change_field_updates_and_validates() {
        let (s, _d) = store();
        s.create_change("c", "", "", "").unwrap();
        s.set_change_field("c", "design", "the approach").unwrap();
        assert_eq!(s.get_change("c").unwrap().unwrap().design, "the approach");
        assert!(s.set_change_field("c", "bogus", "x").is_err());
        assert!(s.set_change_field("nope", "design", "x").is_err());
    }
}
