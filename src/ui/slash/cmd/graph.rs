//! /graph handler — query the entity/relation graph (#393).
//!
//! Subcommands:
//!   /graph search <query> [--kind <kind>] [--compress] [--tier auto|sql|walk|llm]
//!   /graph traverse <id> [--depth <n>] [--compress] [--tier auto|sql|walk|llm]
//!
//! The `--tier` flag forces a specific routing path; `auto` (default) uses
//! keyword-based heuristics in `entity_router::route_query`.

use std::path::Path;

use crate::extras::dirge_paths::ProjectPaths;
use crate::extras::session_db::SessionDb;
#[cfg(feature = "experimental-graph-search")]
use crate::ui::slash::c_result;
use crate::ui::slash::{SlashCtx, c_agent, c_error};

#[cfg(feature = "experimental-graph-search")]
/// Format a timestamp for display. Trims the date portion, keeps time.
fn short_time(ts: &str) -> &str {
    ts.split_once(' ').map(|(_, rest)| rest).unwrap_or(ts)
}

#[cfg(feature = "experimental-graph-search")]
/// Extract `--tier <value>` from a slice of parts. Returns the tier string if present.
fn extract_tier<'a>(parts: &[&'a str]) -> Option<&'a str> {
    parts
        .iter()
        .position(|s| *s == "--tier")
        .and_then(|i| parts.get(i + 1))
        .copied()
}

