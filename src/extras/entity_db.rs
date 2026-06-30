//! SQLite-backed entity/relation graph storage (dirge-graph-search, #393).
//!
//! Two tables added by migration v14: `entities` (kind + name rows
//! extracted from tool output by Janet compressors) and `relations`
//! (typed edges connecting entities). FTS5 is standalone (app-managed
//! sync, no triggers) matching the v7 memories_fts pattern.
//!
//! All functions are gated behind `#[cfg(feature = "experimental-graph-search")]`
//! — if the feature is never enabled, none of this compiles and migration
//! v14 never runs.

use rusqlite::{Connection, params};
use serde::Serialize;
use std::collections::HashMap;

use crate::extras::fts;
use crate::extras::session_db::redact_for_fts;

/// An entity match with computed staleness score (0.0 = oldest, 1.0 = newest).
#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
pub struct EntityMatch {
    pub id: i64,
    pub session_id: String,
    pub kind: String,
    pub name: String,
    pub extra: Option<String>,
    pub created_at: String,
    /// 0.0-1.0 recency score, computed from entity ID order within the session.
    /// 1.0 = most recent, 0.0 = oldest. `None` when staleness wasn't computed.
    pub staleness_score: Option<f64>,
}

impl EntityMatch {
    pub fn from_row(
        row: (i64, String, String, String, Option<String>, String),
        staleness: Option<f64>,
    ) -> Self {
        Self {
            id: row.0,
            session_id: row.1,
            kind: row.2,
            name: row.3,
            extra: row.4,
            created_at: row.5,
            staleness_score: staleness,
        }
    }
}

// ── PRISM Memory Schema constants (#393) ──────────────────────────────────
//
// PRISM defines 6 typed edge types over a hierarchy of entities, facets,
// facet-points, and episodes. When an entity's schema_version is 'prism',
// only these constants are valid rel_type values.
//
// These are string constants usable from both Rust and Janet compressor
// plugins (via harness/record-relation).

/// A facet (sub-entity) belongs to a parent entity.
#[allow(dead_code)]
pub const HAS_FACET: &str = "HAS_FACET";
/// A concrete observation point within a facet.
#[allow(dead_code)]
pub const HAS_POINT: &str = "HAS_POINT";
/// A facet belongs to an episode (temporal grouping).
#[allow(dead_code)]
pub const EPISODE_OF: &str = "EPISODE_OF";
/// Temporal ordering between episodes or entities.
#[allow(dead_code)]
pub const PRECEDES: &str = "PRECEDES";
/// Causal or inferential derivation (e.g. git status → file entity).
#[allow(dead_code)]
pub const DERIVED_FROM: &str = "DERIVED_FROM";
/// Statistical or heuristic correlation.
#[allow(dead_code)]
pub const CORRELATED_WITH: &str = "CORRELATED_WITH";

/// All valid PRISM relation types.
#[allow(dead_code)]
pub const PRISM_REL_TYPES: &[&str] = &[
    HAS_FACET,
    HAS_POINT,
    EPISODE_OF,
    PRECEDES,
    DERIVED_FROM,
    CORRELATED_WITH,
];

/// Validate a relation type against an entity's schema_version.
///
/// For 'generic' (default), all rel_types are accepted.
/// For 'prism', only the 6 PRISM constants are valid.
#[allow(dead_code)]
pub fn validate_rel_type(schema_version: &str, rel_type: &str) -> Result<(), String> {
    if schema_version == "prism" && !PRISM_REL_TYPES.contains(&rel_type) {
        return Err(format!(
            "invalid PRISM relation type '{rel_type}': must be one of HAS_FACET, HAS_POINT, \
             EPISODE_OF, PRECEDES, DERIVED_FROM, CORRELATED_WITH"
        ));
    }
    Ok(())
}

/// Resolve an entity globally by (kind, name), ignoring session_id.
/// Returns the entity's id if found, None otherwise.
pub fn resolve_entity(conn: &Connection, kind: &str, name: &str) -> Result<Option<i64>, String> {
    let mut stmt = conn
        .prepare("SELECT id FROM entities WHERE kind = ?1 AND name = ?2 ORDER BY id LIMIT 1")
        .map_err(|e| format!("resolve_entity: {e}"))?;
    let mut rows = stmt
        .query_map(params![kind, name], |row| row.get(0))
        .map_err(|e| format!("resolve_entity query: {e}"))?;
    rows.next()
        .transpose()
        .map_err(|e| format!("resolve_entity iter: {e}"))
}

