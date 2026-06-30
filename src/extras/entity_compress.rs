//! Evidence compression over entity graph traversal results (#393, N3).
//!
//! `compress_bundle` takes raw `traverse_from` output and condenses it
//! into a human-readable summary: grouping by entity kind, deduplicating
//! per terminal entity, and producing compact bundle text.

use rusqlite::Connection;
use std::collections::HashMap;

/// Compress traversal results into a compact human-readable summary.
///
/// Groups entities by kind, deduplicates per terminal entity (keeping
/// the shortest path), and produces a structured bundle suitable for
/// agent context injection.
pub fn compress_bundle(
    _conn: &Connection,
    trace_nodes: &[(i64, String, u32)],
    _query: &str,
) -> Result<String, String> {
    if trace_nodes.is_empty() {
        return Ok(String::new());
    }

    // Deduplicate: per terminal entity_id, keep the shortest path.
    let mut best: HashMap<i64, (String, u32)> = HashMap::new();
    for (id, path, depth) in trace_nodes {
        best.entry(*id)
            .and_modify(|(p, d)| {
                if *depth < *d {
                    *p = path.clone();
                    *d = *depth;
                }
            })
            .or_insert((path.clone(), *depth));
    }

    // Group by terminal kind extracted from path suffix "name[kind]".
    let mut by_kind: HashMap<String, Vec<String>> = HashMap::new();
    for (path, _depth) in best.values() {
        let kind = extract_terminal_kind(path);
        by_kind.entry(kind).or_default().push(path.clone());
    }

    let mut lines: Vec<String> = Vec::new();

    // Header: counts per kind.
    let kind_summary: Vec<String> = by_kind
        .iter()
        .map(|(k, v)| format!("{} {}", v.len(), k))
        .collect();
    lines.push(format!(
        "{} entities: {}",
        best.len(),
        kind_summary.join(", ")
    ));

    // Detail lines grouped by kind.
    for (kind, paths) in &by_kind {
        lines.push(format!("  [{}]", kind));
        for p in paths {
            lines.push(format!("    {}", p));
        }
    }

    Ok(lines.join("\n"))
}

/// Like `compress_bundle`, but weights output by entity staleness.
///
/// Entities within each kind are sorted newest-first. When a kind has
/// more than `detail_limit` entities, only the newest `detail_limit`
/// are shown in full; the rest are summarized as "and N older <kind>
/// from earlier in the session". An empty or absent staleness map
/// falls back to insertion order (same as `compress_bundle`).
pub fn compress_bundle_stale(
    _conn: &Connection,
    trace_nodes: &[(i64, String, u32)],
    _query: &str,
    staleness: &HashMap<i64, f64>,
) -> Result<String, String> {
    if trace_nodes.is_empty() {
        return Ok(String::new());
    }

    // Deduplicate: per terminal entity_id, keep the shortest path.
    let mut best: HashMap<i64, (String, u32)> = HashMap::new();
    for (id, path, depth) in trace_nodes {
        best.entry(*id)
            .and_modify(|(p, d)| {
                if *depth < *d {
                    *p = path.clone();
                    *d = *depth;
                }
            })
            .or_insert((path.clone(), *depth));
    }

    let detail_limit = 3usize;

    // Group by terminal kind extracted from path suffix "name[kind]".
    // Within each kind, sort by staleness (newest first).
    let mut by_kind: HashMap<String, Vec<(String, f64)>> = HashMap::new();
    for (id, (path, _depth)) in &best {
        let kind = extract_terminal_kind(path);
        let score = staleness.get(id).copied().unwrap_or(0.0);
        by_kind.entry(kind).or_default().push((path.clone(), score));
    }

    let mut lines: Vec<String> = Vec::new();

    // Header: counts per kind.
    let kind_summary: Vec<String> = by_kind
        .iter()
        .map(|(k, v)| format!("{} {}", v.len(), k))
        .collect();
    lines.push(format!(
        "{} entities: {}",
        best.len(),
        kind_summary.join(", ")
    ));

    // Detail lines grouped by kind, sorted by staleness within each.
    let mut kind_names: Vec<String> = by_kind.keys().cloned().collect();
    kind_names.sort();
    for kind in &kind_names {
        let mut entries = by_kind[kind].clone();
        // Sort newest first (higher staleness = more recent)
        entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        lines.push(format!("  [{}]", kind));
        let show_count = entries.len().min(detail_limit);
        for (path, _score) in entries.iter().take(show_count) {
            lines.push(format!("    {}", path));
        }
        if entries.len() > detail_limit {
            let older = entries.len() - detail_limit;
            lines.push(format!(
                "    ... and {} older {} from earlier in the session",
                older, kind
            ));
        }
    }

    Ok(lines.join("\n"))
}

