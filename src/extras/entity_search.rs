//! Recursive CTE query engine over entity/relation graph (#393).
//!
//! PRISM's N1 "Hierarchical Bundle Search" implemented as
//! parameterized `WITH RECURSIVE` queries over `entities` and
//! `relations` tables.

use rusqlite::{Connection, params};
use std::collections::HashMap;

/// Traverse the entity graph outward from seed ids, following typed
/// relations up to `max_depth` hops.
///
/// Returns rows of (entity_id, relation_path, depth) where
/// `relation_path` is a human-readable string like
/// `E0308[error]→occurred_in→src/main.rs[file]`.
///
/// If `min_confidence` is provided, only relations with `confidence >= min_confidence`
/// are traversed. This prunes stale edges when combined with staleness-decayed
/// confidence values.
pub fn traverse_from(
    conn: &Connection,
    seed_ids: &[i64],
    max_depth: u32,
    min_confidence: Option<f64>,
) -> Result<Vec<(i64, String, u32)>, String> {
    if seed_ids.is_empty() {
        return Ok(Vec::new());
    }

    let seed_list = seed_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let sql = format!(
        "WITH RECURSIVE trace(id, path, depth) AS (
            SELECT e.id,
                   e.name || '[' || e.kind || ']',
                   0
            FROM entities e
            WHERE e.id IN ({seed_list})

            UNION ALL

            SELECT e.id,
                   t.path || '→' || r.rel_type || '→' || e.name || '[' || e.kind || ']',
                   t.depth + 1
            FROM trace t
            JOIN relations r ON r.source_id = t.id
            JOIN entities e ON r.target_id = e.id
            WHERE t.depth < ?1 AND r.confidence >= ?2
        )
        SELECT DISTINCT id, path, depth
        FROM trace
        ORDER BY depth, path"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("traverse_from: {e}"))?;

    let confidence = min_confidence.unwrap_or(0.0);

    let mapped = stmt
        .query_map(params![max_depth as i64, confidence], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u32))
        })
        .map_err(|e| format!("traverse_from query: {e}"))?;

    Ok(mapped.filter_map(|r| r.ok()).collect())
}