/// Insert a new entity row. Returns the new row's id.
///
/// FTS5 is synced after insert.
#[allow(dead_code)]
pub fn insert_entity(
    conn: &Connection,
    session_id: &str,
    message_id: Option<i64>,
    kind: &str,
    name: &str,
    extra: Option<&str>,
) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO entities (session_id, message_id, kind, name, extra) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![session_id, message_id, kind, name, extra],
    )
    .map_err(|e| format!("insert_entity: {e}"))?;

    let id = conn.last_insert_rowid();
    sync_entity_fts(conn, id, name, kind)?;
    Ok(id)
}

/// Insert or skip a duplicate entity by (kind, name), globally across
/// all sessions (cross-session dedup). Updates `extra` if the row
/// already existed.
/// Returns the entity's id (existing or new), and syncs FTS5.
pub fn upsert_entity(
    conn: &Connection,
    session_id: &str,
    message_id: Option<i64>,
    kind: &str,
    name: &str,
    extra: Option<&str>,
) -> Result<i64, String> {
    // Cross-session dedup: check globally first
    if let Some(existing_id) = resolve_entity(conn, kind, name)? {
        if let Some(extra_val) = extra {
            let _ = conn.execute(
                "UPDATE entities SET extra = ?1 WHERE id = ?2 AND extra IS NOT ?1",
                params![extra_val, existing_id],
            );
        }
        return Ok(existing_id);
    }

    // New entity: insert and sync FTS
    conn.execute(
        "INSERT INTO entities (session_id, message_id, kind, name, extra) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![session_id, message_id, kind, name, extra],
    )
    .map_err(|e| format!("upsert_entity insert: {e}"))?;

    let id = conn.last_insert_rowid();
    sync_entity_fts(conn, id, name, kind)?;
    Ok(id)
}

/// Insert a typed relation between two entities.
pub fn insert_relation(
    conn: &Connection,
    source_id: i64,
    target_id: i64,
    rel_type: &str,
    session_id: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO relations (source_id, target_id, rel_type, session_id) VALUES (?1, ?2, ?3, ?4)",
        params![source_id, target_id, rel_type, session_id],
    )
    .map_err(|e| format!("insert_relation: {e}"))?;
    Ok(())
}

/// Convenience: upsert two entities and insert a relation between them.
/// The compressor says "error E0308 occurred_in crate dirge-core" — this
/// creates both entities and the edge in one call.
#[allow(dead_code, clippy::too_many_arguments)]
pub fn record_entity_pair(
    conn: &Connection,
    session_id: &str,
    message_id: Option<i64>,
    source_kind: &str,
    source_name: &str,
    target_kind: &str,
    target_name: &str,
    rel_type: &str,
) -> Result<(), String> {
    let sid = upsert_entity(conn, session_id, message_id, source_kind, source_name, None)?;
    let tid = upsert_entity(conn, session_id, message_id, target_kind, target_name, None)?;
    insert_relation(conn, sid, tid, rel_type, session_id)
}

