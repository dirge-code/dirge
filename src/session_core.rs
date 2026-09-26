//! Minimal, wasm-able session persistence.
//!
//! The native session model (`crate::session`) is entangled with the
//! agent/provider graph (`AssetId`, `ThinkingLevel`, `TodoItem`, `TokenUsage`)
//! and cannot compile for `wasm32-unknown-unknown`. This module is the
//! pared-down equivalent: a flat in-memory session store with the same
//! create / append / load / list / delete contract the terminal app uses,
//! JSON-serializable so a browser/Node host can render and inspect it.
//!
//! `MemorySessionStore` is pure Rust (no `wasm_bindgen`), so its tests run
//! natively via `cargo test --lib`. The wasm-bindgen wrapper that exposes it
//! to JS lives in `src/lib.rs` under the `wasm` feature.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// One message in a stored session. `role` is the free-form role string the
/// host supplied ("user" / "assistant" / "system" in the terminal path).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
}

/// A flat, JSON-serializable session. `updated_seq` is a store-assigned
/// monotonic counter used for deterministic newest-first ordering; the
/// millisecond timestamps are display-only and can tie on fast machines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSession {
    pub id: String,
    pub name: String,
    pub messages: Vec<StoredMessage>,
    pub created_at: i64,
    pub updated_at: i64,
    pub updated_seq: u64,
}

/// In-process session backend keyed by id. Dependency-free apart from
/// `uuid`/`serde`, so it compiles for both the native lib and the wasm slice.
#[derive(Default)]
pub struct MemorySessionStore {
    sessions: Mutex<HashMap<String, StoredSession>>,
    next_seq: AtomicU64,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new session and return its id.
    pub fn create(&self, name: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_millis();
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        self.sessions
            .lock()
            .expect("session store poisoned")
            .insert(
                id.clone(),
                StoredSession {
                    id: id.clone(),
                    name: name.to_string(),
                    messages: Vec::new(),
                    created_at: now,
                    updated_at: now,
                    updated_seq: seq,
                },
            );
        id
    }

    /// Append a message to an existing session, bumping its recency.
    pub fn append_message(&self, id: &str, role: &str, content: &str) -> anyhow::Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("session store poisoned"))?;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("no session {id} in memory store"))?;
        session.messages.push(StoredMessage {
            role: role.to_string(),
            content: content.to_string(),
        });
        session.updated_at = now_millis();
        session.updated_seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Load one session by id.
    pub fn get(&self, id: &str) -> anyhow::Result<StoredSession> {
        self.sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("session store poisoned"))?
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no session {id} in memory store"))
    }

    /// List all sessions, newest first (by `updated_seq`).
    pub fn list(&self) -> Vec<StoredSession> {
        let mut sessions: Vec<StoredSession> = self
            .sessions
            .lock()
            .expect("session store poisoned")
            .values()
            .cloned()
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_seq));
        sessions
    }

    /// Delete a session. Idempotent: deleting a missing id is a no-op.
    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        self.sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("session store poisoned"))?
            .remove(id);
        Ok(())
    }
}

fn now_millis() -> i64 {
    #[cfg(all(feature = "wasm", target_arch = "wasm32"))]
    {
        js_sys::Date::now() as i64
    }
    #[cfg(not(all(feature = "wasm", target_arch = "wasm32")))]
    {
        chrono::Utc::now().timestamp_millis()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_returns_unique_ids_with_no_messages() {
        let store = MemorySessionStore::new();
        let a = store.create("first");
        let b = store.create("second");
        assert_ne!(a, b);
        let sa = store.get(&a).unwrap();
        assert_eq!(sa.name, "first");
        assert!(sa.messages.is_empty());
    }

    #[test]
    fn append_and_get_round_trips_in_order() {
        let store = MemorySessionStore::new();
        let id = store.create("session");
        store.append_message(&id, "user", "hello").unwrap();
        store.append_message(&id, "assistant", "hi there").unwrap();
        let s = store.get(&id).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, "user");
        assert_eq!(s.messages[0].content, "hello");
        assert_eq!(s.messages[1].role, "assistant");
        assert_eq!(s.messages[1].content, "hi there");
    }

    #[test]
    fn get_unknown_id_errors() {
        assert!(MemorySessionStore::new().get("nope").is_err());
    }

    #[test]
    fn append_unknown_id_errors() {
        assert!(
            MemorySessionStore::new()
                .append_message("nope", "user", "hi")
                .is_err()
        );
    }

    #[test]
    fn list_orders_newest_first() {
        let store = MemorySessionStore::new();
        let older = store.create("older");
        let newer = store.create("newer");
        store.append_message(&older, "user", "touch").unwrap(); // bumps older above newer
        let list = store.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, older);
        assert_eq!(list[1].id, newer);
    }

    #[test]
    fn delete_removes_and_is_idempotent() {
        let store = MemorySessionStore::new();
        let id = store.create("doomed");
        store.delete(&id).unwrap();
        assert!(store.get(&id).is_err());
        store.delete(&id).unwrap(); // no-op
    }
}
