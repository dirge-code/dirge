//! Swappable session persistence.
//!
//! [`SessionStore`] is the seam between dirge's [`Session`] model and
//! wherever sessions actually live. Two backends ship today:
//!
//! - [`JsonFileStore`] — the native backend, delegating to
//!   [`super::storage`] (one JSON file per session under the data dir).
//! - [`MemoryStore`] — a dependency-free, process-local backend used for
//!   tests and as the seed for a browser/wasm backend (a wasm SQLite or
//!   IndexedDB/OPFS impl is the next drop-in once the `Session` type itself
//!   compiles for wasm — it currently references the agent/provider graph).
//!
//! A new backend is interchangeable the moment it passes
//! [`assert_round_trip`]: implement the five methods and the shared
//! assertion proves it behaves like the file store for the core
//! save/load/delete/list contract.

use std::collections::HashMap;
use std::sync::Mutex;

use super::Session;

/// The persistence contract every session backend implements.
///
/// Kept to the five operations the native app and a browser session both
/// need; higher-level helpers (`load_session_tip`,
/// `recent_project_sessions`) build on `load` + listing and stay in
/// [`super::storage`] for now.
#[allow(dead_code)] // seam: consumed by the agent loop + wasm backend next slice; tests exercise it now
pub trait SessionStore {
    fn save(&self, session: &mut Session) -> anyhow::Result<()>;
    fn load(&self, id: &str) -> anyhow::Result<Session>;
    fn delete(&self, id: &str) -> anyhow::Result<()>;
    fn find_recent(&self, limit: usize) -> anyhow::Result<Vec<Session>>;
    fn find_by_prefix(&self, prefix: &str) -> anyhow::Result<Vec<Session>>;
}

/// Native backend: one JSON file per session in the data dir. Delegates
/// verbatim to the existing [`super::storage`] free functions so every
/// behaviour (schema migration, conflict diversion, atomic writes, asset
/// cleanup) is preserved with zero duplication.
#[allow(dead_code)] // part of the storage seam (see trait)
pub struct JsonFileStore;

impl SessionStore for JsonFileStore {
    fn save(&self, session: &mut Session) -> anyhow::Result<()> {
        super::storage::save_session(session)
    }

    fn load(&self, id: &str) -> anyhow::Result<Session> {
        super::storage::load_session(id)
    }

    fn delete(&self, id: &str) -> anyhow::Result<()> {
        super::storage::delete_session(id)
    }

    fn find_recent(&self, limit: usize) -> anyhow::Result<Vec<Session>> {
        super::storage::find_recent_sessions(limit)
    }

    fn find_by_prefix(&self, prefix: &str) -> anyhow::Result<Vec<Session>> {
        super::storage::find_sessions_by_prefix(prefix)
    }
}

/// In-process backend keyed by session id. Ordering for `find_recent` and
/// `find_by_prefix` matches the file store (newest `updated_at` first).
/// Used for tests and as the minimal wasm seed.
#[allow(dead_code)] // part of the storage seam (see trait)
#[derive(Default)]
pub struct MemoryStore {
    sessions: Mutex<HashMap<String, Session>>,
}

#[allow(dead_code)] // part of the storage seam (see trait)
impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self, id: &str) -> anyhow::Result<Session> {
        self.sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("memory store poisoned"))?
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no session {id} in memory store"))
    }
}

impl SessionStore for MemoryStore {
    fn save(&self, session: &mut Session) -> anyhow::Result<()> {
        self.sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("memory store poisoned"))?
            .insert(session.id.to_string(), session.clone());
        Ok(())
    }

    fn load(&self, id: &str) -> anyhow::Result<Session> {
        self.read(id)
    }

    fn delete(&self, id: &str) -> anyhow::Result<()> {
        self.sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("memory store poisoned"))?
            .remove(id);
        Ok(())
    }

    fn find_recent(&self, limit: usize) -> anyhow::Result<Vec<Session>> {
        let mut sessions: Vec<Session> = self
            .sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("memory store poisoned"))?
            .values()
            .cloned()
            .collect();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        sessions.truncate(limit);
        Ok(sessions)
    }

    fn find_by_prefix(&self, prefix: &str) -> anyhow::Result<Vec<Session>> {
        if prefix.is_empty() {
            anyhow::bail!("session prefix must not be empty");
        }
        let mut sessions: Vec<Session> = self
            .sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("memory store poisoned"))?
            .values()
            .filter(|s| s.id.as_str().starts_with(prefix))
            .cloned()
            .collect();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::MessageRole;

    fn sample(updated_at: &str) -> Session {
        let mut s = Session::new("test", "m", 128_000);
        s.name = "fixture".into();
        s.updated_at = updated_at.into();
        s.add_message(MessageRole::User, "hello");
        s
    }

    /// The shared backend-equivalence assertion. Every `SessionStore` impl
    /// must pass this; run it from a per-backend test to prove a new
    /// backend is drop-in.
    fn assert_round_trip(store: &dyn SessionStore) {
        let mut s = sample("2024-01-01T00:00:00Z");
        let id = s.id.to_string();
        store.save(&mut s).unwrap();

        let loaded = store.load(&id).unwrap();
        assert_eq!(loaded.id, s.id);
        assert_eq!(loaded.name, s.name);
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].role, MessageRole::User);
        assert_eq!(loaded.messages[0].content.as_str(), "hello");
    }

    #[test]
    fn memory_store_round_trips() {
        assert_round_trip(&MemoryStore::new());
    }

    #[test]
    fn json_file_store_round_trips() {
        assert_round_trip(&JsonFileStore);
    }

    #[test]
    fn memory_store_delete_removes() {
        let store = MemoryStore::new();
        let mut s = sample("2024-01-01T00:00:00Z");
        let id = s.id.to_string();
        store.save(&mut s).unwrap();
        store.delete(&id).unwrap();
        assert!(store.load(&id).is_err());
    }

    #[test]
    fn memory_store_recent_orders_newest_first() {
        let store = MemoryStore::new();
        let mut older = sample("2024-01-01T00:00:00Z");
        let mut newer = sample("2024-02-01T00:00:00Z");
        store.save(&mut older).unwrap();
        store.save(&mut newer).unwrap();

        let recent = store.find_recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, newer.id);
        assert_eq!(recent[1].id, older.id);
    }

    #[test]
    fn memory_store_find_by_prefix_matches_and_orders() {
        let store = MemoryStore::new();
        let mut a = sample("2024-01-01T00:00:00Z");
        a.id = "abc-1".into();
        let mut b = sample("2024-02-01T00:00:00Z");
        b.id = "abc-2".into();
        let mut c = sample("2024-03-01T00:00:00Z");
        c.id = "xyz-1".into();
        store.save(&mut a).unwrap();
        store.save(&mut b).unwrap();
        store.save(&mut c).unwrap();

        let found = store.find_by_prefix("abc").unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id.as_str(), "abc-2");
        assert_eq!(found[1].id.as_str(), "abc-1");
    }

    #[test]
    fn memory_store_rejects_empty_prefix() {
        assert!(MemoryStore::new().find_by_prefix("").is_err());
    }
}