pub(crate) async fn cmd_graph(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let sub = parts.get(1).copied().unwrap_or("").trim();

    match sub {
        "search" => {
            let remainder = &parts[2..];
            let query = remainder
                .iter()
                .take_while(|s| **s != "--kind" && **s != "--tier")
                .copied()
                .collect::<Vec<_>>()
                .join(" ");

            if query.is_empty() {
                ctx.renderer.write_line(
                    "/graph search <query> [--kind <kind>] [--compress] [--tier auto|sql|walk|llm]",
                    c_agent(),
                )?;
                return Ok(());
            }

            let kind = remainder
                .iter()
                .position(|s| *s == "--kind")
                .and_then(|i| remainder.get(i + 1))
                .copied();

            let paths = ProjectPaths::new(Path::new(ctx.session.working_dir.as_str()));
            let db = match SessionDb::open(&paths.session_db_path()) {
                Ok(d) => d,
                Err(e) => {
                    ctx.renderer
                        .write_line(&format!("session db open failed: {e}"), c_error())?;
                    return Ok(());
                }
            };

            #[cfg(feature = "experimental-graph-search")]
            {
                let compress = remainder.contains(&"--compress");
                let tier = extract_tier(parts);
                let resolved_tier = match tier {
                    Some("sql") => crate::extras::entity_router::QueryTier::DirectSql,
                    Some("walk") => crate::extras::entity_router::QueryTier::GraphWalk,
                    Some("llm") => crate::extras::entity_router::QueryTier::Llm,
                    _ => crate::extras::entity_router::route_query(&query),
                };

                if matches!(resolved_tier, crate::extras::entity_router::QueryTier::Llm) {
                    ctx.renderer.write_line(
                        "LLM tier not yet implemented — use --tier sql or --tier walk",
                        c_error(),
                    )?;
                    return Ok(());
                }

                if matches!(
                    resolved_tier,
                    crate::extras::entity_router::QueryTier::GraphWalk
                ) {
                    // FTS5 search first, then traverse from each result
                    let _ = kind;
                    let rows = match crate::extras::entity_db::search_entities(
                        &db.conn, &query, kind, None, 20,
                    ) {
                        Ok(r) => r,
                        Err(e) => {
                            ctx.renderer
                                .write_line(&format!("search error: {e}"), c_error())?;
                            return Ok(());
                        }
                    };
                    if rows.is_empty() {
                        ctx.renderer.write_line("no entities found", c_agent())?;
                        return Ok(());
                    }
                    let seed_ids: Vec<i64> = rows.iter().map(|(id, ..)| *id).collect();
                    let depth: u32 = parts
                        .iter()
                        .position(|s| *s == "--depth")
                        .and_then(|i| parts.get(i + 1))
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(3);
                    match crate::extras::entity_search::traverse_from(
                        &db.conn, &seed_ids, depth, None,
                    ) {
                        Ok(trace) if trace.is_empty() => {
                            ctx.renderer
                                .write_line("no edges from results", c_agent())?;
                        }
                        Ok(trace) => {
                            if compress {
                                match crate::extras::entity_compress::compress_bundle(
                                    &db.conn, &trace, &query,
                                ) {
                                    Ok(summary) => {
                                        for line in summary.lines() {
                                            ctx.renderer.write_line(line, c_result())?;
                                        }
                                    }
                                    Err(e) => {
                                        ctx.renderer.write_line(
                                            &format!("compress error: {e}"),
                                            c_error(),
                                        )?;
                                    }
                                }
                            } else {
                                for (_id, path, d) in &trace {
                                    ctx.renderer
                                        .write_line(&format!("  d={d}  {path}"), c_result())?;
                                }
                                ctx.renderer
                                    .write_line(&format!("{} nodes", trace.len()), c_agent())?;
                            }
                        }
                        Err(e) => {
                            ctx.renderer
                                .write_line(&format!("traverse error: {e}"), c_error())?;
                        }
                    }
                } else {
                    // DirectSql: FTS5 only
                    let _ = kind;
                    match crate::extras::entity_db::search_entities(
                        &db.conn, &query, kind, None, 20,
                    ) {
                        Ok(rows) if rows.is_empty() => {
                            ctx.renderer.write_line("no entities found", c_agent())?;
                        }
                        Ok(rows) => {
                            if compress {
                                match crate::extras::entity_compress::compress_search_results(
                                    &db.conn, &rows, &query,
                                ) {
                                    Ok(summary) => {
                                        for line in summary.lines() {
                                            ctx.renderer.write_line(line, c_result())?;
                                        }
                                    }
                                    Err(e) => {
                                        ctx.renderer.write_line(
                                            &format!("compress error: {e}"),
                                            c_error(),
                                        )?;
                                    }
                                }
                            } else {
                                for (id, _sid, ek, ename, extra, ts) in &rows {
                                    let extra_str = extra
                                        .as_deref()
                                        .map(|e| format!("  {}", e))
                                        .unwrap_or_default();
                                    ctx.renderer.write_line(
                                        &format!(
                                            "#{id}  {short}  {ek}/{ename}{extra_str}",
                                            short = short_time(ts),
                                        ),
                                        c_result(),
                                    )?;
                                }
                                ctx.renderer
                                    .write_line(&format!("{} results", rows.len()), c_agent())?;
                            }
                        }
                        Err(e) => {
                            ctx.renderer
                                .write_line(&format!("search error: {e}"), c_error())?;
                        }
                    }
                }
            }
            #[cfg(not(feature = "experimental-graph-search"))]
            {
                let _ = (db, query, kind);
                ctx.renderer
                    .write_line("experimental-graph-search feature not enabled", c_error())?;
            }
        }

        "traverse" => {
            let seed_str = parts.get(2).copied().unwrap_or("");
            let seed_id: i64 = match seed_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    ctx.renderer.write_line(
                        "/graph traverse <entity-id> [--depth <n>] [--compress] [--tier auto|sql|walk|llm]",
                        c_agent(),
                    )?;
                    return Ok(());
                }
            };

            let depth: u32 = parts
                .iter()
                .position(|s| *s == "--depth")
                .and_then(|i| parts.get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(3);

            let paths = ProjectPaths::new(Path::new(ctx.session.working_dir.as_str()));
            let db = match SessionDb::open(&paths.session_db_path()) {
                Ok(d) => d,
                Err(e) => {
                    ctx.renderer
                        .write_line(&format!("session db open failed: {e}"), c_error())?;
                    return Ok(());
                }
            };

            #[cfg(feature = "experimental-graph-search")]
            {
                let compress = parts.contains(&"--compress");
                let tier = extract_tier(parts);
                let resolved_tier = match tier {
                    Some("sql") => crate::extras::entity_router::QueryTier::DirectSql,
                    Some("walk") => crate::extras::entity_router::QueryTier::GraphWalk,
                    Some("llm") => crate::extras::entity_router::QueryTier::Llm,
                    _ => crate::extras::entity_router::QueryTier::GraphWalk,
                };

                if matches!(resolved_tier, crate::extras::entity_router::QueryTier::Llm) {
                    ctx.renderer.write_line(
                        "LLM tier not yet implemented — use --tier sql or --tier walk",
                        c_error(),
                    )?;
                    return Ok(());
                }

                if matches!(
                    resolved_tier,
                    crate::extras::entity_router::QueryTier::DirectSql
                ) {
                    // Direct entity lookup by id
                    let row: Result<(String, String, Option<String>, String), _> =
                        db.conn.query_row(
                            "SELECT kind, name, extra, created_at FROM entities WHERE id = ?1",
                            [seed_id],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                        );
                    match row {
                        Ok((ek, ename, extra, ts)) => {
                            let extra_str = extra
                                .as_deref()
                                .map(|e| format!("  {}", e))
                                .unwrap_or_default();
                            ctx.renderer.write_line(
                                &format!(
                                    "#{seed_id}  {short}  {ek}/{ename}{extra_str}",
                                    short = short_time(&ts),
                                ),
                                c_result(),
                            )?;
                        }
                        Err(_) => {
                            ctx.renderer
                                .write_line(&format!("entity #{seed_id} not found"), c_agent())?;
                        }
                    }
                } else {
                    let _ = depth;
                    match crate::extras::entity_search::traverse_from(
                        &db.conn,
                        &[seed_id],
                        depth,
                        None,
                    ) {
                        Ok(rows) if rows.is_empty() => {
                            ctx.renderer.write_line(
                                &format!("entity #{seed_id} not found or no edges"),
                                c_agent(),
                            )?;
                        }
                        Ok(rows) => {
                            if compress {
                                match crate::extras::entity_compress::compress_bundle(
                                    &db.conn, &rows, "",
                                ) {
                                    Ok(summary) => {
                                        for line in summary.lines() {
                                            ctx.renderer.write_line(line, c_result())?;
                                        }
                                    }
                                    Err(e) => {
                                        ctx.renderer.write_line(
                                            &format!("compress error: {e}"),
                                            c_error(),
                                        )?;
                                    }
                                }
                            } else {
                                for (_id, path, d) in &rows {
                                    ctx.renderer
                                        .write_line(&format!("  d={d}  {path}"), c_result())?;
                                }
                                ctx.renderer
                                    .write_line(&format!("{} nodes", rows.len()), c_agent())?;
                            }
                        }
                        Err(e) => {
                            ctx.renderer
                                .write_line(&format!("traverse error: {e}"), c_error())?;
                        }
                    }
                }
            }
            #[cfg(not(feature = "experimental-graph-search"))]
            {
                let _ = (db, seed_id, depth);
                ctx.renderer
                    .write_line("experimental-graph-search feature not enabled", c_error())?;
            }
        }

        "" => {
            ctx.renderer.write_line(
                "/graph search <query> [--kind <kind>] [--compress] [--tier auto|sql|walk|llm]",
                c_agent(),
            )?;
            ctx.renderer.write_line(
                "/graph traverse <id> [--depth <n>] [--compress] [--tier auto|sql|walk|llm]",
                c_agent(),
            )?;
            ctx.renderer.write_line("", c_agent())?;
            ctx.renderer
                .write_line("  --tier auto   keyword-based routing (default)", c_agent())?;
            ctx.renderer
                .write_line("  --tier sql    force FTS5 entity search", c_agent())?;
            ctx.renderer
                .write_line("  --tier walk   force graph traversal", c_agent())?;
            ctx.renderer.write_line(
                "  --tier llm    LLM decomposition (not yet implemented)",
                c_agent(),
            )?;
            ctx.renderer.write_line("", c_agent())?;
            ctx.renderer
                .write_line("requires experimental-graph-search feature", c_agent())?;
        }

        other => {
            ctx.renderer
                .write_line(&format!("unknown /graph sub-command: {other}"), c_error())?;
        }
    }

    Ok(())
}
