use super::*;

use std::sync::atomic::{AtomicU32, Ordering};

static DB_COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_db() -> (SessionDb, std::path::PathBuf) {
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "dirge-session-db-test-{}-{}",
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");
    let db = SessionDb::open(&path).unwrap();
    (db, dir)
}

/// PR #392 CI failure: several components (session persistence,
/// memory store, session search) open the same state.db, and tests
/// build them in parallel. On a FRESH file, concurrent first opens
/// raced the migration chain — both connections read user_version=0
/// and both ran v1's CREATE TABLE, so the loser errored out and its
/// `SqliteMemoryStore::load` returned None. Migrations must be
/// serialized: every concurrent open of a fresh DB succeeds.
#[test]
fn concurrent_first_opens_all_succeed() {
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "dirge-session-db-race-{}-{}",
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let p = path.clone();
            std::thread::spawn(move || SessionDb::open(&p).map(|_| ()))
        })
        .collect();
    let results: Vec<Result<(), String>> = handles
        .into_iter()
        .map(|h| h.join().expect("thread must not panic"))
        .collect();
    for (i, r) in results.iter().enumerate() {
        assert!(r.is_ok(), "concurrent open {i} failed: {r:?}");
    }

    // The DB ends up fully migrated exactly once.
    let db = SessionDb::open(&path).unwrap();
    let ver: u32 = db
        .conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(ver, SCHEMA_VERSION);
    let _ = std::fs::remove_dir_all(&dir);
}

/// dirge-slj2: post-v6 the messages_fts index holds a REDACTED,
/// CONCATENATED projection, but the v1/v2 messages_ad trigger issued
/// the external-content FTS5 'delete' command with raw old.content —
/// mismatched values corrupt the index. v8 must drop the trigger.
/// The trigram delete trigger targets a STANDALONE fts table with
/// plain DML and is correct — it must survive.
#[test]
fn schema_v8_drops_the_stale_fts_delete_trigger() {
    let (db, _dir) = temp_db();
    let ad: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='messages_ad'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ad, 0, "messages_ad must be dropped by v8");
    let trigram: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='messages_fts_trigram_delete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(trigram, 1, "the correct trigram delete trigger stays");
}

/// dirge-lerb + dirge-fa10: v9 dropped the dead `memories.confidence`
/// column (and its constant data); v13 later RE-introduced confidence
/// as a live, read column. So across the full chain a pre-v9 DB ends up
/// with a confidence column again — but holding the v13 DEFAULT, proving
/// v9 really discarded the old column and its values rather than
/// preserving them. Rows survive the drop-then-re-add.
#[test]
fn schema_v9_drops_confidence_column() {
    // Fresh DB runs every migration including v13, so confidence is
    // present again (it was absent only between v9 and v13).
    let (db, _dir) = temp_db();
    let present: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'confidence'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(present, 1, "v13 re-adds confidence after v9 dropped it");

    // Simulate a pre-v9 DB: a memories table WITH a HIGH confidence
    // value + a row, user_version pinned to 8, then reopen so migrate()
    // runs v9 (drop) … v13 (re-add).
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("dirge-v9-migrate-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, uid TEXT NOT NULL UNIQUE,
                 target TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'procedural',
                 content TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'active',
                 tier TEXT NOT NULL DEFAULT 'hot', confidence REAL NOT NULL DEFAULT 0.6,
                 salience REAL NOT NULL DEFAULT 0.5, created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL, last_used_at TEXT,
                 use_count INTEGER NOT NULL DEFAULT 0, superseded_by TEXT
             );
             INSERT INTO memories (uid, target, content, confidence, created_at, updated_at)
                 VALUES ('urn:ump:keep', 'memory', 'survives the drop', 0.99, 'x', 'x');
             PRAGMA user_version = 8;",
        )
        .unwrap();
    }
    let db = SessionDb::open(&path).unwrap();
    let (present, confidence): (i64, f64) = db
        .conn
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'confidence'),
                 (SELECT confidence FROM memories WHERE uid = 'urn:ump:keep')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(present, 1, "confidence column present again after v13");
    assert!(
        (confidence - 0.6).abs() < 1e-9,
        "the old 0.99 value was discarded by v9; v13 re-adds at the 0.6 default: {confidence}",
    );
    let kept: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE uid = 'urn:ump:keep'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1, "existing rows survive the drop-then-re-add");
    let _ = std::fs::remove_dir_all(&dir);
}

