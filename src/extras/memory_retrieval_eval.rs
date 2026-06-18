//! Memory retrieval-quality eval harness.
//!
//! Companion to `agent::compaction_recall`. That harness asks "does a fact
//! survive being SUMMARIZED?"; this one asks "does a stored fact survive being
//! SEARCHED FOR?" — the other half of memory fidelity. It's idea #5 from the
//! Elastic agent-memory write-up
//! (elastic.co/search-labs/blog/agent-memory-elasticsearch): generate
//! plausible queries per stored memory and measure whether the retriever
//! surfaces the source entry in the top K (Recall@K).
//!
//! The corpus pairs each entry with two query sets:
//!   * `lexical_queries` — share words with the entry, so BM25 should find them.
//!   * `semantic_queries` — paraphrases with no shared content words, the case
//!     keyword search structurally misses.
//!
//! The scorer ([`recall_at_k`]) is pure and takes a pluggable `search`
//! closure (query → ranked entry contents), so the SAME corpus measures any
//! retrieval backend: today's BM25 [`SqliteMemoryStore::search_entries`], and
//! later a hybrid dense+rerank provider — the number this harness reports is
//! the BM25 baseline that hybrid retrieval has to beat (dirge-4hld).
//!
//! cfg(test) like `compaction_recall`: it runs as part of `cargo test`, and
//! the corpus/scorer are the reusable pieces a future real-embedder run wires
//! its own `search` closure into.

#![cfg(test)]

use crate::extras::dirge_paths::ProjectPaths;
use crate::extras::memory_db::{MemoryKind, SqliteMemoryStore};

/// One stored memory plus the queries that SHOULD retrieve it.
struct MemoryProbe {
    content: &'static str,
    kind: MemoryKind,
    /// Queries sharing words with `content` — BM25's home turf.
    lexical_queries: &'static [&'static str],
    /// Paraphrases with no shared content words — the keyword-search blind spot
    /// that a semantic/hybrid retriever is meant to close.
    semantic_queries: &'static [&'static str],
}

/// A small, dirge-flavored corpus of project facts. Contents are distinct
/// enough that a query's target is unambiguous, and the semantic queries are
/// deliberately built from synonyms absent from the entry so BM25's implicit
/// AND (every >2-char token must appear) structurally misses them.
fn seed_corpus() -> Vec<MemoryProbe> {
    vec![
        MemoryProbe {
            content: "build the project with cargo build --bin dirge",
            kind: MemoryKind::Procedural,
            lexical_queries: &["cargo build", "build the project"],
            semantic_queries: &["compile the executable"],
        },
        MemoryProbe {
            content: "run the test suite with cargo test --bin dirge",
            kind: MemoryKind::Procedural,
            lexical_queries: &["cargo test", "run the test suite"],
            semantic_queries: &["execute unit checks"],
        },
        MemoryProbe {
            content: "the project pins its MSRV in rust-toolchain.toml",
            kind: MemoryKind::Semantic,
            lexical_queries: &["MSRV rust toolchain"],
            semantic_queries: &["minimum supported language baseline"],
        },
        MemoryProbe {
            content: "format all code with cargo fmt before committing",
            kind: MemoryKind::Procedural,
            lexical_queries: &["cargo fmt", "format code before committing"],
            semantic_queries: &["tidy whitespace and indentation"],
        },
        MemoryProbe {
            content: "long-term memory persists in SQLite at .dirge/sessions/state.db",
            kind: MemoryKind::Semantic,
            lexical_queries: &["SQLite memory persists", "state.db"],
            semantic_queries: &["where recollections are saved"],
        },
        MemoryProbe {
            content: "the main agent loop lives in src/agent/agent_loop.rs",
            kind: MemoryKind::Semantic,
            lexical_queries: &["agent loop", "agent_loop.rs"],
            semantic_queries: &["primary control cycle location"],
        },
        MemoryProbe {
            content: "secrets are redacted before FTS indexing of messages",
            kind: MemoryKind::Semantic,
            lexical_queries: &["redacted FTS indexing", "secrets"],
            semantic_queries: &["credentials scrubbed from search"],
        },
        MemoryProbe {
            content: "use bd beads for issue tracking not markdown TODO lists",
            kind: MemoryKind::Procedural,
            lexical_queries: &["beads issue tracking", "markdown TODO"],
            semantic_queries: &["how to file a ticket"],
        },
    ]
}

/// Flatten the corpus into `(query, target_content)` pairs for one query set.
fn pairs(corpus: &[MemoryProbe], semantic: bool) -> Vec<(&'static str, &'static str)> {
    corpus
        .iter()
        .flat_map(|p| {
            let qs = if semantic {
                p.semantic_queries
            } else {
                p.lexical_queries
            };
            qs.iter().map(move |q| (*q, p.content))
        })
        .collect()
}