/// Weighted variant of `traverse_from`. Instead of uniform depth-based BFS,
/// each relation type carries a cost multiplier from `edge_weights`.
/// Traversal prunes when cumulative cost exceeds `max_cost`.
///
/// Returns (entity_id, relation_path, cumulative_cost).
/// Unmapped relation types default to cost 1.0.
#[allow(dead_code)]
pub fn traverse_from_weighted(
    conn: &Connection,
    seed_ids: &[i64],
    edge_weights: &HashMap<String, f64>,
    max_cost: f64,
) -> Result<Vec<(i64, String, f64)>, String> {
    if seed_ids.is_empty() {
        return Ok(Vec::new());
    }

    let seed_list = seed_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let cost_case = if edge_weights.is_empty() {
        "1.0".to_string()
    } else {
        let branches: Vec<String> = edge_weights
            .iter()
            .map(|(rel_type, weight)| {
                format!("WHEN '{}' THEN {}", rel_type.replace('\'', "''"), weight)
            })
            .collect();
        format!("CASE r.rel_type {} ELSE 1.0 END", branches.join(" "))
    };

    let sql = format!(
        "WITH RECURSIVE trace(id, path, cost) AS (
            SELECT e.id,
                   e.name || '[' || e.kind || ']',
                   0.0
            FROM entities e
            WHERE e.id IN ({seed_list})

            UNION ALL

            SELECT e.id,
                   t.path || '→' || r.rel_type || '→' || e.name || '[' || e.kind || ']',
                   t.cost + {cost_case}
            FROM trace t
            JOIN relations r ON r.source_id = t.id
            JOIN entities e ON r.target_id = e.id
            WHERE t.cost + {cost_case} <= ?1
        )
        SELECT DISTINCT id, path, cost
        FROM trace
        ORDER BY cost, path"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("traverse_from_weighted: {e}"))?;

    let mapped = stmt
        .query_map(params![max_cost], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|e| format!("traverse_from_weighted query: {e}"))?;

    Ok(mapped.filter_map(|r| r.ok()).collect())
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::extras::entity_db::*;
    use rusqlite::Connection;

    pub(crate) fn setup_graph(conn: &Connection) {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                started_at TEXT NOT NULL,
                last_active TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL DEFAULT '',
                timestamp TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS entities (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id),
                message_id INTEGER REFERENCES messages(id),
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                extra TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS relations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_id INTEGER NOT NULL REFERENCES entities(id),
                target_id INTEGER NOT NULL REFERENCES entities(id),
                rel_type TEXT NOT NULL,
                session_id TEXT NOT NULL REFERENCES sessions(id),
                confidence REAL DEFAULT 1.0,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS entities_fts USING fts5(
                name, kind,
                tokenize='unicode61'
            );
            ",
        )
        .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO sessions (id, started_at, last_active) VALUES ('ts', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO messages (session_id, role, content, timestamp) VALUES ('ts', 'tool', '', datetime('now'))",
            [],
        )
        .unwrap();
    }

    #[test]
    fn traverse_empty_seeds_returns_empty() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);
        let results = traverse_from(&conn, &[], 3, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn traverse_single_hop() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);

        let err_id = insert_entity(&conn, "ts", Some(1), "error", "E0308", None).unwrap();
        let file_id = insert_entity(&conn, "ts", Some(1), "file", "src/main.rs", None).unwrap();
        insert_relation(&conn, err_id, file_id, "occurred_in", "ts").unwrap();

        let results = traverse_from(&conn, &[err_id], 2, None).unwrap();

        // Depth 0: the seed itself
        let seeds: Vec<_> = results.iter().filter(|r| r.2 == 0).collect();
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].0, err_id);
        assert!(seeds[0].1.contains("E0308"));

        // Depth 1: the file
        let depth1: Vec<_> = results.iter().filter(|r| r.2 == 1).collect();
        assert_eq!(depth1.len(), 1);
        assert_eq!(depth1[0].0, file_id);
        assert!(depth1[0].1.contains("occurred_in"));
        assert!(depth1[0].1.contains("src/main.rs"));
    }

    #[test]
    fn traverse_two_hops() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);

        let err_id = insert_entity(&conn, "ts", Some(1), "error", "E0308", None).unwrap();
        let file_id = insert_entity(&conn, "ts", Some(1), "file", "src/main.rs", None).unwrap();
        let commit_id = insert_entity(&conn, "ts", Some(1), "commit", "abc123", None).unwrap();

        insert_relation(&conn, err_id, file_id, "occurred_in", "ts").unwrap();
        insert_relation(&conn, file_id, commit_id, "touched_by", "ts").unwrap();

        let results = traverse_from(&conn, &[err_id], 3, None).unwrap();
        assert!(!results.is_empty());

        // Should find the commit at depth 2
        let depth2: Vec<_> = results.iter().filter(|r| r.2 == 2).collect();
        assert_eq!(depth2.len(), 1);
        assert_eq!(depth2[0].0, commit_id);
        assert!(depth2[0].1.contains("abc123"));
    }

    #[test]
    fn traverse_weighted_cost_pruning() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);

        let err_id = insert_entity(&conn, "ts", Some(1), "error", "E0308", None).unwrap();
        let file_id = insert_entity(&conn, "ts", Some(1), "file", "src/main.rs", None).unwrap();
        let commit_id = insert_entity(&conn, "ts", Some(1), "commit", "abc123", None).unwrap();

        insert_relation(&conn, err_id, file_id, "occurred_in", "ts").unwrap();
        insert_relation(&conn, file_id, commit_id, "touched_by", "ts").unwrap();

        // occurred_in is cheap (0.5), touched_by is expensive (2.0).
        let mut weights = HashMap::new();
        weights.insert("occurred_in".to_string(), 0.5);
        weights.insert("touched_by".to_string(), 2.0);

        // max_cost 3.0: error→occurred_in→file costs 0.5, file→touched_by→commit costs 2.0 → 2.5 total
        let results = traverse_from_weighted(&conn, &[err_id], &weights, 3.0).unwrap();
        let depth2: Vec<_> = results.iter().filter(|r| r.0 == commit_id).collect();
        assert_eq!(depth2.len(), 1);
        assert_eq!(depth2[0].2, 2.5);

        // max_cost 1.0: error→file costs 0.5, but file→commit costs 2.0 (total 2.5 > 1.0)
        let results = traverse_from_weighted(&conn, &[err_id], &weights, 0.9).unwrap();
        let depth2: Vec<_> = results.iter().filter(|r| r.0 == commit_id).collect();
        assert!(
            depth2.is_empty(),
            "commit should be unreachable with max_cost 0.9"
        );
    }

    #[test]
    fn traverse_confidence_filter_prunes_stale_edges() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);

        let err_id = insert_entity(&conn, "ts", Some(1), "error", "E0308", None).unwrap();
        let file_id = insert_entity(&conn, "ts", Some(1), "file", "src/main.rs", None).unwrap();
        let commit_id = insert_entity(&conn, "ts", Some(1), "commit", "abc123", None).unwrap();

        insert_relation(&conn, err_id, file_id, "occurred_in", "ts").unwrap();
        insert_relation(&conn, file_id, commit_id, "touched_by", "ts").unwrap();

        // Lower confidence on the second relation
        conn.execute(
            "UPDATE relations SET confidence = 0.3 WHERE source_id = ?1 AND target_id = ?2",
            rusqlite::params![file_id, commit_id],
        )
        .unwrap();

        // min_confidence 0.0 → all edges visible
        let results = traverse_from(&conn, &[err_id], 3, Some(0.0)).unwrap();
        let depth2: Vec<_> = results.iter().filter(|r| r.0 == commit_id).collect();
        assert_eq!(depth2.len(), 1, "commit reachable with min_confidence=0.0");

        // min_confidence 0.5 → prunes the 0.3-confidence edge
        let results = traverse_from(&conn, &[err_id], 3, Some(0.5)).unwrap();
        let depth2: Vec<_> = results.iter().filter(|r| r.0 == commit_id).collect();
        assert!(depth2.is_empty(), "commit pruned with min_confidence=0.5");

        // No filter → default 0.0, all edges visible
        let results = traverse_from(&conn, &[err_id], 3, None).unwrap();
        let depth2: Vec<_> = results.iter().filter(|r| r.0 == commit_id).collect();
        assert_eq!(
            depth2.len(),
            1,
            "commit reachable with no confidence filter"
        );
    }
}
