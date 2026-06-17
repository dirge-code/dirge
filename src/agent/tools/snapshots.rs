//! File-state snapshots for `/rewind`.
//!
//! Conversation rewind (`ui/search_rewind.rs`) truncates the message
//! history but leaves the *files* the agent edited in their mutated
//! state. This module captures the pre-mutation content of every file
//! a write/edit/edit_lines/apply_patch touches, keyed by the user
//! turn that triggered it, so a rewind can also roll the working tree
//! back — making a long autonomous run safe to unwind (the article's
//! "rewind" lever).
//!
//! Shape, mirroring the global `modified` registry: a process-global
//! store the mutating tools poke at via [`capture`], the UI brackets
//! turns with [`begin_turn`], and the rewind path calls
//! [`restore_from`]. Content is addressed through a small dedup pool
//! (FNV-64 keyed, byte-verified on collision) so a file edited many
//! times across turns doesn't store many copies.
//!
//! In-memory and process-scoped: rewind works within a live session,
//! not across a restart. Persisting objects to the session dir is a
//! follow-up.
//!
//! Why a process-global `static` is the right scope here (dirge-ho0g).
//! dirge runs exactly one interactive/headless session per process, so
//! the global store *is* the session store — there is no second
//! top-level session to collide with. Subagents (the `task` tool) run
//! in-process and deliberately share it: they never call [`begin_turn`]
//! (only the UI does, once per user message — see
//! `ui::begin_snapshot_turn`), so a subagent's edits fold into the
//! parent's open turn bucket and a parent `/rewind` rolls them back
//! along with the parent's own edits. That is the behavior we want — a
//! turn's file changes undo as a unit regardless of which agent made
//! them. Giving each subagent its own store would *break* that
//! (subagent edits would become un-rewindable); a per-session handle
//! threaded through every tool would behave identically to this global
//! in the single-session model, for a much larger surface. So the
//! global stays until a real multi-session-per-process need appears.
//!
//! Captures are atomic under the store mutex with earliest-pre-state-
//! per-path-per-turn, so concurrent subagents can't corrupt a bucket —
//! interleaved captures just resolve to the first pre-state seen for
//! each path, which is exactly the restore target. One acknowledged
//! wrinkle: a *background* subagent that finishes after the parent has
//! moved to a later turn attributes its capture to that later bucket;
//! this is inherent to any shared-session store and is accepted.

#[allow(unused_imports)]
use crate::hash::fnv64;
use crate::sync_util::LockExt;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

/// Largest file we snapshot. Above this we skip capture entirely
/// rather than hold huge blobs in memory; such a file simply won't be
/// rolled back (a documented gap, not a correctness bug).
const MAX_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;

/// Most turn buckets retained. A long run editing across hundreds of
/// turns drops its oldest pre-states rather than growing without
/// bound; rewinding past the retained window restores nothing for the
/// evicted turns.
const MAX_TURNS: usize = 200;

/// What a file looked like before a turn first touched it.
#[derive(Clone)]
enum Capture {
    /// File did not exist — restoring deletes it.
    Absent,
    /// File existed with this content (shared via the dedup pool).
    Content(Arc<Vec<u8>>),
}

struct TurnBucket {
    /// The user-message id that opened this turn.
    turn_id: String,
    /// First-seen pre-state per file this turn (earliest wins).
    captures: IndexMap<PathBuf, Capture>,
}