/// Recall@K outcome over a set of `(query, target)` pairs.
struct RecallReport {
    k: usize,
    total: usize,
    hits: usize,
    /// `(query, target)` pairs whose target did NOT appear in the top K.
    misses: Vec<(String, String)>,
}

impl RecallReport {
    fn recall(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.hits as f64 / self.total as f64
    }
}

/// Score Recall@K: for each `(query, target)`, does `search(query)` surface
/// `target` within the top K ranked results? `search` returns ranked entry
/// contents — the one pluggable piece, so BM25 and a future hybrid retriever
/// are measured by the identical corpus and scorer.
fn recall_at_k(
    search: impl Fn(&str) -> Vec<String>,
    pairs: &[(&'static str, &'static str)],
    k: usize,
) -> RecallReport {
    let mut hits = 0;
    let mut misses = Vec::new();
    for (query, target) in pairs {
        let ranked = search(query);
        if ranked.iter().take(k).any(|c| c == target) {
            hits += 1;
        } else {
            misses.push(((*query).to_string(), (*target).to_string()));
        }
    }
    RecallReport {
        k,
        total: pairs.len(),
        hits,
        misses,
    }
}

// ── Test wiring ──────────────────────────────────────────────────────

fn temp_project() -> (ProjectPaths, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "dirge-retrieval-eval-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    (ProjectPaths::new(&dir), dir)
}

/// Build a store seeded with the corpus and return a `search` closure over its
/// BM25 retrieval — the production retrieval path the baseline measures.
fn seeded_bm25_search(corpus: &[MemoryProbe]) -> (SqliteMemoryStore, std::path::PathBuf) {
    let (paths, dir) = temp_project();
    let store = SqliteMemoryStore::load(&paths).unwrap();
    for p in corpus {
        store.add_entry("memory", p.content, Some(p.kind)).unwrap();
    }
    (store, dir)
}