/// Compress FTS5 search results into a kind-grouped summary.
///
/// Takes entity rows as (id, session_id, kind, name, extra, created_at)
/// and produces a compact bundle.
#[allow(clippy::type_complexity)]
pub fn compress_search_results(
    _conn: &Connection,
    rows: &[(i64, String, String, String, Option<String>, String)],
    _query: &str,
) -> Result<String, String> {
    if rows.is_empty() {
        return Ok(String::new());
    }

    let mut by_kind: HashMap<String, Vec<String>> = HashMap::new();
    for (_id, _sid, kind, name, extra, _ts) in rows {
        let label = match extra {
            Some(e) if !e.is_empty() => format!("{}/{} ({})", kind, name, e),
            _ => format!("{}/{}", kind, name),
        };
        by_kind.entry(kind.clone()).or_default().push(label);
    }

    let mut lines: Vec<String> = Vec::new();
    let kind_summary: Vec<String> = by_kind
        .iter()
        .map(|(k, v)| format!("{} {}", v.len(), k))
        .collect();
    lines.push(format!(
        "{} entities: {}",
        rows.len(),
        kind_summary.join(", ")
    ));

    for (kind, labels) in &by_kind {
        lines.push(format!("  [{}]", kind));
        for label in labels {
            lines.push(format!("    {}", label));
        }
    }

    Ok(lines.join("\n"))
}