/// dirge-slj2: the safe delete path. delete_session_messages must remove each
/// row's exact indexed projection (the redacted + concatenated text
/// insert_message wrote) from the FTS5 index, leaving zero ghosts in either
/// index and other sessions untouched.
#[test]
fn delete_session_messages_cleans_both_fts_indexes() {
    let (db, _dir) = temp_db();
    db.insert_session("s1", "cli", "gpt-5", "openai", "2026-01-01T10:00:00Z")
        .unwrap();
    // Projection differs from raw content two ways: the bearer token
    // is redacted at index time, and tool_name is concatenated in.
    db.insert_message(
        "s1",
        "assistant",
        "Authorization: Bearer supersecret123 zebraword",
        Some("uniquetool"),
        None,
        None,
        "2026-01-01T10:01:00Z",
    )
    .unwrap();
    db.insert_session("s2", "cli", "gpt-5", "openai", "2026-01-01T11:00:00Z")
        .unwrap();
    db.insert_message(
        "s2",
        "user",
        "zebraword survives in the other session",
        None,
        None,
        None,
        "2026-01-01T11:01:00Z",
    )
    .unwrap();

    let deleted = db.delete_session_messages("s1").unwrap();
    assert_eq!(deleted, 1);

    // No ghosts in either index for the deleted session's tokens.
    let fts_ghosts: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'uniquetool'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_ghosts, 0, "unicode61 index must be clean");
    let trigram_ghosts: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH 'uniquetool'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(trigram_ghosts, 0, "trigram index must be clean");

    // The other session's content still searches fine.
    let results = db.search_messages("zebraword", None).unwrap();
    assert_eq!(results.len(), 1, "other session must stay searchable");
    assert_eq!(results[0].session_id, "s2");

    // Rows gone, count reset.
    let remaining: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = 's1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);
    let count: i64 = db
        .conn
        .query_row(
            "SELECT message_count FROM sessions WHERE id = 's1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "session message_count must reset");
}

/// dirge-aceg: the FTS5 'delete' must use the text that was INDEXED, not a
/// fresh recomputation from the raw row — the redactor evolves, so recomputing
/// a row indexed under an older redactor yields a different projection and the
/// 'delete' corrupts the index instead of cleaning it.
///
/// Simulate that drift by mutating the raw `messages.content` AFTER indexing
/// (no trigger updates the FTS — insert_message owns that path). A recompute
/// would now 'delete' the wrong tokens and leave the real entry behind;
/// reading the stored projection back deletes it cleanly.
#[test]
fn delete_uses_indexed_projection_not_recomputation() {
    let (db, _dir) = temp_db();
    db.insert_session("s1", "cli", "gpt-5", "openai", "2026-01-01T10:00:00Z")
        .unwrap();
    db.insert_message(
        "s1",
        "assistant",
        "alphaindexed betaindexed",
        None,
        None,
        None,
        "2026-01-01T10:01:00Z",
    )
    .unwrap();

    // Drift: the raw row now reads different text than what was indexed. The
    // FTS tables still hold "alphaindexed betaindexed".
    db.conn
        .execute(
            "UPDATE messages SET content = 'totallydifferent gammaword' WHERE session_id = 's1'",
            [],
        )
        .unwrap();

    let deleted = db.delete_session_messages("s1").unwrap();
    assert_eq!(deleted, 1);

    // The INDEXED token must be gone — proving 'delete' targeted the stored
    // projection, not the drifted raw text (which a recompute would have used,
    // missing this entry and corrupting the index).
    let ghosts: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'alphaindexed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        ghosts, 0,
        "indexed token survived — recompute-based delete missed it"
    );
}

#[test]
fn create_and_read_session() {
    let (db, _dir) = temp_db();
    db.insert_session(
        "sess-1",
        "cli",
        "claude-opus",
        "anthropic",
        "2025-01-15T10:00:00Z",
    )
    .unwrap();

    let count: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE id = 'sess-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn insert_message_and_fts5_search() {
    let (db, _dir) = temp_db();
    db.insert_session(
        "sess-1",
        "cli",
        "claude-opus",
        "anthropic",
        "2025-01-15T10:00:00Z",
    )
    .unwrap();

    db.insert_message(
        "sess-1",
        "user",
        "how do we handle database migrations",
        None,
        None,
        None,
        "2025-01-15T10:01:00Z",
    )
    .unwrap();

    let results = db.search_messages("database migrations", None).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("database migrations"));
}

/// dirge-f67u regression guard: insert_message must commit all four of
/// its effects (messages row, messages_fts projection,
/// messages_fts_trigram projection, sessions.message_count bump) as a
/// single atomic unit. A partial write leaves a message row with no FTS
/// projection (invisible to search) and message_count drift, and later
/// corrupts the external-content FTS5 index on delete. We can't inject a
/// deterministic mid-transaction fault, so this asserts the committed
/// end-state: all four effects present and consistent.
#[test]
fn insert_message_commits_all_four_effects() {
    let (db, _dir) = temp_db();
    db.insert_session(
        "sess-1",
        "cli",
        "claude-opus",
        "anthropic",
        "2025-01-15T10:00:00Z",
    )
    .unwrap();

    let before: i64 = db
        .conn
        .query_row(
            "SELECT message_count FROM sessions WHERE id = 'sess-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, 0);

    let row_id = db
        .insert_message(
            "sess-1",
            "user",
            "how do we handle database migrations",
            None,
            None,
            None,
            "2025-01-15T10:01:00Z",
        )
        .unwrap();

    // 1. The messages row exists at the returned id, content intact.
    let (stored_id, stored_content): (i64, String) = db
        .conn
        .query_row(
            "SELECT id, content FROM messages WHERE session_id = 'sess-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored_id, row_id);
    assert_eq!(stored_content, "how do we handle database migrations");

    // 2. Findable via the fts search path.
    let fts_hits = db.search_messages("database migrations", None).unwrap();
    assert_eq!(fts_hits.len(), 1, "fts search must find the message");
    assert_eq!(fts_hits[0].id, row_id);

    // 3. Findable via the trigram search path.
    let trigram_hits = db
        .search_messages_trigram("database migrations", None)
        .unwrap();
    assert_eq!(
        trigram_hits.len(),
        1,
        "trigram search must find the message"
    );
    assert_eq!(trigram_hits[0].id, row_id);

    // 4. session.message_count incremented by exactly 1.
    let after: i64 = db
        .conn
        .query_row(
            "SELECT message_count FROM sessions WHERE id = 'sess-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        after,
        before + 1,
        "message_count must increment by exactly 1"
    );
}

#[test]
fn list_sessions_returns_recent() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();
    db.insert_session(
        "sess-2",
        "subagent",
        "claude-sonnet",
        "anthropic",
        "2025-01-15T11:00:00Z",
    )
    .unwrap();

    let sessions = db.list_sessions_rich(None).unwrap();
    assert_eq!(sessions.len(), 2);
    // Most recent first.
    assert_eq!(sessions[0].id, "sess-2");
    assert_eq!(sessions[1].id, "sess-1");
}

#[test]
fn list_sessions_excludes_source() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();
    db.insert_session(
        "sess-2",
        "review-fork",
        "claude-sonnet",
        "anthropic",
        "2025-01-15T11:00:00Z",
    )
    .unwrap();

    let sessions = db.list_sessions_rich(Some(&["review-fork"])).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "sess-1");
}