/// FTS5 search over entities by name + kind. Follows memory_db's
/// fts::quote_terms + MATCH pattern. Returns (id, session_id, kind, name, extra, created_at).
///
/// When `session_id` is `Some`, results are scoped to that session only
/// via `AND e.session_id = ?`. This prevents stale cross-session entities
/// from leaking into the current agent's reasoning context.
#[allow(clippy::type_complexity)]
pub fn search_entities(
    conn: &Connection,
    query: &str,
    kind_filter: Option<&str>,
    session_id: Option<&str>,
    limit: usize,
) -> Result<Vec<(i64, String, String, String, Option<String>, String)>, String> {
    let fts_query = fts::quote_terms(query);
    if fts_query.is_empty() {
        return Ok(Vec::new());
    }

    let mut stmt;
    let rows: Vec<_> = match (kind_filter, session_id) {
        (Some(kind), Some(sid)) => {
            stmt = conn
                .prepare(
                    "SELECT e.id, e.session_id, e.kind, e.name, e.extra, e.created_at
                     FROM entities_fts
                     JOIN entities e ON e.id = entities_fts.rowid
                     WHERE entities_fts MATCH ?1 AND e.kind = ?2 AND e.session_id = ?3
                     ORDER BY rank
                     LIMIT ?4",
                )
                .map_err(|e| format!("search_entities: {e}"))?;
            stmt.query_map(params![fts_query, kind, sid, limit as i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .map_err(|e| format!("search_entities query: {e}"))?
            .filter_map(|r| r.ok())
            .collect()
        }
        (Some(kind), None) => {
            stmt = conn
                .prepare(
                    "SELECT e.id, e.session_id, e.kind, e.name, e.extra, e.created_at
                     FROM entities_fts
                     JOIN entities e ON e.id = entities_fts.rowid
                     WHERE entities_fts MATCH ?1 AND e.kind = ?2
                     ORDER BY rank
                     LIMIT ?3",
                )
                .map_err(|e| format!("search_entities: {e}"))?;
            stmt.query_map(params![fts_query, kind, limit as i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .map_err(|e| format!("search_entities query: {e}"))?
            .filter_map(|r| r.ok())
            .collect()
        }
        (None, Some(sid)) => {
            stmt = conn
                .prepare(
                    "SELECT e.id, e.session_id, e.kind, e.name, e.extra, e.created_at
                     FROM entities_fts
                     JOIN entities e ON e.id = entities_fts.rowid
                     WHERE entities_fts MATCH ?1 AND e.session_id = ?2
                     ORDER BY rank
                     LIMIT ?3",
                )
                .map_err(|e| format!("search_entities: {e}"))?;
            stmt.query_map(params![fts_query, sid, limit as i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .map_err(|e| format!("search_entities query: {e}"))?
            .filter_map(|r| r.ok())
            .collect()
        }
        (None, None) => {
            stmt = conn
                .prepare(
                    "SELECT e.id, e.session_id, e.kind, e.name, e.extra, e.created_at
                     FROM entities_fts
                     JOIN entities e ON e.id = entities_fts.rowid
                     WHERE entities_fts MATCH ?1
                     ORDER BY rank
                     LIMIT ?2",
                )
                .map_err(|e| format!("search_entities: {e}"))?;
            stmt.query_map(params![fts_query, limit as i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .map_err(|e| format!("search_entities query: {e}"))?
            .filter_map(|r| r.ok())
            .collect()
        }
    };

    Ok(rows)
}

/// FTS5 search over entities with staleness scores attached.
///
/// Wraps `search_entities` results in `EntityMatch` structs with
/// computed `staleness_score` from `entity_staleness_scores`.
#[allow(dead_code)]
pub fn search_entities_with_staleness(
    conn: &Connection,
    query: &str,
    kind_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<EntityMatch>, String> {
    let rows = search_entities(conn, query, kind_filter, None, limit)?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // Batch staleness: compute scores for all sessions referenced in results
    let mut session_ids: Vec<&str> = rows.iter().map(|r| r.1.as_str()).collect();
    session_ids.sort();
    session_ids.dedup();

    let mut all_scores: HashMap<i64, f64> = HashMap::new();
    for sid in &session_ids {
        if let Ok(scores) = entity_staleness_scores(conn, sid) {
            all_scores.extend(scores);
        }
    }

    Ok(rows
        .into_iter()
        .map(|row| {
            let staleness = all_scores.get(&row.0).copied();
            EntityMatch::from_row(row, staleness)
        })
        .collect())
}

/// Compute staleness scores for all entities in a session.
///
/// Returns a map of entity_id → 0.0-1.0 recency score, where 1.0 is the
/// most recent entity in the session and 0.0 is the oldest. Score is
/// computed from entity `id` order within the session (higher id = newer).
pub fn entity_staleness_scores(
    conn: &Connection,
    session_id: &str,
) -> Result<HashMap<i64, f64>, String> {
    let mut stmt = conn
        .prepare("SELECT id FROM entities WHERE session_id = ?1 ORDER BY id")
        .map_err(|e| format!("entity_staleness_scores: {e}"))?;

    let ids: Vec<i64> = stmt
        .query_map(params![session_id], |row| row.get(0))
        .map_err(|e| format!("entity_staleness_scores query: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let max_id = *ids.last().unwrap() as f64;
    let min_id = *ids.first().unwrap() as f64;
    let range = if (max_id - min_id) < 1.0 {
        1.0
    } else {
        max_id - min_id
    };

    Ok(ids
        .into_iter()
        .map(|id| (id, (id as f64 - min_id) / range))
        .collect())
}

// ── Internal helpers ──────────────────────────────────────────────────────

fn sync_entity_fts(conn: &Connection, rowid: i64, name: &str, kind: &str) -> Result<(), String> {
    conn.execute("DELETE FROM entities_fts WHERE rowid = ?1", params![rowid])
        .map_err(|e| format!("entity FTS delete: {e}"))?;
    conn.execute(
        "INSERT INTO entities_fts(rowid, name, kind) VALUES (?1, ?2, ?3)",
        params![rowid, redact_for_fts(name), redact_for_fts(kind)],
    )
    .map_err(|e| format!("entity FTS insert: {e}"))?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();

        conn.execute_batch(
            "
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL DEFAULT 'cli',
                model TEXT NOT NULL DEFAULT '',
                provider TEXT NOT NULL DEFAULT '',
                started_at TEXT NOT NULL,
                last_active TEXT NOT NULL,
                title TEXT NOT NULL DEFAULT '',
                message_count INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id),
                role TEXT NOT NULL,
                content TEXT NOT NULL DEFAULT '',
                tool_name TEXT,
                tool_calls TEXT,
                tool_call_id TEXT,
                timestamp TEXT NOT NULL
            );
            CREATE TABLE entities (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id),
                message_id INTEGER REFERENCES messages(id),
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                extra TEXT,
                schema_version TEXT NOT NULL DEFAULT 'generic',
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE relations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_id INTEGER NOT NULL REFERENCES entities(id),
                target_id INTEGER NOT NULL REFERENCES entities(id),
                rel_type TEXT NOT NULL,
                session_id TEXT NOT NULL REFERENCES sessions(id),
                confidence REAL DEFAULT 1.0,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE VIRTUAL TABLE entities_fts USING fts5(
                name, kind,
                tokenize='unicode61'
            );
            ",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO sessions (id, started_at, last_active) VALUES ('test-session', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('test-session', 'tool', '', datetime('now'))",
            [],
        )
        .unwrap();

        conn
    }

    #[test]
    fn insert_and_query_entity() {
        let conn = in_memory_db();
        let id = insert_entity(
            &conn,
            "test-session",
            Some(1),
            "file",
            "src/main.rs",
            Some("modified"),
        )
        .unwrap();

        let row: (String, String) = conn
            .query_row(
                "SELECT kind, name FROM entities WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(row.0, "file");
        assert_eq!(row.1, "src/main.rs");

        // FTS5 should find it
        let results = search_entities(&conn, "main.rs", None, None, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "test-session");
        assert_eq!(results[0].3, "src/main.rs");
    }

    #[test]
    fn upsert_entity_dedup() {
        let conn = in_memory_db();
        let id1 = upsert_entity(&conn, "test-session", Some(1), "error", "E0308", None).unwrap();
        let id2 = upsert_entity(
            &conn,
            "test-session",
            Some(1),
            "error",
            "E0308",
            Some("msg"),
        )
        .unwrap();

        assert_eq!(id1, id2, "same (session, kind, name) returns same id");

        // Verify extra was updated
        let extra: Option<String> = conn
            .query_row(
                "SELECT extra FROM entities WHERE id = ?1",
                params![id1],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(extra, Some("msg".to_string()));
    }

    #[test]
    fn insert_relation_and_pair() {
        let conn = in_memory_db();
        let file_id =
            insert_entity(&conn, "test-session", Some(1), "file", "src/main.rs", None).unwrap();
        let err_id = insert_entity(&conn, "test-session", Some(1), "error", "E0308", None).unwrap();

        insert_relation(&conn, err_id, file_id, "occurred_in", "test-session").unwrap();

        // Verify relation exists
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM relations WHERE source_id = ?1 AND target_id = ?2",
                params![err_id, file_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn record_entity_pair_creates_both() {
        let conn = in_memory_db();
        record_entity_pair(
            &conn,
            "test-session",
            Some(1),
            "error",
            "E0308",
            "file",
            "src/main.rs",
            "occurred_in",
        )
        .unwrap();

        // Both entities exist
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entities", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        // Relation exists
        let rel_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM relations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rel_count, 1);
    }

    #[test]
    fn fts_search_by_kind() {
        let conn = in_memory_db();
        insert_entity(&conn, "test-session", Some(1), "file", "src/main.rs", None).unwrap();
        insert_entity(&conn, "test-session", Some(1), "error", "E0308", None).unwrap();
        insert_entity(&conn, "test-session", Some(1), "file", "src/lib.rs", None).unwrap();

        let files = search_entities(&conn, "src", Some("file"), None, 10).unwrap();
        assert_eq!(files.len(), 2);
        for f in &files {
            assert_eq!(f.2, "file");
        }

        let errors = search_entities(&conn, "E0308", Some("error"), None, 10).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].2, "error");
    }

    #[test]
    fn upsert_cross_session_dedup() {
        let conn = in_memory_db();

        // Add a second session
        conn.execute(
            "INSERT INTO sessions (id, started_at, last_active) VALUES ('other-session', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('other-session', 'tool', '', datetime('now'))",
            [],
        )
        .unwrap();

        let id1 =
            upsert_entity(&conn, "test-session", Some(1), "file", "src/main.rs", None).unwrap();
        let id2 =
            upsert_entity(&conn, "other-session", Some(2), "file", "src/main.rs", None).unwrap();

        assert_eq!(id1, id2, "cross-session same (kind, name) shares entity id");

        // Only one row in entities for this (kind, name)
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entities WHERE kind = 'file' AND name = 'src/main.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "one entity row for cross-session dedup");
    }

    #[test]
    fn prism_rel_type_constants_defined() {
        assert_eq!(HAS_FACET, "HAS_FACET");
        assert_eq!(DERIVED_FROM, "DERIVED_FROM");
        assert_eq!(PRISM_REL_TYPES.len(), 6);
    }

    #[test]
    fn validate_rel_type_generic_accepts_anything() {
        // generic schema_version accepts any rel_type
        assert!(validate_rel_type("generic", "occurred_in").is_ok());
        assert!(validate_rel_type("generic", "random_type").is_ok());
        assert!(validate_rel_type("generic", HAS_FACET).is_ok());
    }

    #[test]
    fn validate_rel_type_prism_rejects_unknown() {
        assert!(validate_rel_type("prism", "occurred_in").is_err());
        assert!(validate_rel_type("prism", "random").is_err());
    }

    #[test]
    fn validate_rel_type_prism_accepts_all_six() {
        assert!(validate_rel_type("prism", HAS_FACET).is_ok());
        assert!(validate_rel_type("prism", HAS_POINT).is_ok());
        assert!(validate_rel_type("prism", EPISODE_OF).is_ok());
        assert!(validate_rel_type("prism", PRECEDES).is_ok());
        assert!(validate_rel_type("prism", DERIVED_FROM).is_ok());
        assert!(validate_rel_type("prism", CORRELATED_WITH).is_ok());
    }

    #[test]
    fn insert_relation_with_prism_constant() {
        let conn = in_memory_db();
        let a = insert_entity(&conn, "test-session", Some(1), "error", "E0308", None).unwrap();
        let b = insert_entity(&conn, "test-session", Some(1), "file", "src/main.rs", None).unwrap();

        // HAS_FACET should work (generic schema accepts all)
        insert_relation(&conn, a, b, HAS_FACET, "test-session").unwrap();

        let rel_type: String = conn
            .query_row(
                "SELECT rel_type FROM relations WHERE source_id = ?1 AND target_id = ?2",
                params![a, b],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rel_type, "HAS_FACET");
    }

    #[test]
    fn resolve_entity_finds_existing() {
        let conn = in_memory_db();
        let id = insert_entity(&conn, "test-session", Some(1), "error", "E0308", None).unwrap();

        let found = resolve_entity(&conn, "error", "E0308").unwrap();
        assert_eq!(found, Some(id));

        let missing = resolve_entity(&conn, "error", "nonexistent").unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn cross_session_two_relations_one_entity() {
        let conn = in_memory_db();

        conn.execute(
            "INSERT INTO sessions (id, started_at, last_active) VALUES ('other-session', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('other-session', 'tool', '', datetime('now'))",
            [],
        )
        .unwrap();

        // Session 1: error E0308 occurred_in src/main.rs
        record_entity_pair(
            &conn,
            "test-session",
            Some(1),
            "error",
            "E0308",
            "file",
            "src/main.rs",
            "occurred_in",
        )
        .unwrap();

        // Session 2: same error, same file → same entity ids, new relation
        record_entity_pair(
            &conn,
            "other-session",
            Some(2),
            "error",
            "E0308",
            "file",
            "src/main.rs",
            "occurred_in",
        )
        .unwrap();

        // One entity row per (kind, name)
        let entity_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entities", [], |row| row.get(0))
            .unwrap();
        assert_eq!(entity_count, 2, "two unique entities, not four");

        // Two relations (one per session)
        let rel_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM relations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rel_count, 2, "two relations across two sessions");

        // Both relations reference the same entity ids
        let distinct_entities: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT e.id) FROM entities e
                 JOIN relations r ON r.source_id = e.id OR r.target_id = e.id",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(distinct_entities, 2);
    }

    #[test]
    fn search_entities_single_result_cross_session() {
        let conn = in_memory_db();

        conn.execute(
            "INSERT INTO sessions (id, started_at, last_active) VALUES ('other-session', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('other-session', 'tool', '', datetime('now'))",
            [],
        )
        .unwrap();

        upsert_entity(&conn, "test-session", Some(1), "error", "E0308", None).unwrap();
        upsert_entity(&conn, "other-session", Some(2), "error", "E0308", None).unwrap();

        let results = search_entities(&conn, "E0308", None, None, 10).unwrap();
        assert_eq!(
            results.len(),
            1,
            "FTS5 returns one result for cross-session dup"
        );
        assert_eq!(results[0].2, "error");
        assert_eq!(results[0].3, "E0308");
    }

    #[test]
    fn staleness_scores_newest_is_highest() {
        let conn = in_memory_db();
        // Insert entities with increasing ids (newer = higher id)
        let id1 = insert_entity(&conn, "test-session", Some(1), "file", "old.rs", None).unwrap();
        let id2 = insert_entity(&conn, "test-session", Some(1), "file", "mid.rs", None).unwrap();
        let id3 = insert_entity(&conn, "test-session", Some(1), "file", "new.rs", None).unwrap();

        let scores = entity_staleness_scores(&conn, "test-session").unwrap();
        assert_eq!(scores.len(), 3);
        assert!(scores[&id1] < scores[&id2], "older entity scores lower");
        assert!(scores[&id2] < scores[&id3], "newer entity scores higher");
        assert!(
            (scores[&id3] - 1.0).abs() < f64::EPSILON,
            "newest entity scores ~1.0"
        );
        assert!(
            (scores[&id1] - 0.0).abs() < f64::EPSILON,
            "oldest entity scores ~0.0"
        );
    }

    #[test]
    fn staleness_scores_empty_session() {
        let conn = in_memory_db();
        let scores = entity_staleness_scores(&conn, "test-session").unwrap();
        assert!(scores.is_empty());
    }

    #[test]
    fn staleness_scores_single_entity_is_1() {
        let conn = in_memory_db();
        let id = insert_entity(&conn, "test-session", Some(1), "file", "only.rs", None).unwrap();
        let scores = entity_staleness_scores(&conn, "test-session").unwrap();
        assert_eq!(scores.len(), 1);
        assert!(
            (scores[&id] - 0.0).abs() < f64::EPSILON,
            "single entity scores 0.0 (min==max)"
        );
    }

    #[test]
    fn entity_match_from_row_staleness_some() {
        let row = (
            42i64,
            "s1".into(),
            "error".into(),
            "E0308".into(),
            None,
            "2025-01-01".into(),
        );
        let m = EntityMatch::from_row(row, Some(0.75));
        assert_eq!(m.id, 42);
        assert_eq!(m.kind, "error");
        assert_eq!(m.name, "E0308");
        assert_eq!(m.staleness_score, Some(0.75));
    }

    #[test]
    fn entity_match_from_row_staleness_none() {
        let row = (
            1i64,
            "s2".into(),
            "file".into(),
            "main.rs".into(),
            None,
            "2025-01-01".into(),
        );
        let m = EntityMatch::from_row(row, None);
        assert_eq!(m.staleness_score, None);
    }

    #[test]
    fn search_entities_with_staleness_scores() {
        let conn = in_memory_db();
        insert_entity(&conn, "test-session", Some(1), "file", "old.rs", None).unwrap();
        insert_entity(&conn, "test-session", Some(1), "file", "new.rs", None).unwrap();

        let results = search_entities(&conn, "file", None, None, 10).unwrap();
        assert_eq!(results.len(), 2);
        // Now test with staleness scores
        let results = search_entities_with_staleness(&conn, "file", None, 10).unwrap();
        assert_eq!(results.len(), 2);

        // newer.rs (higher id) should have higher staleness than old.rs
        let old: Vec<_> = results.iter().filter(|m| m.name == "old.rs").collect();
        let new: Vec<_> = results.iter().filter(|m| m.name == "new.rs").collect();
        assert_eq!(old.len(), 1);
        assert_eq!(new.len(), 1);
        assert!(
            new[0].staleness_score.unwrap() > old[0].staleness_score.unwrap(),
            "new.rs should score higher than old.rs"
        );
    }

    #[test]
    fn search_entities_with_staleness_empty_results() {
        let conn = in_memory_db();
        let results = search_entities_with_staleness(&conn, "nonexistent", None, 10).unwrap();
        assert!(results.is_empty());
    }
}
