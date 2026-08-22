//! SQLite store for vigil heartbeat/wakeup configurations.
//!
//! Vigil entries live in the per-project session DB (`.dirge/sessions/state.db`).
//! The store owns its schema via idempotent `CREATE TABLE IF NOT EXISTS` on open.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

/// Lifecycle states for a vigil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VigilStatus {
    Active,
    Paused,
    Resting,
}

impl VigilStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            VigilStatus::Active => "active",
            VigilStatus::Paused => "paused",
            VigilStatus::Resting => "resting",
        }
    }
}

/// A stored vigil row.
pub struct VigilRow {
    pub name: String,
    pub payload_json: String,
    pub status: VigilStatus,
    pub created_at: String,
    pub updated_at: String,
}

/// SQLite-backed vigil store.
pub struct VigilStore {
    conn: Mutex<Connection>,
}

impl VigilStore {
    pub fn open(paths: &super::dirge_paths::ProjectPaths) -> Result<Self, String> {
        Self::open_at(&paths.session_db_path())
    }

    pub fn open_at(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|e| format!("open vigil db at {}: {e}", path.display()))?;
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.ensure_schema()?;
        Ok(store)
    }

    fn ensure_schema(&self) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS vigils (
                name        TEXT PRIMARY KEY NOT NULL,
                payload_json TEXT NOT NULL,
                status      TEXT NOT NULL DEFAULT 'active',
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_vigils_status ON vigils(status);",
        )
        .map_err(|e| format!("create vigils table: {e}"))
    }

    pub fn upsert(&self, name: &str, payload_json: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO vigils (name, payload_json, status, updated_at)
             VALUES (?1, ?2, 'active', datetime('now'))
             ON CONFLICT(name) DO UPDATE SET
                 payload_json = excluded.payload_json,
                 status = 'active',
                 updated_at = datetime('now')",
            params![name, payload_json],
        )
        .map_err(|e| format!("upsert vigil {name}: {e}"))?;
        Ok(())
    }

    pub fn set_status(&self, name: &str, status: VigilStatus) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let affected = conn
            .execute(
                "UPDATE vigils SET status = ?1, updated_at = datetime('now') WHERE name = ?2",
                params![status.as_str(), name],
            )
            .map_err(|e| format!("set status for vigil {name}: {e}"))?;
        if affected == 0 {
            return Err(format!("vigil {name} not found"));
        }
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let affected = conn
            .execute("DELETE FROM vigils WHERE name = ?1", params![name])
            .map_err(|e| format!("remove vigil {name}: {e}"))?;
        if affected == 0 {
            return Err(format!("vigil {name} not found"));
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<Option<VigilRow>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name, payload_json, status, created_at, updated_at
                 FROM vigils WHERE name = ?1",
            )
            .map_err(|e| format!("prepare get vigil {name}: {e}"))?;
        let row = stmt
            .query_row(params![name], |row| {
                Ok(VigilRow {
                    name: row.get(0)?,
                    payload_json: row.get(1)?,
                    status: {
                        let s: String = row.get(2)?;
                        match s.as_str() {
                            "active" => VigilStatus::Active,
                            "paused" => VigilStatus::Paused,
                            "resting" => VigilStatus::Resting,
                            _ => VigilStatus::Active,
                        }
                    },
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })
            .optional()
            .map_err(|e| format!("get vigil {name}: {e}"))?;
        Ok(row)
    }

    pub fn list_non_resting(&self) -> Result<Vec<VigilRow>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name, payload_json, status, created_at, updated_at
                 FROM vigils WHERE status != 'resting' ORDER BY name",
            )
            .map_err(|e| format!("prepare list_non_resting: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(VigilRow {
                    name: row.get(0)?,
                    payload_json: row.get(1)?,
                    status: {
                        let s: String = row.get(2)?;
                        match s.as_str() {
                            "active" => VigilStatus::Active,
                            "paused" => VigilStatus::Paused,
                            "resting" => VigilStatus::Resting,
                            _ => VigilStatus::Active,
                        }
                    },
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })
            .map_err(|e| format!("list_non_resting: {e}"))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_db() -> (VigilStore, std::path::PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("dirge-vigildb-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = VigilStore::open_at(&dir.join("state.db")).unwrap();
        (store, dir)
    }

    #[test]
    fn upsert_then_get_roundtrips_payload() {
        let (store, _dir) = temp_db();
        store.upsert("poll", "{\"name\":\"poll\"}").unwrap();
        let row = store.get("poll").unwrap().expect("row exists");
        assert_eq!(row.name, "poll");
        assert_eq!(row.payload_json, "{\"name\":\"poll\"}");
        assert_eq!(row.status, VigilStatus::Active);
    }

    #[test]
    fn upsert_resets_status_to_active() {
        let (store, _dir) = temp_db();
        store.upsert("poll", "v1").unwrap();
        store.set_status("poll", VigilStatus::Paused).unwrap();
        store.upsert("poll", "v2").unwrap();
        let row = store.get("poll").unwrap().unwrap();
        assert_eq!(row.status, VigilStatus::Active);
        assert_eq!(row.payload_json, "v2");
    }

    #[test]
    fn list_non_resting_excludes_resting() {
        let (store, _dir) = temp_db();
        store.upsert("a", "1").unwrap();
        store.upsert("b", "2").unwrap();
        store.upsert("c", "3").unwrap();
        store.set_status("b", VigilStatus::Resting).unwrap();
        let names: Vec<String> = store
            .list_non_resting()
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn remove_deletes_row() {
        let (store, _dir) = temp_db();
        store.upsert("poll", "1").unwrap();
        store.remove("poll").unwrap();
        assert!(store.get("poll").unwrap().is_none());
    }

    #[test]
    fn status_and_remove_on_missing_name_error() {
        let (store, _dir) = temp_db();
        assert!(store.set_status("nope", VigilStatus::Paused).is_err());
        assert!(store.remove("nope").is_err());
    }
}