#[test]
fn session_split_parent_chain() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    // Split: child session points to parent.
    db.insert_session("sess-2", "cli", "gpt-5", "openai", "2025-01-15T11:00:00Z")
        .unwrap();
    db.set_parent_session("sess-2", "sess-1").unwrap();

    let parent: String = db
        .conn
        .query_row(
            "SELECT parent_session_id FROM sessions WHERE id = 'sess-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(parent, "sess-1");
}

#[test]
fn fts5_search_with_role_filter() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    db.insert_message(
        "sess-1",
        "user",
        "how do we build this",
        None,
        None,
        None,
        "2025-01-15T10:01:00Z",
    )
    .unwrap();
    db.insert_message(
        "sess-1",
        "assistant",
        "run cargo build",
        None,
        None,
        None,
        "2025-01-15T10:02:00Z",
    )
    .unwrap();

    let results = db.search_messages("build", Some("user")).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].role, "user");
}

#[test]
fn anchored_view_returns_window_around_match() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    // Insert 10 messages.
    for i in 0..10 {
        db.insert_message(
            "sess-1",
            if i % 2 == 0 { "user" } else { "assistant" },
            &format!("message {}", i),
            None,
            None,
            None,
            &format!("2025-01-15T10:{:02}:00Z", i),
        )
        .unwrap();
    }

    // Anchor on message 5.
    let view = db.get_anchored_view("sess-1", 5, 2).unwrap();

    // Window should have 5 messages: anchor + 2 before + 2 after.
    assert_eq!(view.messages.len(), 5);
    assert_eq!(view.anchor_index, 2);
    assert_eq!(view.before, 2);
    assert_eq!(view.after, 2);
}

#[test]
fn resolve_parent_walks_lineage() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();
    db.insert_session("sess-2", "cli", "gpt-5", "openai", "2025-01-15T11:00:00Z")
        .unwrap();
    db.insert_session("sess-3", "cli", "gpt-5", "openai", "2025-01-15T12:00:00Z")
        .unwrap();

    db.set_parent_session("sess-2", "sess-1").unwrap();
    db.set_parent_session("sess-3", "sess-2").unwrap();

    assert_eq!(db.resolve_parent("sess-3").unwrap(), "sess-1");
    assert_eq!(db.resolve_parent("sess-2").unwrap(), "sess-1");
    assert_eq!(db.resolve_parent("sess-1").unwrap(), "sess-1");
}

