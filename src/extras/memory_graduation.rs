//! Recurrence-weighted salience graduation (dirge-4nix).
//!
//! When the SAME learning has been stored as near-duplicate memory entries
//! (a recurring lesson), boost the representative entry's salience so it's
//! more likely to be retained and surfaced — WITHOUT merging or deleting
//! anything. The existing LLM consolidation pass still owns dedup; this is
//! a salience nudge, not consolidation.
//!
//! All functions here are pure and heavily unit-tested. No DB access.

use std::collections::BTreeSet;

/// Conservative: near-identical, not merely related.
pub(crate) const MIN_JACCARD: f64 = 0.80;
/// Skip trivially-short entries that make Jaccard noisy.
pub(crate) const MIN_TOKENS: usize = 8;
/// A learning stored ≥2 times has recurred.
pub(crate) const MIN_CLUSTER_SIZE: usize = 2;

/// A cluster of near-duplicate entries that represent one recurring learning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecurrenceCluster {
    pub rep_uid: String,
    pub member_uids: Vec<String>,
    pub size: usize,
    /// Stable content-hash of the cluster (ledger key).
    pub hash: String,
}

/// Minimal input view for clustering — testable without a DB.
#[derive(Debug, Clone)]
pub(crate) struct GraduationInput {
    pub uid: String,
    pub target: String,
    pub content: String,
    pub use_count: i64,
    pub created_at: String,
}

/// Normalize content to a token SET: lowercase, split on any
/// non-alphanumeric, drop empties, collect into a BTreeSet.
fn tokens(content: &str) -> BTreeSet<String> {
    content
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Jaccard similarity of two token sets: |A∩B| / |A∪B|; 0.0 if both empty.
fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    intersection as f64 / union as f64
}

