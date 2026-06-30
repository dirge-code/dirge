//! Adaptive intent routing for entity graph queries (#393, N4).
//!
//! `route_query` classifies a natural-language query into a `QueryTier`
//! so the server can skip expensive operations when possible. The default
//! routing heuristics are keyword-based — no LLM required.
//!
//! When the LLM tier is selected explicitly, the caller should decompose
//! the query externally; the router's job is just to say "this needs an LLM."

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryTier {
    /// FTS5 direct search (entity by name/kind).
    DirectSql,
    /// Recursive CTE traversal from seed entities.
    GraphWalk,
    /// Defer to an LLM call for structured decomposition (not yet implemented).
    Llm,
}

/// Relation-type keywords that signal a graph-walk query.
static RELATION_KEYWORDS: &[&str] = &[
    "depends on",
    "touched by",
    "touches",
    "occurred in",
    "occurs in",
    "related to",
    "connected to",
    "what depends",
    "what touches",
    "what files",
    "what errors",
    "which files",
    "which errors",
];

/// True when `query` looks like a direct entity reference: numeric id,
/// short name with no relation words, or a specific identifier pattern.
fn looks_like_entity_ref(query: &str) -> bool {
    let trimmed = query.trim();

    // #42 style entity-id reference
    if trimmed.starts_with('#') && trimmed[1..].chars().all(|c| c.is_ascii_digit()) {
        return true;
    }

    // Single token (likely an exact name like "E0308")
    if trimmed.split_whitespace().count() == 1 {
        return true;
    }

    false
}

fn contains_relation_keyword(query: &str) -> bool {
    let lower = query.to_lowercase();
    RELATION_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// Classify a query into the cheapest tier that can answer it.
///
/// Heuristics (no LLM):
/// - Contains relation keywords ("depends on", "touched by", etc.) → GraphWalk
/// - Looks like a specific entity reference (#42, single token) → DirectSql
/// - Default → DirectSql
pub fn route_query(query: &str) -> QueryTier {
    if contains_relation_keyword(query) {
        return QueryTier::GraphWalk;
    }
    if looks_like_entity_ref(query) {
        return QueryTier::DirectSql;
    }
    // Fallback: cheapest path
    QueryTier::DirectSql
}

/// Parse a `--tier` value string.
#[allow(dead_code)]
pub fn parse_tier(s: &str) -> Option<QueryTier> {
    match s {
        "auto" => None, // caller resolves via route_query
        "sql" => Some(QueryTier::DirectSql),
        "walk" => Some(QueryTier::GraphWalk),
        "llm" => Some(QueryTier::Llm),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_ref_triggers_direct_sql() {
        assert_eq!(route_query("#42"), QueryTier::DirectSql);
        assert_eq!(route_query("E0308"), QueryTier::DirectSql);
        assert_eq!(route_query("src/main.rs"), QueryTier::DirectSql);
    }

    #[test]
    fn relation_keyword_triggers_graph_walk() {
        assert_eq!(
            route_query("what errors occurred in src/main.rs"),
            QueryTier::GraphWalk
        );
        assert_eq!(
            route_query("which files are touched by commit abc123"),
            QueryTier::GraphWalk
        );
        assert_eq!(
            route_query("what depends on dirge-core"),
            QueryTier::GraphWalk
        );
    }

    #[test]
    fn ambiguous_query_falls_back_to_direct_sql() {
        assert_eq!(route_query("show me everything"), QueryTier::DirectSql);
    }

    #[test]
    fn parse_tier_variants() {
        assert_eq!(parse_tier("sql"), Some(QueryTier::DirectSql));
        assert_eq!(parse_tier("walk"), Some(QueryTier::GraphWalk));
        assert_eq!(parse_tier("llm"), Some(QueryTier::Llm));
        assert_eq!(parse_tier("auto"), None);
        assert_eq!(parse_tier("garbage"), None);
    }
}