#[test]
fn fold_chain_resolves_through_canonical_db_ids() {
    // dirge-g1ze: turns persist under `db_session_id(session.id)` and the
    // fold handler inserts/links the rotated session under the SAME
    // derivation. A message written after the fold and the parent link
    // must therefore land on the same row, so lineage walks back to root.
    use crate::text::db_session_id;
    let (db, _dir) = temp_db();

    // Original session; a turn is persisted under its canonical id.
    let orig = "abcd1234ef567890"; // a plain uuid-like runtime id
    let orig_db = db_session_id(orig);
    db.insert_session(&orig_db, "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    // Two successive folds rotate to distinct `compacted-<hex>` ids.
    let fold1 = "compacted-11112222";
    let fold2 = "compacted-33334444";
    let fold1_db = db_session_id(fold1);
    let fold2_db = db_session_id(fold2);

    // Distinct folds must not collapse to one row.
    assert_ne!(fold1_db, fold2_db, "folds must key distinct rows");

    // Fold handler: insert new, link to parent (canonical ids throughout).
    db.insert_session(&fold1_db, "cli", "gpt-5", "openai", "2025-01-15T11:00:00Z")
        .unwrap();
    db.set_parent_session(&fold1_db, &orig_db).unwrap();
    db.insert_session(&fold2_db, "cli", "gpt-5", "openai", "2025-01-15T12:00:00Z")
        .unwrap();
    db.set_parent_session(&fold2_db, &fold1_db).unwrap();

    // A turn persisted AFTER the second fold uses the same derivation the
    // fold handler inserted under — so it hits an existing, linked row.
    db.insert_message(
        &db_session_id(fold2),
        "user",
        "post-fold turn",
        None,
        None,
        None,
        "2025-01-15T12:01:00Z",
    )
    .unwrap();

    // Lineage walks all the way back to the original session.
    assert_eq!(db.resolve_parent(&fold2_db).unwrap(), orig_db);
    assert_eq!(db.resolve_parent(&fold1_db).unwrap(), orig_db);
}

#[test]
fn fts5_search_finds_tool_names() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    // Insert an assistant message that used the `read` tool.
    db.insert_message(
        "sess-1",
        "assistant",
        "Let me read that file.",
        Some("read"),
        Some(r#"[{"name":"read","args":{"path":"/tmp/x"}}]"#),
        None,
        "2025-01-15T10:02:00Z",
    )
    .unwrap();

    // Insert a user message (no tool).
    db.insert_message(
        "sess-1",
        "user",
        "show me the build output",
        None,
        None,
        None,
        "2025-01-15T10:01:00Z",
    )
    .unwrap();

    // Searching for "read" (the tool name) should find the assistant message.
    let results = db.search_messages("read", None).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].role, "assistant");

    // Searching for "build" should find the user message.
    let results = db.search_messages("build", None).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].role, "user");
}

#[test]
fn trigram_fts5_indexes_and_searches() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    // Insert a message with tool_name populated.
    db.insert_message(
        "sess-1",
        "assistant",
        "Let me read that file.",
        Some("read"),
        None,
        None,
        "2025-01-15T10:02:00Z",
    )
    .unwrap();

    // Trigram table should exist and be searchable.
    let count: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH 'read'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(count > 0, "trigram FTS5 should find 'read'");

    // Trigram supports substring queries that unicode61 doesn't.
    let count: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH 'rea'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(count > 0, "trigram should find substring 'rea'");
}

#[test]
fn migration_v4_adds_session_columns() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    // New columns should be writable.
    db.conn
        .execute(
            "UPDATE sessions SET ended_at = '2025-01-15T11:00:00Z', end_reason = 'done', tool_call_count = 3, api_call_count = 2 WHERE id = 'sess-1'",
            [],
        )
        .unwrap();

    let (ended_at, end_reason, tool_call_count, api_call_count): (
        Option<String>,
        Option<String>,
        i64,
        i64,
    ) = db
        .conn
        .query_row(
            "SELECT ended_at, end_reason, tool_call_count, api_call_count FROM sessions WHERE id = 'sess-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(ended_at.as_deref(), Some("2025-01-15T11:00:00Z"));
    assert_eq!(end_reason.as_deref(), Some("done"));
    assert_eq!(tool_call_count, 3);
    assert_eq!(api_call_count, 2);
}