/// Group entries into near-duplicate clusters.
///
/// - Only entries with the SAME `target` may cluster together.
/// - Skip entries with fewer than `min_tokens` tokens.
/// - Greedy single pass for determinism: iterate entries in a STABLE order
///   (sort by uid first); an entry joins the first existing cluster whose
///   REPRESENTATIVE has jaccard >= `min_jaccard`, else it starts a new cluster.
/// - Representative = member with highest use_count, tie-broken by newest
///   created_at, then uid for total determinism.
/// - Return ONLY clusters with size >= `min_size`.
/// - `hash` = hex sha256 of the sorted-by-uid member `content`s joined by "\n".
pub(crate) fn recurrence_clusters(
    entries: &[GraduationInput],
    min_jaccard: f64,
    min_tokens: usize,
    min_size: usize,
) -> Vec<RecurrenceCluster> {
    // Stable sort: uid first for deterministic iteration order.
    let mut sorted: Vec<&GraduationInput> = entries.iter().collect();
    sorted.sort_by(|a, b| a.uid.cmp(&b.uid));

    // Precompute token sets, filtering short entries.
    let token_sets: Vec<(&GraduationInput, BTreeSet<String>)> =
        sorted.iter().map(|e| (*e, tokens(&e.content))).collect();

    // Group: list of (entries, representative_index)
    let mut clusters: Vec<Vec<usize>> = Vec::new();

    for (i, (_entry, tok)) in token_sets.iter().enumerate() {
        if tok.len() < min_tokens {
            continue;
        }
        let entry_target = &token_sets[i].0.target;
        // Find first cluster whose representative is jaccard-close and same target.
        let mut found = false;
        for cluster in clusters.iter_mut() {
            let rep_idx = cluster[0]; // representative is first member (we'll reorder later)
            let rep_target = &token_sets[rep_idx].0.target;
            if rep_target != entry_target {
                continue;
            }
            let rep_tokens = &token_sets[rep_idx].1;
            if jaccard(rep_tokens, tok) >= min_jaccard {
                cluster.push(i);
                found = true;
                break;
            }
        }
        if !found {
            clusters.push(vec![i]);
        }
    }

    // Build result: select representative for each cluster, compute hash.
    clusters
        .into_iter()
        .filter(|c| c.len() >= min_size)
        .map(|indices| {
            // Representative: highest use_count, tie-break newest created_at, then uid.
            let rep_idx = *indices
                .iter()
                .max_by(|&&a, &&b| {
                    let ea = token_sets[a].0;
                    let eb = token_sets[b].0;
                    ea.use_count
                        .cmp(&eb.use_count)
                        .then_with(|| ea.created_at.cmp(&eb.created_at))
                        .then_with(|| ea.uid.cmp(&eb.uid))
                })
                .unwrap();
            let rep = token_sets[rep_idx].0;

            let mut member_uids: Vec<String> = indices
                .iter()
                .map(|&i| token_sets[i].0.uid.clone())
                .collect();

            // Hash: sorted-by-uid member contents joined by "\n".
            let mut pairs: Vec<(&str, &str)> = indices
                .iter()
                .map(|&i| {
                    (
                        token_sets[i].0.uid.as_str(),
                        token_sets[i].0.content.as_str(),
                    )
                })
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0));
            let hash_input: String = pairs
                .iter()
                .map(|(_, content)| *content)
                .collect::<Vec<&str>>()
                .join("\n");
            let hash = {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(hash_input.as_bytes());
                format!("{:x}", hasher.finalize())
            };

            member_uids.sort();

            RecurrenceCluster {
                rep_uid: rep.uid.clone(),
                member_uids,
                size: indices.len(),
                hash,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── tokens ────────────────────────────────────────────

    #[test]
    fn tokens_lowercases_and_splits_on_non_alphanumeric() {
        let t = tokens("Hello, World! Foo-Bar");
        assert!(t.contains("hello"));
        assert!(t.contains("world"));
        assert!(t.contains("foo"));
        assert!(t.contains("bar"));
        assert!(!t.contains("Hello"));
    }

    #[test]
    fn tokens_deduplicates() {
        let t = tokens("a a b b c");
        assert_eq!(t.len(), 3);
        assert!(t.contains("a"));
        assert!(t.contains("b"));
        assert!(t.contains("c"));
    }

    #[test]
    fn tokens_empty_string_is_empty_set() {
        assert!(tokens("").is_empty());
    }

    // ── jaccard ───────────────────────────────────────────

    #[test]
    fn jaccard_identical_is_one() {
        let a = tokens("foo bar baz");
        let b = tokens("foo bar baz");
        assert!((jaccard(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn jaccard_disjoint_is_zero() {
        let a = tokens("foo bar");
        let b = tokens("baz qux");
        assert!((jaccard(&a, &b) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn jaccard_partial_overlap() {
        let a = tokens("foo bar baz");
        let b = tokens("foo bar qux");
        // intersection = {foo, bar} = 2, union = {foo, bar, baz, qux} = 4
        assert!((jaccard(&a, &b) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn jaccard_both_empty_is_zero() {
        let a: BTreeSet<String> = BTreeSet::new();
        let b: BTreeSet<String> = BTreeSet::new();
        assert!((jaccard(&a, &b) - 0.0).abs() < 1e-9);
    }

    // ── clustering ────────────────────────────────────────

    fn make_entry(
        uid: &str,
        target: &str,
        content: &str,
        use_count: i64,
        created_at: &str,
    ) -> GraduationInput {
        GraduationInput {
            uid: uid.to_string(),
            target: target.to_string(),
            content: content.to_string(),
            use_count,
            created_at: created_at.to_string(),
        }
    }

    #[test]
    fn two_near_identical_entries_cluster() {
        let entries = vec![
            make_entry(
                "a",
                "memory",
                "dirge uses SQLite for long-term memory storage",
                1,
                "2025-01-01T00:00:00Z",
            ),
            make_entry(
                "b",
                "memory",
                "dirge uses SQLite for long term memory storage",
                2,
                "2025-01-02T00:00:00Z",
            ),
        ];
        let clusters = recurrence_clusters(&entries, MIN_JACCARD, MIN_TOKENS, MIN_CLUSTER_SIZE);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].size, 2);
        // b has higher use_count → representative
        assert_eq!(clusters[0].rep_uid, "b");
    }

    #[test]
    fn unrelated_entries_do_not_cluster() {
        let entries = vec![
            make_entry(
                "a",
                "memory",
                "dirge uses SQLite for long-term memory storage",
                1,
                "2025-01-01T00:00:00Z",
            ),
            make_entry(
                "b",
                "memory",
                "the sky is blue and the grass is green",
                1,
                "2025-01-02T00:00:00Z",
            ),
        ];
        let clusters = recurrence_clusters(&entries, MIN_JACCARD, MIN_TOKENS, MIN_CLUSTER_SIZE);
        assert!(clusters.is_empty());
    }

    #[test]
    fn different_target_never_clusters() {
        let entries = vec![
            make_entry(
                "a",
                "memory",
                "dirge uses SQLite for long-term memory storage",
                1,
                "2025-01-01T00:00:00Z",
            ),
            make_entry(
                "b",
                "pitfalls",
                "dirge uses SQLite for long-term memory storage",
                1,
                "2025-01-02T00:00:00Z",
            ),
        ];
        let clusters = recurrence_clusters(&entries, MIN_JACCARD, MIN_TOKENS, MIN_CLUSTER_SIZE);
        assert!(clusters.is_empty());
    }

    #[test]
    fn short_entry_below_min_tokens_never_clusters() {
        let entries = vec![
            make_entry("a", "memory", "short", 1, "2025-01-01T00:00:00Z"),
            make_entry("b", "memory", "short", 1, "2025-01-02T00:00:00Z"),
        ];
        // "short" has 1 token, below MIN_TOKENS=8
        let clusters = recurrence_clusters(&entries, MIN_JACCARD, MIN_TOKENS, MIN_CLUSTER_SIZE);
        assert!(clusters.is_empty());
    }

    #[test]
    fn representative_by_use_count_then_created_at() {
        let entries = vec![
            make_entry(
                "low",
                "memory",
                "this is a recurring lesson about testing methodology",
                1,
                "2025-01-01T00:00:00Z",
            ),
            make_entry(
                "high",
                "memory",
                "this is a recurring lesson about testing methodology indeed",
                5,
                "2025-01-01T00:00:00Z",
            ),
            make_entry(
                "mid",
                "memory",
                "this is a recurring lesson about testing methodology yes",
                3,
                "2025-01-01T00:00:00Z",
            ),
        ];
        let clusters = recurrence_clusters(&entries, MIN_JACCARD, MIN_TOKENS, MIN_CLUSTER_SIZE);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].rep_uid, "high");
        assert_eq!(clusters[0].size, 3);
    }

    #[test]
    fn representative_tie_break_by_newest_created_at() {
        let entries = vec![
            make_entry(
                "a",
                "memory",
                "this is a recurring lesson about testing methodology",
                2,
                "2025-01-01T00:00:00Z",
            ),
            make_entry(
                "b",
                "memory",
                "this is a recurring lesson about testing methodology indeed",
                2,
                "2025-01-03T00:00:00Z",
            ),
        ];
        let clusters = recurrence_clusters(&entries, MIN_JACCARD, MIN_TOKENS, MIN_CLUSTER_SIZE);
        assert_eq!(clusters.len(), 1);
        // same use_count, b has newer created_at
        assert_eq!(clusters[0].rep_uid, "b");
    }

    #[test]
    fn singletons_excluded() {
        let entries = vec![
            make_entry(
                "a",
                "memory",
                "this is a unique fact about project structure",
                1,
                "2025-01-01T00:00:00Z",
            ),
            make_entry(
                "b",
                "memory",
                "something completely different here",
                1,
                "2025-01-02T00:00:00Z",
            ),
        ];
        let clusters = recurrence_clusters(&entries, MIN_JACCARD, MIN_TOKENS, MIN_CLUSTER_SIZE);
        assert!(clusters.is_empty());
    }

    #[test]
    fn hash_stable_across_input_reorderings() {
        let content =
            "dirge uses SQLite for long term persistent storage of memory entries and facts";
        let a = make_entry("a", "memory", content, 1, "2025-01-01T00:00:00Z");
        let b = make_entry(
            "b",
            "memory",
            "dirge uses SQLite for long term persistent storage of memory entries",
            2,
            "2025-01-02T00:00:00Z",
        );
        let entries1 = vec![a.clone(), b.clone()];
        let entries2 = vec![b, a];
        let c1 = recurrence_clusters(&entries1, MIN_JACCARD, MIN_TOKENS, MIN_CLUSTER_SIZE);
        let c2 = recurrence_clusters(&entries2, MIN_JACCARD, MIN_TOKENS, MIN_CLUSTER_SIZE);
        assert_eq!(c1[0].hash, c2[0].hash);
    }

    #[test]
    fn hash_differs_when_membership_differs() {
        let base = "dirge uses SQLite for long term persistent storage of memory entries";
        let a = make_entry("a", "memory", base, 1, "2025-01-01T00:00:00Z");
        let b = make_entry(
            "b",
            "memory",
            "dirge uses SQLite for long term persistent storage of memory entries and facts",
            2,
            "2025-01-02T00:00:00Z",
        );
        let c = make_entry(
            "c",
            "memory",
            "dirge uses SQLite for long term persistent storage of memory entries plus more",
            3,
            "2025-01-03T00:00:00Z",
        );
        // All three cluster: both calls return 1 cluster, but hashes differ.
        let hash_abc = recurrence_clusters(
            &[a.clone(), b.clone(), c.clone()],
            MIN_JACCARD,
            MIN_TOKENS,
            MIN_CLUSTER_SIZE,
        )[0]
        .hash
        .clone();
        let hash_ab = recurrence_clusters(&[a, b], MIN_JACCARD, MIN_TOKENS, MIN_CLUSTER_SIZE)[0]
            .hash
            .clone();
        assert_ne!(hash_abc, hash_ab);
    }
}