/// Build a compact graph context string for injection into the agent's
/// system prompt on the next turn. Queries all entities for the session,
/// traverses to depth 2, and compresses into a kind-grouped summary.
pub fn build_graph_context(conn: &Connection, session_id: &str) -> Result<String, String> {
    use rusqlite::params;

    let mut stmt = conn
        .prepare("SELECT id FROM entities WHERE session_id = ?1 ORDER BY id DESC LIMIT 50")
        .map_err(|e| format!("build_graph_context: {e}"))?;

    let ids: Vec<i64> = stmt
        .query_map(params![session_id], |row| row.get(0))
        .map_err(|e| format!("build_graph_context query: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    if ids.is_empty() {
        return Ok(String::new());
    }

    let trace = crate::extras::entity_search::traverse_from(conn, &ids, 2, None)?;

    if trace.is_empty() {
        return Ok(String::new());
    }

    let staleness = crate::extras::entity_db::entity_staleness_scores(conn, session_id)?;
    let compressed = compress_bundle_stale(conn, &trace, "session", &staleness)?;

    if compressed.is_empty() {
        return Ok(String::new());
    }

    Ok(format!("## Session Graph\n{}", compressed))
}

fn extract_terminal_kind(path: &str) -> String {
    path.rsplit('[')
        .next()
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::entity_db::*;
    use crate::extras::entity_search::traverse_from;
    use rusqlite::Connection;
    use rusqlite::params;

    fn setup_graph(conn: &Connection) {
        conn.execute_batch(
            "
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                started_at TEXT NOT NULL,
                last_active TEXT NOT NULL
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL DEFAULT '',
                timestamp TEXT NOT NULL
            );
            CREATE TABLE entities (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id),
                message_id INTEGER REFERENCES messages(id),
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                extra TEXT,
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
            "INSERT INTO sessions (id, started_at, last_active) VALUES ('ts', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('ts', 'tool', '', datetime('now'))",
            [],
        )
        .unwrap();
    }

    #[test]
    fn compress_empty_returns_empty_string() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);
        let result = compress_bundle(&conn, &[], "test").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn compress_bundle_groups_by_kind() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);

        let err1 = insert_entity(&conn, "ts", Some(1), "error", "E0308", None).unwrap();
        let err2 = insert_entity(&conn, "ts", Some(1), "error", "E0309", None).unwrap();
        let file1 = insert_entity(&conn, "ts", Some(1), "file", "src/main.rs", None).unwrap();
        insert_relation(&conn, err1, file1, "occurred_in", "ts").unwrap();
        insert_relation(&conn, err2, file1, "occurred_in", "ts").unwrap();

        let trace = traverse_from(&conn, &[err1, err2], 2, None).unwrap();
        assert!(trace.len() >= 3, "expected at least 3 trace nodes");

        let compressed = compress_bundle(&conn, &trace, "E0308").unwrap();
        // Should group: 2 errors, 1 file
        assert!(
            compressed.contains("error"),
            "expected error kind in output: {compressed}"
        );
        assert!(
            compressed.contains("file"),
            "expected file kind in output: {compressed}"
        );
        assert!(
            compressed.contains("E0308"),
            "expected entity name: {compressed}"
        );
    }

    #[test]
    fn compress_bundle_dedup_shortest_path() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);

        let err = insert_entity(&conn, "ts", Some(1), "error", "E0308", None).unwrap();
        let file = insert_entity(&conn, "ts", Some(1), "file", "src/main.rs", None).unwrap();
        // Direct relation error→file
        insert_relation(&conn, err, file, "occurred_in", "ts").unwrap();
        // Also a transitive path via another file (same terminal entity)
        let other = insert_entity(&conn, "ts", Some(1), "file", "src/lib.rs", None).unwrap();
        insert_relation(&conn, err, other, "touched_by", "ts").unwrap();
        insert_relation(&conn, other, file, "touched_by", "ts").unwrap();

        let trace = traverse_from(&conn, &[err], 3, None).unwrap();

        let compressed = compress_bundle(&conn, &trace, "E0308").unwrap();
        // The file entity appears at depth 1 and depth 2 — dedup keeps depth 1.
        // src/main.rs should appear once, not twice.
        let count = compressed.matches("src/main.rs").count();
        assert_eq!(
            count, 1,
            "expected src/main.rs exactly once, got {count}: {compressed}"
        );
    }

    #[test]
    fn build_graph_context_empty_session() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);
        let result = build_graph_context(&conn, "ts").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn build_graph_context_with_entities() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);

        let err = insert_entity(&conn, "ts", Some(1), "error", "E0308", None).unwrap();
        let file = insert_entity(&conn, "ts", Some(1), "file", "src/main.rs", None).unwrap();
        insert_relation(&conn, err, file, "occurred_in", "ts").unwrap();

        let result = build_graph_context(&conn, "ts").unwrap();
        assert!(!result.is_empty(), "expected non-empty context: {result}");
        assert!(
            result.contains("## Session Graph"),
            "expected header: {result}"
        );
        assert!(result.contains("E0308"), "expected entity name: {result}");
        assert!(
            result.contains("src/main.rs"),
            "expected related entity: {result}"
        );
    }

    #[test]
    fn build_graph_context_other_session_not_visible() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);
        conn.execute(
            "INSERT INTO sessions (id, started_at, last_active) VALUES ('other', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('other', 'tool', '', datetime('now'))",
            [],
        )
        .unwrap();

        insert_entity(&conn, "other", Some(1), "error", "E0308", None).unwrap();

        // Querying 'ts' should not see 'other' entities
        let result = build_graph_context(&conn, "ts").unwrap();
        assert!(result.is_empty(), "expected empty for 'ts': {result}");
    }

    #[test]
    fn build_graph_context_staleness_orders_recent_first() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);
        let sid = "staleness-session";
        conn.execute(
            "INSERT INTO sessions (id, started_at, last_active) VALUES (?1, datetime('now'), datetime('now'))",
            params![sid],
        )
        .unwrap();
        for _ in 1..=3 {
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp) VALUES (?1, 'tool', '', datetime('now'))",
                params![sid],
            )
            .unwrap();
        }

        // Old entity (message 1)
        let old_err = insert_entity(&conn, sid, Some(1), "error", "E0308", None).unwrap();
        let old_file = insert_entity(&conn, sid, Some(1), "file", "src/old.rs", None).unwrap();
        insert_relation(&conn, old_err, old_file, "occurred_in", sid).unwrap();

        // New entity (message 3)
        let new_err = insert_entity(&conn, sid, Some(3), "error", "E0422", None).unwrap();
        let new_file = insert_entity(&conn, sid, Some(3), "file", "src/new.rs", None).unwrap();
        insert_relation(&conn, new_err, new_file, "occurred_in", sid).unwrap();

        let result = build_graph_context(&conn, sid).unwrap();
        assert!(!result.is_empty());

        // New entity should appear before old entity in the staleness-sorted output
        let pos_old = result.find("E0308").unwrap();
        let pos_new = result.find("E0422").unwrap();
        assert!(
            pos_new < pos_old,
            "E0422 (newer) should come before E0308 (older), got:\n{result}"
        );
    }

    // ── Task 20: Benchmark harness ──────────────────────────────────────

    fn setup_multi_turn_session(conn: &Connection, sid: &str) {
        conn.execute(
            "INSERT INTO sessions (id, started_at, last_active) VALUES (?1, datetime('now'), datetime('now'))",
            params![sid],
        )
        .unwrap();
        for _turn in 1..=5 {
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp) VALUES (?1, 'tool', '', datetime('now'))",
                params![sid],
            )
            .unwrap();
        }
    }

    #[test]
    fn multi_turn_scenario_retains_error_history() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);
        let sid = "dirge-abc12345";
        setup_multi_turn_session(&conn, sid);

        // Turn 1 (msg 1): agent edits src/main.rs
        let main_rs =
            insert_entity(&conn, sid, Some(1), "file", "src/main.rs", Some("edited")).unwrap();

        // Turn 2 (msg 2): build records error E0308 in src/main.rs
        let e0308 =
            insert_entity(&conn, sid, Some(2), "error", "E0308", Some("type mismatch")).unwrap();
        insert_relation(&conn, e0308, main_rs, "occurred_in", sid).unwrap();

        // Turn 3 (msg 3): agent fixes the error (file entity re-recorded in FTS)
        let _fix = insert_entity(
            &conn,
            sid,
            Some(3),
            "file",
            "src/main.rs",
            Some("fixed E0308"),
        )
        .unwrap();
        insert_relation(&conn, _fix, e0308, "fixes", sid).unwrap();

        // Turn 4 (msg 4): build records warning W0301
        let w0301 = insert_entity(
            &conn,
            sid,
            Some(4),
            "warning",
            "W0301",
            Some("unused import"),
        )
        .unwrap();
        insert_relation(&conn, w0301, main_rs, "occurred_in", sid).unwrap();

        // Turn 5: build_graph_context should contain entities from all turns
        let context = build_graph_context(&conn, sid).unwrap();
        assert!(!context.is_empty(), "expected non-empty context");
        assert!(
            context.contains("E0308"),
            "context should mention the error from turn 2: {context}"
        );
        assert!(
            context.contains("W0301"),
            "context should mention the warning from turn 4: {context}"
        );
        assert!(
            context.contains("src/main.rs"),
            "context should mention the file: {context}"
        );
    }

    #[test]
    fn multi_turn_graph_context_is_compact() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);
        let sid = "dirge-compact-test";
        setup_multi_turn_session(&conn, sid);

        // Simulate 20 entities across 5 turns — typical real session
        let files = ["src/main.rs", "src/lib.rs", "tests/integration.rs"];
        let errors = ["E0308", "E0412", "E0597"];
        let warnings = ["W0301", "W0202"];

        let mut file_ids = Vec::new();
        for (i, f) in files.iter().enumerate() {
            let fid = insert_entity(&conn, sid, Some(1 + i as i64), "file", f, None).unwrap();
            file_ids.push(fid);
        }

        for (i, e) in errors.iter().enumerate() {
            let eid = insert_entity(&conn, sid, Some(2 + i as i64), "error", e, None).unwrap();
            let fid = file_ids[i % file_ids.len()];
            insert_relation(&conn, eid, fid, "occurred_in", sid).unwrap();
        }

        for (i, w) in warnings.iter().enumerate() {
            let wid = insert_entity(&conn, sid, Some(3 + i as i64), "warning", w, None).unwrap();
            let fid = file_ids[(i + 1) % file_ids.len()];
            insert_relation(&conn, wid, fid, "occurred_in", sid).unwrap();
        }

        let context = build_graph_context(&conn, sid).unwrap();
        assert!(!context.is_empty());

        let line_count = context.lines().count();
        // Context should be compact: ≤20 lines for 8 entities + relations
        assert!(
            line_count <= 20,
            "context too large ({line_count} lines): {context}"
        );

        // Token estimate: ~1 token/word. Context should be ≤200 tokens.
        let word_count = context.split_whitespace().count();
        assert!(
            word_count <= 200,
            "context too many words ({word_count}): {context}"
        );
    }

    #[test]
    fn traverse_from_scalability_1000_entities() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);
        let sid = "dirge-scalability-test";
        setup_multi_turn_session(&conn, sid);

        // Insert 1000 entities (100 per type × 10 types)
        let kinds = [
            "file", "error", "warning", "commit", "function", "module", "test", "doc", "config",
            "trait",
        ];
        let mut ids = Vec::with_capacity(1000);

        for kind_idx in 0..10 {
            for n in 0..100 {
                let name = format!("{}{}", kinds[kind_idx], n);
                let id = insert_entity(&conn, sid, Some(1), kinds[kind_idx], &name, None).unwrap();
                ids.push(id);
            }
        }
        assert_eq!(ids.len(), 1000);

        // Insert 2000 relations linking entities in a chain
        for i in 0..2000 {
            let src = ids[i % 1000];
            let tgt = ids[(i + 1) % 1000];
            let rel_type = if i % 2 == 0 {
                "occurred_in"
            } else {
                "touched_by"
            };
            insert_relation(&conn, src, tgt, rel_type, sid).unwrap();
        }

        // Seed with 10 entities
        let seeds: Vec<i64> = ids[0..10].to_vec();

        let start = std::time::Instant::now();
        let results = traverse_from(&conn, &seeds, 3, None).unwrap();
        let elapsed = start.elapsed();

        assert!(!results.is_empty(), "expected traversal results");
        assert!(
            elapsed.as_millis() < 50,
            "traverse_from with 1000 entities/2000 relations took {}ms, expected <50ms",
            elapsed.as_millis()
        );
    }

    #[test]
    fn compress_bundle_stale_sorts_newest_first() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);

        // oldest → mid → newest
        let old = insert_entity(&conn, "ts", Some(1), "file", "old.rs", None).unwrap();
        let mid = insert_entity(&conn, "ts", Some(1), "file", "mid.rs", None).unwrap();
        let new = insert_entity(&conn, "ts", Some(1), "error", "E0308", None).unwrap();
        insert_relation(&conn, new, old, "occurred_in", "ts").unwrap();
        insert_relation(&conn, new, mid, "touched_by", "ts").unwrap();

        let trace = traverse_from(&conn, &[new], 2, None).unwrap();

        let scores = entity_staleness_scores(&conn, "ts").unwrap();
        let compressed = compress_bundle_stale(&conn, &trace, "test", &scores).unwrap();

        // Newest entities (higher id) should appear first within each kind group.
        // Check that the file kind lists mid.rs before old.rs
        let mid_pos = compressed.find("mid.rs").unwrap();
        let old_pos = compressed.find("old.rs").unwrap();
        assert!(
            mid_pos < old_pos,
            "expected mid.rs (newer) before old.rs (older), got:\n{compressed}"
        );
    }

    #[test]
    fn compress_bundle_stale_summarizes_older_when_many() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);

        let err = insert_entity(&conn, "ts", Some(1), "error", "E0308", None).unwrap();
        // Create 5 file entities — detail_limit is 3, so 2 should be summarized
        let files: Vec<i64> = (0..5)
            .map(|i| {
                insert_entity(&conn, "ts", Some(1), "file", &format!("file{}.rs", i), None).unwrap()
            })
            .collect();
        for &f in &files {
            insert_relation(&conn, err, f, "occurred_in", "ts").unwrap();
        }

        let trace = traverse_from(&conn, &[err], 2, None).unwrap();
        let scores = entity_staleness_scores(&conn, "ts").unwrap();
        let compressed = compress_bundle_stale(&conn, &trace, "test", &scores).unwrap();

        // Should show the newest 3 file entities in full
        assert!(
            compressed.contains("file4.rs"),
            "newest file should be shown: {compressed}"
        );
        // Should summarize the remaining 2
        assert!(
            compressed.contains("and 2 older file"),
            "expected 'and 2 older file' summary, got:\n{compressed}"
        );
    }

    #[test]
    fn compress_bundle_stale_empty_staleness_no_sort() {
        let conn = Connection::open_in_memory().unwrap();
        setup_graph(&conn);

        let err = insert_entity(&conn, "ts", Some(1), "error", "E0308", None).unwrap();
        let file = insert_entity(&conn, "ts", Some(1), "file", "src/main.rs", None).unwrap();
        insert_relation(&conn, err, file, "occurred_in", "ts").unwrap();

        let trace = traverse_from(&conn, &[err], 2, None).unwrap();
        // Empty staleness map — should not crash, fall back to insertion order
        let compressed = compress_bundle_stale(&conn, &trace, "test", &HashMap::new()).unwrap();
        assert!(compressed.contains("E0308"));
        assert!(compressed.contains("src/main.rs"));
    }
}