#[test]
fn migration_v5_adds_message_columns() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();
    let msg_id = db
        .insert_message(
            "sess-1",
            "user",
            "hello",
            None,
            None,
            None,
            "2025-01-15T10:01:00Z",
        )
        .unwrap();

    // New columns should be writable.
    db.conn
        .execute(
            "UPDATE messages SET token_count = 42, finish_reason = 'stop' WHERE id = ?1",
            params![msg_id],
        )
        .unwrap();

    let (token_count, finish_reason): (Option<i64>, Option<String>) = db
        .conn
        .query_row(
            "SELECT token_count, finish_reason FROM messages WHERE id = ?1",
            params![msg_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(token_count, Some(42));
    assert_eq!(finish_reason.as_deref(), Some("stop"));
}

#[test]
fn end_session_marks_ended_at() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    db.end_session("sess-1", "done").unwrap();

    let ended_at: Option<String> = db
        .conn
        .query_row(
            "SELECT ended_at FROM sessions WHERE id = 'sess-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(ended_at.is_some(), "ended_at should be set");
}

#[test]
fn end_session_is_idempotent() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    db.end_session("sess-1", "compression").unwrap();
    // Second call with a different reason should no-op.
    db.end_session("sess-1", "done").unwrap();

    let end_reason: String = db
        .conn
        .query_row(
            "SELECT end_reason FROM sessions WHERE id = 'sess-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(end_reason, "compression", "first end_reason wins");
}

#[test]
fn open_failure_returns_descriptive_error() {
    // dirge-w4i7: assert on the returned Err, NOT the process-global
    // last_init_error(). open() clears that global on every successful
    // open, so a parallel test opening a DB races this assertion down to
    // None and the test flakes. The returned error carries the same
    // message and is per-call, so it's race-free.
    //
    // Attempt to open a path that doesn't exist as a directory (the parent
    // dir creation is done by open(), but a file where a directory should
    // be will fail).
    let bad = std::env::temp_dir().join(format!(
        "dirge-bad-db-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    // Create a regular file where state.db should be a dir.
    std::fs::write(&bad, "not a db").unwrap();
    let db_path = bad.join("state.db");

    let err = match SessionDb::open(&db_path) {
        Ok(_) => panic!("should fail to open on bad path"),
        Err(e) => e,
    };
    assert!(
        err.contains("Failed to open"),
        "error should describe the failure: {err}"
    );

    // Clean up.
    let _ = std::fs::remove_file(&bad);
}

#[test]
fn redact_for_fts_strips_vendor_prefix_tokens() {
    // AWS access key
    let r = redact_for_fts("aws key: AKIAIOSFODNN7EXAMPLE here");
    assert!(!r.contains("AKIAIOSFODNN7EXAMPLE"), "got: {r}");
    assert!(r.contains("<REDACTED>"));

    // GitHub PAT classic
    let r = redact_for_fts("token: ghp_abcdefghijklmnopqrstuvwxyz0123456789");
    assert!(!r.contains("ghp_abcdefghij"), "got: {r}");
    assert!(r.contains("<REDACTED>"));

    // Slack
    let r = redact_for_fts("creds=xoxb-1234567890-abcdefghij-AbCdEfGh tail");
    assert!(!r.contains("xoxb-1234567890"), "got: {r}");

    // OpenAI/Anthropic sk-
    let r = redact_for_fts("ANTHROPIC_API_KEY=sk-ant-12345abcdefghijklmnopqrst");
    assert!(!r.contains("sk-ant-12345abcdefghijklmnopqrst"), "got: {r}");
}

#[test]
fn redact_for_fts_strips_url_userinfo() {
    let r = redact_for_fts("DATABASE_URL=postgres://admin:hunter2@db.internal:5432/app");
    assert!(!r.contains("hunter2"), "got: {r}");
    // The whole assignment value gets caught by the env-assign
    // pattern first (DATABASE_URL doesn't trip the AUTH/KEY/TOKEN
    // gate, but the userinfo regex does — verify either way).
    assert!(r.contains("<REDACTED>"), "got: {r}");

    let r = redact_for_fts("call https://deploy:secret-tok@webhook.example.com/x");
    assert!(!r.contains("secret-tok"), "got: {r}");
}

#[test]
fn redact_for_fts_strips_authorization_header() {
    let r = redact_for_fts("Authorization: Bearer ey-some-opaque-token");
    assert!(!r.contains("ey-some-opaque-token"), "got: {r}");
    assert!(r.contains("<REDACTED>"));

    // case-insensitive
    let r = redact_for_fts("authorization: bearer abc.def.ghi");
    assert!(!r.contains("abc.def.ghi"), "got: {r}");
}

#[test]
fn redact_for_fts_strips_env_assignment() {
    let r = redact_for_fts("OPENAI_API_KEY=opaque-value-1234567890");
    assert!(!r.contains("opaque-value-1234567890"), "got: {r}");
    assert!(r.contains("<REDACTED>"));

    let r = redact_for_fts("password=hunter2");
    assert!(!r.contains("hunter2"), "got: {r}");
}

#[test]
fn redact_for_fts_strips_jwt() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    let r = redact_for_fts(&format!("token = {jwt}"));
    assert!(
        !r.contains("SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"),
        "got: {r}"
    );
    assert!(r.contains("<REDACTED>"));
}

#[test]
fn redact_for_fts_leaves_plain_text_alone() {
    let plain = "how do we handle database migrations in this project";
    assert_eq!(redact_for_fts(plain), plain);
    // Empty input is preserved.
    assert_eq!(redact_for_fts(""), "");
    // A bare URL with no userinfo passes through.
    let url = "see https://api.example.com/v1/docs";
    assert_eq!(redact_for_fts(url), url);
}

#[test]
fn redact_for_fts_strips_json_field() {
    let r = redact_for_fts(r#"{"api_key": "secret-value-xyz", "name": "alice"}"#);
    assert!(!r.contains("secret-value-xyz"), "got: {r}");
    assert!(r.contains("\"alice\""), "non-secret fields preserved: {r}");
}

/// End-to-end: secrets pass through `insert_message` to the FTS5
/// indexes redacted, but the raw row in `messages` retains the
/// original content for transcript replay.
#[test]
fn fts_index_holds_redacted_text_messages_table_holds_raw() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    let raw = "Authorization: Bearer ey-opaque-token here is some context";
    db.insert_message(
        "sess-1",
        "assistant",
        raw,
        None,
        None,
        None,
        "2025-01-15T10:01:00Z",
    )
    .unwrap();

    // messages table holds RAW content (round-trip preserved).
    let stored: String = db
        .conn
        .query_row(
            "SELECT content FROM messages WHERE session_id = 'sess-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, raw);

    // FTS indexes hold REDACTED content. A search for the secret
    // token finds nothing; a search for the non-secret context
    // finds the row.
    let hits = db.search_messages("ey-opaque-token", None).unwrap();
    assert!(hits.is_empty(), "FTS must not index the secret token");

    let hits = db.search_messages("context", None).unwrap();
    assert_eq!(hits.len(), 1, "non-secret tokens still searchable");
}

#[test]
fn fts_index_redacts_secrets_inside_tool_calls() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    let tool_calls = r#"[{"name":"bash","args":{"cmd":"curl -H 'Authorization: Bearer ghp_abcdefghijklmnopqrstuvwxyz0123456789' https://api.example.com"}}]"#;
    db.insert_message(
        "sess-1",
        "assistant",
        "Calling the API",
        Some("bash"),
        Some(tool_calls),
        None,
        "2025-01-15T10:01:00Z",
    )
    .unwrap();

    // Raw tool_calls preserved.
    let raw: String = db
        .conn
        .query_row(
            "SELECT tool_calls FROM messages WHERE session_id = 'sess-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(raw.contains("ghp_abcdefghij"), "raw kept");

    // FTS must not surface the PAT.
    let hits = db
        .search_messages("ghp_abcdefghijklmnopqrstuvwxyz0123456789", None)
        .unwrap();
    assert!(hits.is_empty(), "PAT must be redacted from FTS");

    // Non-secret tool name + content still searchable.
    let hits = db.search_messages("bash", None).unwrap();
    assert_eq!(hits.len(), 1);
}

/// Ensures v2→v3→v4→v5 chain works from a v2 database.
#[test]
fn migration_from_v2_to_v5_adds_trigram_and_columns() {
    // Create a v2 database by hand.
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "dirge-session-db-cross-test-{}-{}",
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("state.db");

    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .unwrap();
    conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA foreign_keys=ON;")
        .unwrap();

    // Create v1 schema (as if migration v1 ran), then run v2 to get to v2.
    conn.execute_batch(
        "
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY, source TEXT DEFAULT 'cli',
            model TEXT DEFAULT '', provider TEXT DEFAULT '',
            started_at TEXT NOT NULL, last_active TEXT NOT NULL,
            title TEXT DEFAULT '', message_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(id),
            role TEXT NOT NULL, content TEXT NOT NULL DEFAULT '',
            tool_name TEXT, tool_calls TEXT, tool_call_id TEXT,
            timestamp TEXT NOT NULL
        );
        CREATE VIRTUAL TABLE messages_fts USING fts5(
            content, content=messages, content_rowid=id
        );
        ",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 2).unwrap();
    conn.close().unwrap();

    // Open via SessionDb — v3, v4, v5 should fire.
    let db = SessionDb::open(&db_path).unwrap();

    // Verify pragma version reaches the current schema.
    let ver: u32 = db
        .conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(
        ver, SCHEMA_VERSION,
        "should be at the current schema version after migration"
    );

    // Trigram table should exist.
    let trigram_exists: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='messages_fts_trigram'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(trigram_exists, 1, "trigram table should exist");

    // v4 columns should be present.
    let _ = db.conn.execute(
        "UPDATE sessions SET ended_at = 'x', end_reason = 'r', tool_call_count = 1, api_call_count = 1 WHERE 1=0",
        [],
    );

    // v5 columns should be present.
    let _ = db.conn.execute(
        "UPDATE messages SET token_count = 0, finish_reason = '' WHERE 1=0",
        [],
    );
}

// --- v10: session_checkpoints (durable structured session state) ---

/// A fresh DB carries the v10 checkpoint table.
#[test]
fn schema_v10_creates_session_checkpoints() {
    let (db, _dir) = temp_db();
    let cols: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('session_checkpoints')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        cols > 0,
        "session_checkpoints table must exist on a fresh DB"
    );
}

/// First upsert inserts intent + summary at revision 0; reading it back
/// round-trips every field.
#[test]
fn checkpoint_first_upsert_round_trips() {
    let (db, _dir) = temp_db();
    db.upsert_checkpoint("s1", "fix the resume bug", "## Goal\nresume works")
        .unwrap();
    let cp = db
        .get_checkpoint("s1")
        .unwrap()
        .expect("checkpoint present");
    assert_eq!(cp.intent, "fix the resume bug");
    assert_eq!(cp.summary, "## Goal\nresume works");
    assert_eq!(cp.revision, 0);
}

/// The intent slot is the drift anchor: written once on insert, never
/// rewritten by later upserts. The summary body IS replaced and the
/// revision bumps each fold.
#[test]
fn checkpoint_intent_is_immutable_summary_is_replaced() {
    let (db, _dir) = temp_db();
    db.upsert_checkpoint("s1", "original intent", "first body")
        .unwrap();
    // Later folds pass a possibly-drifted intent — it must be ignored.
    db.upsert_checkpoint("s1", "DRIFTED intent", "second body")
        .unwrap();
    db.upsert_checkpoint("s1", "", "third body").unwrap();

    let cp = db.get_checkpoint("s1").unwrap().unwrap();
    assert_eq!(
        cp.intent, "original intent",
        "intent must never be rewritten"
    );
    assert_eq!(cp.summary, "third body", "summary is the latest fold");
    assert_eq!(cp.revision, 2, "revision bumps once per fold after insert");
}

/// Distinct sessions keep independent checkpoints; an absent session
/// reads back as None.
#[test]
fn checkpoint_is_per_session() {
    let (db, _dir) = temp_db();
    db.upsert_checkpoint("a", "intent a", "body a").unwrap();
    db.upsert_checkpoint("b", "intent b", "body b").unwrap();
    assert_eq!(db.get_checkpoint("a").unwrap().unwrap().summary, "body a");
    assert_eq!(db.get_checkpoint("b").unwrap().unwrap().intent, "intent b");
    assert!(db.get_checkpoint("missing").unwrap().is_none());
}

/// The checkpoint is keyed by the conversation's stable origin id, which
/// the fold handler carries forward across rotations. A resume that
/// resolves any chain member to that origin recovers it; the rotating
/// tip id carries no checkpoint of its own.
#[test]
fn checkpoint_after_fold_keys_by_origin() {
    let (db, _dir) = temp_db();
    // The fold passes the stable origin and the rotated summary.
    db.checkpoint_after_fold("conv-origin", "verbatim first ask", "## Goal\ndone");

    let cp = db
        .get_checkpoint("conv-origin")
        .unwrap()
        .expect("checkpoint stored under the origin id");
    assert_eq!(cp.intent, "verbatim first ask");
    assert_eq!(cp.summary, "## Goal\ndone");
    assert!(
        db.get_checkpoint("compacted-tip").unwrap().is_none(),
        "must not be keyed by a rotating tip id"
    );
}

/// A prune-only pass yields no summary — nothing to checkpoint.
#[test]
fn checkpoint_after_fold_skips_empty_summary() {
    let (db, _dir) = temp_db();
    db.checkpoint_after_fold("conv-origin", "intent", "");
    assert!(db.get_checkpoint("conv-origin").unwrap().is_none());
}

/// A pre-v10 DB (checkpoint table absent, version pinned to 9) gains the
/// table on reopen without disturbing existing memories.
#[test]
fn schema_v10_migrates_from_v9() {
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("dirge-v10-migrate-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, uid TEXT NOT NULL UNIQUE,
                 target TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'procedural',
                 content TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'active',
                 tier TEXT NOT NULL DEFAULT 'hot', salience REAL NOT NULL DEFAULT 0.5,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL, last_used_at TEXT,
                 use_count INTEGER NOT NULL DEFAULT 0, superseded_by TEXT
             );
             INSERT INTO memories (uid, target, content, created_at, updated_at)
                 VALUES ('urn:ump:keep', 'memory', 'survives the migration', 'x', 'x');
             PRAGMA user_version = 9;",
        )
        .unwrap();
    }
    let db = SessionDb::open(&path).unwrap();
    // Table now exists and is usable.
    db.upsert_checkpoint("s1", "intent", "body").unwrap();
    assert_eq!(db.get_checkpoint("s1").unwrap().unwrap().summary, "body");
    // Pre-existing memory survived.
    let kept: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE uid = 'urn:ump:keep'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1, "v10 migration must not disturb existing memories");
    let _ = std::fs::remove_dir_all(&dir);
}