struct Store {
    turns: Vec<TurnBucket>,
    /// Content-addressed pool: FNV-64(content) → interned bytes.
    /// On a hash hit we verify bytes are equal before reusing.
    pool: std::collections::HashMap<u64, Arc<Vec<u8>>>,
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(|| {
    Mutex::new(Store {
        turns: Vec::new(),
        pool: std::collections::HashMap::new(),
    })
});

/// Open a new turn bucket for `turn_id` (the user message that
/// triggered the agent run). Captures made until the next
/// `begin_turn` are attributed to this turn.
pub fn begin_turn(turn_id: &str) {
    let mut s = STORE.lock_ignore_poison();
    s.turns.push(TurnBucket {
        turn_id: turn_id.to_string(),
        captures: IndexMap::new(),
    });
    // Evict oldest turns past the cap; pool entries they alone
    // referenced drop when their Arcs go.
    while s.turns.len() > MAX_TURNS {
        s.turns.remove(0);
    }
}

/// Intern `bytes` through the dedup pool, returning a shared handle.
/// On an FNV-64 collision with *different* bytes, returns a fresh
/// un-pooled Arc so we never alias distinct content.
fn intern(store: &mut Store, bytes: Vec<u8>) -> Arc<Vec<u8>> {
    let key = fnv64(&bytes);
    if let Some(existing) = store.pool.get(&key) {
        if **existing == bytes {
            return existing.clone();
        }
        // Collision with different content — don't pool, don't clobber.
        return Arc::new(bytes);
    }
    let arc = Arc::new(bytes);
    store.pool.insert(key, arc.clone());
    arc
}

/// Record the current on-disk state of `path` as the pre-mutation
/// snapshot for the active turn, if not already captured this turn.
/// Best-effort: a missing file is recorded as "absent" (restore will
/// delete it); an over-cap file is skipped.
///
/// Call this from a mutating tool *before* writing.
pub fn capture(path: &Path) {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    // Read pre-state before locking the store (I/O outside the lock).
    let capture = match std::fs::metadata(&canonical) {
        Ok(meta) if meta.is_file() => {
            if meta.len() > MAX_SNAPSHOT_BYTES {
                return; // too big to snapshot; leave it un-rewindable
            }
            match std::fs::read(&canonical) {
                Ok(bytes) => Some(bytes),
                Err(_) => return, // unreadable — skip rather than guess
            }
        }
        // Doesn't exist (or isn't a regular file) → absent.
        _ => None,
    };

    let mut s = STORE.lock_ignore_poison();
    // No turn open (e.g. a tool ran before any prompt) → open an
    // anonymous one so the capture isn't lost.
    if s.turns.is_empty() {
        s.turns.push(TurnBucket {
            turn_id: String::new(),
            captures: IndexMap::new(),
        });
    }
    let entry = match capture {
        Some(bytes) => Capture::Content(intern(&mut s, bytes)),
        None => Capture::Absent,
    };
    let last = s.turns.last_mut().expect("just ensured non-empty");
    // Earliest pre-state within a turn wins — don't overwrite.
    if !last.captures.contains_key(&canonical) {
        last.captures.insert(canonical, entry);
    }
}

/// Record `content` as the pre-mutation snapshot for `path` this
/// turn, when the caller already has the file's current bytes in hand
/// (e.g. an edit tool that just read the file to apply its change).
/// Avoids a second read from disk and captures the exact bytes the
/// edit was based on. Use [`capture`] instead when the file may be
/// absent (create) or when the pre-content isn't already available.
pub fn capture_bytes(path: &Path, content: &[u8]) {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if content.len() as u64 > MAX_SNAPSHOT_BYTES {
        return;
    }
    let mut s = STORE.lock_ignore_poison();
    if s.turns.is_empty() {
        s.turns.push(TurnBucket {
            turn_id: String::new(),
            captures: IndexMap::new(),
        });
    }
    let interned = intern(&mut s, content.to_vec());
    let last = s.turns.last_mut().expect("just ensured non-empty");
    if !last.captures.contains_key(&canonical) {
        last.captures.insert(canonical, Capture::Content(interned));
    }
}

/// Roll files back to their pre-state as of `turn_id` and every later
/// turn, then drop those turn buckets. Returns the restored paths.
///
/// For each file, the *earliest* captured pre-state at or after
/// `turn_id` is the restore target (that's the content from before
/// the rewound region began touching it). A file captured as absent
/// is deleted. If `turn_id` isn't in the store, nothing is restored.
pub fn restore_from(turn_id: &str) -> Vec<PathBuf> {
    let mut s = STORE.lock_ignore_poison();
    let idx = match s.turns.iter().position(|t| t.turn_id == turn_id) {
        Some(i) => i,
        None => return Vec::new(),
    };

    // Collect earliest capture per path across buckets [idx..].
    let mut targets: IndexMap<PathBuf, Capture> = IndexMap::new();
    for bucket in &s.turns[idx..] {
        for (path, cap) in &bucket.captures {
            targets.entry(path.clone()).or_insert_with(|| cap.clone());
        }
    }

    let mut restored = Vec::new();
    for (path, cap) in &targets {
        let ok = match cap {
            Capture::Content(bytes) => std::fs::write(path, bytes.as_slice()).is_ok(),
            Capture::Absent => match std::fs::remove_file(path) {
                Ok(_) => true,
                // Already gone is a successful "restore to absent".
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => false,
            },
        };
        if ok {
            restored.push(path.clone());
        }
    }

    // Drop the rewound turns.
    s.turns.truncate(idx);
    restored
}

/// Drop all snapshots (hooked into /clear).
pub fn clear() {
    let mut s = STORE.lock_ignore_poison();
    s.turns.clear();
    s.pool.clear();
}

/// Process-wide gate for tests that touch the global store, so they
/// don't observe each other's turns/objects when run in parallel.
/// Lives at module scope so cross-module tests (e.g. the UI rewind
/// integration test) can serialize against the unit tests here.
///
/// This exists *because* the store is a deliberate process-global (see
/// the module doc / dirge-ho0g): one shared store means parallel tests
/// must serialize. It is the accepted cost of keeping the global, not a
/// smell to refactor away — a per-session store would remove the need
/// for it but at the price of a much larger production surface and a
/// loss of parent-rewindable subagent edits.
#[cfg(test)]
pub(crate) static TEST_GATE: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated<R>(f: impl FnOnce(&Path) -> R) -> R {
        let _g = TEST_GATE.lock_ignore_poison();
        clear();
        let dir = std::env::temp_dir().join(format!("dirge-snap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = f(&dir);
        clear();
        let _ = std::fs::remove_dir_all(&dir);
        r
    }

    #[test]
    fn restore_reverts_edit_to_pre_state() {
        isolated(|dir| {
            let p = dir.join("a.txt");
            std::fs::write(&p, "original").unwrap();

            begin_turn("u1");
            capture(&p); // pre-state = "original"
            std::fs::write(&p, "mutated").unwrap();

            let restored = restore_from("u1");
            assert_eq!(restored.len(), 1);
            assert_eq!(std::fs::read_to_string(&p).unwrap(), "original");
        });
    }

    #[test]
    fn earliest_pre_state_within_turn_wins() {
        isolated(|dir| {
            let p = dir.join("a.txt");
            std::fs::write(&p, "v0").unwrap();
            begin_turn("u1");
            capture(&p); // v0 — this is the one that must restore
            std::fs::write(&p, "v1").unwrap();
            capture(&p); // v1 — ignored (already captured this turn)
            std::fs::write(&p, "v2").unwrap();

            restore_from("u1");
            assert_eq!(std::fs::read_to_string(&p).unwrap(), "v0");
        });
    }

    #[test]
    fn restore_spans_multiple_turns_taking_earliest() {
        isolated(|dir| {
            let p = dir.join("a.txt");
            std::fs::write(&p, "t1pre").unwrap();
            begin_turn("u1");
            capture(&p);
            std::fs::write(&p, "after-t1").unwrap();

            begin_turn("u2");
            capture(&p); // pre = "after-t1"
            std::fs::write(&p, "after-t2").unwrap();

            // Rewinding to u1 undoes BOTH turns → earliest pre-state.
            restore_from("u1");
            assert_eq!(std::fs::read_to_string(&p).unwrap(), "t1pre");
        });
    }

    #[test]
    fn newly_created_file_is_deleted_on_restore() {
        isolated(|dir| {
            let p = dir.join("new.txt");
            begin_turn("u1");
            capture(&p); // file absent
            std::fs::write(&p, "created this turn").unwrap();

            let restored = restore_from("u1");
            assert_eq!(restored.len(), 1);
            assert!(!p.exists(), "file created in the turn must be removed");
        });
    }

    #[test]
    fn rewinding_to_unknown_turn_restores_nothing() {
        isolated(|dir| {
            let p = dir.join("a.txt");
            std::fs::write(&p, "x").unwrap();
            begin_turn("u1");
            capture(&p);
            std::fs::write(&p, "y").unwrap();

            let restored = restore_from("nope");
            assert!(restored.is_empty());
            // Untouched.
            assert_eq!(std::fs::read_to_string(&p).unwrap(), "y");
        });
    }

    #[test]
    fn restore_truncates_rewound_turns() {
        isolated(|dir| {
            let p = dir.join("a.txt");
            std::fs::write(&p, "v0").unwrap();
            begin_turn("u1");
            capture(&p);
            std::fs::write(&p, "v1").unwrap();
            restore_from("u1"); // drops u1

            // A second rewind to u1 now finds nothing.
            std::fs::write(&p, "v2").unwrap();
            let restored = restore_from("u1");
            assert!(restored.is_empty());
            assert_eq!(std::fs::read_to_string(&p).unwrap(), "v2");
        });
    }

    #[test]
    fn capture_bytes_records_pre_state_without_reading_disk() {
        isolated(|dir| {
            let p = dir.join("a.txt");
            // File on disk says "disk", but the caller hands us "inhand"
            // — capture_bytes must record the in-hand bytes (the content
            // the edit was based on), not re-read the file.
            std::fs::write(&p, "disk").unwrap();
            begin_turn("u1");
            capture_bytes(&p, b"inhand");
            std::fs::write(&p, "mutated").unwrap();

            restore_from("u1");
            assert_eq!(std::fs::read_to_string(&p).unwrap(), "inhand");
        });
    }

    #[test]
    fn subagent_edits_fold_into_parent_turn_and_are_parent_rewindable() {
        // Contract lock (dirge-ho0g): subagents run in-process and
        // share this global store. They never open their own turn —
        // only the UI calls begin_turn, once per user message — so an
        // edit made "by a subagent" (i.e. any capture with no nested
        // begin_turn of its own) must attribute to the parent's open
        // turn and be undone by a parent rewind to that turn. This is
        // the intended behavior the global store provides; a
        // per-subagent store would break it.
        isolated(|dir| {
            let parent_file = dir.join("parent.txt");
            let sub_file = dir.join("sub.txt");
            std::fs::write(&parent_file, "p0").unwrap();
            std::fs::write(&sub_file, "s0").unwrap();

            // Parent opens the turn and makes one edit.
            begin_turn("u1");
            capture(&parent_file);
            std::fs::write(&parent_file, "p1").unwrap();

            // A subagent edits a different file during the same turn,
            // WITHOUT opening a turn of its own.
            capture(&sub_file);
            std::fs::write(&sub_file, "s1").unwrap();

            // Parent rewind to u1 rolls back both files.
            let restored = restore_from("u1");
            assert_eq!(restored.len(), 2, "both parent and subagent edits restore");
            assert_eq!(std::fs::read_to_string(&parent_file).unwrap(), "p0");
            assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "s0");
        });
    }

    #[test]
    fn dedup_pool_reuses_identical_content() {
        isolated(|dir| {
            let a = dir.join("a.txt");
            let b = dir.join("b.txt");
            std::fs::write(&a, "same").unwrap();
            std::fs::write(&b, "same").unwrap();
            begin_turn("u1");
            capture(&a);
            capture(&b);
            // Both captures should share one pooled object.
            let s = STORE.lock_ignore_poison();
            assert_eq!(
                s.pool.len(),
                1,
                "identical content must dedup to one object"
            );
        });
    }
}