fn ranked_contents(store: &SqliteMemoryStore, query: &str) -> Vec<String> {
    let resp = store.search_entries(query).unwrap();
    resp["results"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .filter_map(|r| r["content"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn harness_credits_a_perfect_retriever() {
    // A retriever that always returns the exact target first scores 1.0 — the
    // scorer's own sanity check, independent of any store.
    let corpus = seed_corpus();
    let all = pairs(&corpus, false);
    let report = recall_at_k(|q| vec![target_for(&corpus, q).to_string()], &all, 5);
    assert_eq!(report.recall(), 1.0, "perfect retriever must score 1.0");
    assert!(report.misses.is_empty());
}

#[test]
fn harness_flags_a_blind_retriever() {
    // A retriever that returns nothing scores 0.0 and reports every pair as a
    // miss — so a regression to empty results can't masquerade as success.
    let corpus = seed_corpus();
    let all = pairs(&corpus, false);
    let report = recall_at_k(|_q| Vec::new(), &all, 5);
    assert_eq!(report.recall(), 0.0);
    assert_eq!(report.misses.len(), all.len());
}

/// BM25 BASELINE: keyword queries that share words with their entry are
/// recovered at high Recall@5. This is the number hybrid retrieval (dirge-4hld)
/// must beat — established here so the comparison is grounded, not asserted.
#[test]
fn bm25_baseline_recovers_lexical_queries() {
    let corpus = seed_corpus();
    let (store, dir) = seeded_bm25_search(&corpus);
    let lexical = pairs(&corpus, false);
    let report = recall_at_k(|q| ranked_contents(&store, q), &lexical, 5);
    assert!(
        report.recall() >= 0.85,
        "BM25 lexical Recall@{} = {:.2} (baseline); misses: {:?}",
        report.k,
        report.recall(),
        report.misses,
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// BM25 PARAPHRASE GAP: queries phrased with synonyms the entry doesn't
/// contain are structurally missed by keyword search. This documents the gap a
/// semantic/hybrid retriever exists to close — and proves the harness actually
/// detects retrieval failures rather than always passing.
#[test]
fn bm25_has_a_paraphrase_gap() {
    let corpus = seed_corpus();
    let (store, dir) = seeded_bm25_search(&corpus);
    let lexical = recall_at_k(|q| ranked_contents(&store, q), &pairs(&corpus, false), 5);
    let semantic = recall_at_k(|q| ranked_contents(&store, q), &pairs(&corpus, true), 5);
    assert!(
        semantic.recall() < lexical.recall(),
        "paraphrase recall ({:.2}) must trail lexical ({:.2}) — the gap hybrid closes",
        semantic.recall(),
        lexical.recall(),
    );
    assert!(
        !semantic.misses.is_empty(),
        "the harness must surface concrete paraphrase misses for diagnosis",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Lookup helper for the perfect-retriever test: the target content a query
/// belongs to.
fn target_for<'a>(corpus: &'a [MemoryProbe], query: &str) -> &'a str {
    corpus
        .iter()
        .find(|p| p.lexical_queries.contains(&query) || p.semantic_queries.contains(&query))
        .map(|p| p.content)
        // Fail loudly rather than returning "" — a silent empty target would
        // turn a corpus typo into a never-matching phantom "hit".
        .unwrap_or_else(|| panic!("query {query:?} is not registered in the corpus"))
}

// ── Hybrid retriever, measured by the SAME scorer (dirge-4hld) ────────

/// A deterministic stand-in for a real embedding model, scoped to THIS corpus:
/// each entry's distinguishing terms and each paraphrase query's synonyms map
/// to a shared concept dimension, so a query lands near its target without any
/// lexical overlap. Generic/shared tokens (`cargo`, `rust`, `agent`, `search`,
/// `file`) are deliberately unmapped so entries don't bleed into each other.
/// It plays the role `compaction_recall`'s faithful mock summarizer does — the
/// real semantic quality is for an off-CI run with a real embedder; this proves
/// the FUSION recovers paraphrases when the embedder has signal.
struct ConceptEmbedder;

impl ConceptEmbedder {
    fn concept(word: &str) -> Option<usize> {
        Some(match word.to_lowercase().as_str() {
            "build" | "compile" | "executable" => 0,
            "test" | "tests" | "suite" | "unit" | "checks" => 1,
            "msrv" | "toolchain" | "minimum" | "supported" | "language" | "baseline" => 2,
            "format" | "fmt" | "tidy" | "whitespace" | "indentation" => 3,
            "memory" | "persists" | "sqlite" | "recollections" | "saved" => 4,
            "loop" | "control" | "cycle" | "primary" => 5,
            "secrets" | "redacted" | "credentials" | "scrubbed" => 6,
            "beads" | "issue" | "tracking" | "ticket" => 7,
            _ => return None,
        })
    }
}

impl crate::extras::memory_hybrid::Embedder for ConceptEmbedder {
    fn embed(&self, texts: &[String]) -> Vec<Option<Vec<f32>>> {
        texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; 8];
                let mut any = false;
                for word in t.split(|c: char| !c.is_alphanumeric()) {
                    if let Some(d) = Self::concept(word) {
                        v[d] += 1.0;
                        any = true;
                    }
                }
                any.then_some(v)
            })
            .collect()
    }
}

fn provider_ranked_contents(
    provider: &dyn crate::extras::memory_provider::MemoryProvider,
    query: &str,
) -> Vec<String> {
    let resp = provider.search(query).unwrap();
    resp["results"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .filter_map(|r| r["content"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The headline result of dirge-4hld measured on the SAME corpus + scorer as
/// the BM25 baseline: hybrid (dense fused with BM25) recovers the paraphrase
/// queries that pure BM25 structurally misses, so its semantic Recall@5 beats
/// BM25's. This is the "number hybrid has to beat" actually being beaten —
/// driven by a deterministic embedder so it runs in CI.
#[test]
fn hybrid_beats_bm25_on_the_paraphrase_corpus() {
    use std::sync::Arc;
    let corpus = seed_corpus();
    let (paths, dir) = temp_project();
    let store = Arc::new(SqliteMemoryStore::load(&paths).unwrap());
    for p in &corpus {
        store.add_entry("memory", p.content, Some(p.kind)).unwrap();
    }

    let semantic = pairs(&corpus, true);
    // BM25 baseline over the paraphrase queries — structurally ~0 (no shared
    // tokens, implicit AND).
    let bm25 = recall_at_k(|q| ranked_contents(&store, q), &semantic, 5);

    // Same corpus, same scorer, hybrid retriever with the concept embedder.
    let hybrid = crate::extras::memory_hybrid::HybridMemoryProvider::new(
        store.clone(),
        Arc::new(ConceptEmbedder),
    );
    let hybrid_report = recall_at_k(|q| provider_ranked_contents(&hybrid, q), &semantic, 5);

    assert!(
        hybrid_report.recall() > bm25.recall(),
        "hybrid paraphrase Recall@5 ({:.2}) must beat BM25 ({:.2}); hybrid misses: {:?}",
        hybrid_report.recall(),
        bm25.recall(),
        hybrid_report.misses,
    );
    // With a clean concept signal the fusion should recover essentially all of
    // them — guard against a fusion regression that only partially helps.
    assert!(
        hybrid_report.recall() >= 0.85,
        "hybrid should recover the paraphrases given embedder signal: {:.2}",
        hybrid_report.recall(),
    );
    let _ = std::fs::remove_dir_all(&dir);
}