/// dirge-zygq: a fresh DB carries the procedural-effectiveness
/// columns, and a pre-v12 DB gains them on reopen with sane defaults
/// while existing memories survive.
#[test]
fn schema_v12_adds_procedural_outcome_columns() {
    // Fresh DB: all three columns present.
    let (db, _dir) = temp_db();
    for col in ["success_count", "failure_count", "last_success_at"] {
        let present: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = ?1",
                [col],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "fresh DB must have the {col} column");
    }

    // Pre-v12 DB: memories table without the outcome columns + a row,
    // version pinned to 11, then reopen so migrate() runs v12.
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("dirge-v12-migrate-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, uid TEXT NOT NULL UNIQUE,
                 target TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'procedural',
                 content TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'active',
                 tier TEXT NOT NULL DEFAULT 'hot', salience REAL NOT NULL DEFAULT 0.5,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL, last_used_at TEXT,
                 use_count INTEGER NOT NULL DEFAULT 0, superseded_by TEXT
             );
             INSERT INTO memories (uid, target, content, created_at, updated_at)
                 VALUES ('urn:ump:keep', 'memory', 'survives the migration', 'x', 'x');
             PRAGMA user_version = 11;",
        )
        .unwrap();
    }
    let db = SessionDb::open(&path).unwrap();
    // Columns now exist with the documented defaults on the kept row.
    let (s, f, last): (i64, i64, Option<String>) = db
        .conn
        .query_row(
            "SELECT success_count, failure_count, last_success_at FROM memories
             WHERE uid = 'urn:ump:keep'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (s, f, last),
        (0, 0, None),
        "outcome columns default to 0/0/NULL"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// dirge-fa10: a fresh DB carries the confidence + superseded_at
/// columns, and a pre-v13 DB gains them on reopen — confidence
/// defaulting to 0.6 on existing rows — without disturbing them.
#[test]
fn schema_v13_adds_confidence_and_supersession_columns() {
    let (db, _dir) = temp_db();
    for col in ["confidence", "superseded_at"] {
        let present: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = ?1",
                [col],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "fresh DB must have the {col} column");
    }

    // Pre-v13 DB (v12 shape: has outcome columns, no confidence), a row,
    // version pinned to 12, then reopen so migrate() runs v13.
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("dirge-v13-migrate-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, uid TEXT NOT NULL UNIQUE,
                 target TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'procedural',
                 content TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'active',
                 tier TEXT NOT NULL DEFAULT 'hot', salience REAL NOT NULL DEFAULT 0.5,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL, last_used_at TEXT,
                 use_count INTEGER NOT NULL DEFAULT 0, superseded_by TEXT,
                 success_count INTEGER NOT NULL DEFAULT 0,
                 failure_count INTEGER NOT NULL DEFAULT 0, last_success_at TEXT
             );
             INSERT INTO memories (uid, target, content, created_at, updated_at)
                 VALUES ('urn:ump:keep', 'memory', 'survives the migration', 'x', 'x');
             PRAGMA user_version = 12;",
        )
        .unwrap();
    }
    let db = SessionDb::open(&path).unwrap();
    let (confidence, superseded_at): (f64, Option<String>) = db
        .conn
        .query_row(
            "SELECT confidence, superseded_at FROM memories WHERE uid = 'urn:ump:keep'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        (confidence - 0.6).abs() < 1e-9,
        "existing rows backfill confidence to 0.6: {confidence}"
    );
    assert_eq!(superseded_at, None, "superseded_at defaults NULL");
    let _ = std::fs::remove_dir_all(&dir);
}

/// dirge-70ht: the skills tables are created idempotently OUTSIDE the
/// version ladder (feature-gated SCHEMA_VERSION can't host a
/// feature-independent table above the graph migrations without breaking
/// the enable-graph-later upgrade path). A fresh open must create them.
#[test]
fn skills_tables_created_on_fresh_open() {
    let (db, _dir) = temp_db();
    let table: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skills'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(table, 1, "skills table must exist");
    let fts: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='skills_fts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts, 1, "skills_fts must exist");
}

/// The skills tables must appear even for a DB already at SCHEMA_VERSION,
/// where the migration ladder early-returns. Dropping them and bumping
/// user_version to the ceiling simulates a DB created before skills
/// existed; reopening must recreate them (proving `ensure_skills_tables`
/// runs ahead of the early return).
#[test]
fn skills_tables_recreated_when_db_at_schema_version() {
    let (db, dir) = temp_db();
    let path = dir.join("state.db");
    {
        db.conn
            .execute_batch(
                "DROP TABLE skills;
                 DROP TABLE skills_fts;",
            )
            .unwrap();
        db.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)
            .unwrap();
    }
    drop(db);
    let reopened = SessionDb::open(&path).unwrap();
    let present: i64 = reopened
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skills'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        present, 1,
        "skills table must be recreated even when the version ladder is satisfied"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// dirge-uzw4: a foreign/too-small anchor id makes anchor_row 0, and the old
// `anchor_row.saturating_sub(1) as usize` wrapped -1 to usize::MAX — so
// `before` (and anchor_index) blew up past the number of returned messages.
// Clamp at zero: an out-of-range anchor collapses to a zero-offset window.
#[test]
fn anchored_view_foreign_anchor_does_not_overflow_before() {
    let (db, dir) = temp_db();
    db.insert_session("s1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();
    for i in 0..3 {
        db.insert_message(
            "s1",
            "user",
            &format!("m{i}"),
            None,
            None,
            None,
            &format!("2025-01-15T10:0{i}:00Z"),
        )
        .unwrap();
    }
    // Anchor id 0 precedes every message → anchor_row = 0.
    let view = db.get_anchored_view("s1", 0, 5).unwrap();
    assert_eq!(
        view.before, 0,
        "before must clamp to 0 for a foreign anchor"
    );
    assert_eq!(view.anchor_index, 0);
    assert!(
        view.anchor_index <= view.messages.len(),
        "anchor_index must stay within the returned window"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// dirge-m7ja: a fold persists its lineage atomically (old ended, rotated row
/// inserted, child→parent linked) and returns a Result the caller can act on —
/// replacing three silent best-effort writes that could leave a partial chain.
#[test]
fn link_fold_persists_atomic_resolvable_lineage() {
    let (db, _dir) = temp_db();
    db.insert_session("root", "cli", "m", "p", "2026-01-01T10:00:00Z")
        .unwrap();

    db.link_fold("root", "child", "cli", "m", "p", "2026-01-01T10:05:00Z")
        .unwrap();

    // The link exists → resolve_parent walks child → root.
    assert_eq!(db.resolve_parent("child").unwrap(), "root");
    // Old session ended with the compression reason.
    let reason: Option<String> = db
        .conn
        .query_row(
            "SELECT end_reason FROM sessions WHERE id = 'root'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason.as_deref(), Some("compression"));
    // Rotated session row present.
    let present: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE id = 'child'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(present, 1);
}

/// dirge-m7ja: successive folds chain, so resolve_parent from any tip reaches
/// the original root — the property search/browse rely on to show one
/// conversation, not one per rotation.
#[test]
fn link_fold_chains_multiple_folds_to_original_root() {
    let (db, _dir) = temp_db();
    db.insert_session("s0", "cli", "m", "p", "2026-01-01T10:00:00Z")
        .unwrap();
    db.link_fold("s0", "s1", "cli", "m", "p", "2026-01-01T10:05:00Z")
        .unwrap();
    db.link_fold("s1", "s2", "cli", "m", "p", "2026-01-01T10:10:00Z")
        .unwrap();

    assert_eq!(db.resolve_parent("s2").unwrap(), "s0");
    assert_eq!(db.resolve_parent("s1").unwrap(), "s0");
}
