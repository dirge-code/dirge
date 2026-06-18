//! Hybrid memory retrieval: dense recall fused with BM25 (dirge-4hld).
//!
//! Idea #1 from the Elastic agent-memory write-up. The builtin
//! [`SqliteMemoryStore`] search is BM25 only — strong on exact tokens
//! (paths, error codes, identifiers), blind to paraphrase ("compile the
//! binary" never matches a stored "build the project"). The retrieval eval
//! (`memory_retrieval_eval`) pins that gap: lexical recall is high, paraphrase
//! recall is ~0.
//!
//! [`HybridMemoryProvider`] wraps the builtin store and an [`Embedder`]. It
//! delegates every operation to the inner store EXCEPT `search`, which it
//! overrides to run dense cosine ranking over all active entries and fuse it
//! with the inner BM25 ranking via Reciprocal Rank Fusion. BM25 keeps its
//! exact-match precision; dense recall recovers the paraphrases. When no
//! embedder signal is available (the query won't embed), search falls back to
//! the inner BM25 result verbatim — so a misconfigured embedder degrades to
//! today's behavior rather than breaking retrieval.
//!
//! Wiring is opt-in (config `memory.hybrid_retrieval` + an embedder backend);
//! the default build never constructs this and is unchanged.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::memory_db::SqliteMemoryStore;
use super::memory_provider::MemoryProvider;

/// Max fused results returned — matches the builtin `SEARCH_RESULT_LIMIT`.
const HYBRID_RESULT_LIMIT: usize = 8;

/// BM25 candidates pulled into the fusion. Over-fetching past the
/// [`HYBRID_RESULT_LIMIT`] keeps the two fusion legs symmetric — dense ranking
/// already sees every active entry, so clipping BM25 to 8 would let a
/// lexically-relevant entry ranked just past the cut vanish from the BM25 leg
/// (Elastic over-fetches each leg for the same reason). Memory stores are
/// small, so 50 covers the whole corpus in practice.
const FUSION_BM25_POOL: usize = 50;

/// Reciprocal Rank Fusion constant. The classic default (60); larger values
/// flatten the contribution of top ranks, smaller ones sharpen it. Fusing two
/// short lists, the exact value matters little, but it's the documented knob.
const RRF_K: f64 = 60.0;

/// Produces a dense vector for each input text, preserving order. A text that
/// can't be embedded yields `None` and is simply absent from dense ranking
/// (it can still surface via BM25) — so a transient backend error degrades
/// recall instead of failing the search.
pub trait Embedder: Send + Sync {
    fn embed(&self, texts: &[String]) -> Vec<Option<Vec<f32>>>;
}

/// Cosine similarity in [-1, 1]; 0 when either vector has zero norm or the
/// lengths differ (a defensive guard — a real embedder returns fixed-width
/// vectors, but a misconfigured one shouldn't panic the search path).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Reciprocal Rank Fusion of several ranked key lists (each best-first). A
/// key's fused score is the sum over lists of `1/(k + rank)` (rank 0-based);
/// keys absent from a list contribute nothing from it. Returns keys ordered by
/// descending fused score, ties broken by first appearance for determinism.
pub fn rrf_fuse(rankings: &[Vec<String>], k: f64) -> Vec<String> {
    let mut score: HashMap<&str, f64> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for ranking in rankings {
        for (rank, key) in ranking.iter().enumerate() {
            let e = score.entry(key.as_str()).or_insert_with(|| {
                order.push(key.as_str());
                0.0
            });
            *e += 1.0 / (k + rank as f64);
        }
    }
    // Stable sort over first-appearance order keeps ties deterministic.
    order.sort_by(|a, b| {
        score[b]
            .partial_cmp(&score[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    order.into_iter().map(str::to_string).collect()
}

/// FNV-1a 64-bit — keys the per-content embedding cache so repeated searches
/// don't re-embed unchanged entries.
fn content_key(content: &str) -> u64 {
    crate::hash::fnv64(content.as_bytes())
}

/// A memory provider that fuses the inner store's BM25 search with dense
/// embedding recall. All non-search operations delegate to `inner`.
pub struct HybridMemoryProvider {
    inner: Arc<SqliteMemoryStore>,
    embedder: Arc<dyn Embedder>,
    /// content-hash → embedding, so entries are embedded once across searches.
    cache: Mutex<HashMap<u64, Vec<f32>>>,
}

impl HybridMemoryProvider {
    pub fn new(inner: Arc<SqliteMemoryStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self {
            inner,
            embedder,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Embed `contents`, serving hits from the cache and only calling the
    /// embedder for the misses. Returns a vector per input (order preserved);
    /// an un-embeddable entry stays `None`.
    fn embed_cached(&self, contents: &[String]) -> Vec<Option<Vec<f32>>> {
        let mut out: Vec<Option<Vec<f32>>> = vec![None; contents.len()];
        let mut miss_idx: Vec<usize> = Vec::new();
        {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            for (i, c) in contents.iter().enumerate() {
                if let Some(v) = cache.get(&content_key(c)) {
                    out[i] = Some(v.clone());
                } else {
                    miss_idx.push(i);
                }
            }
        }
        if miss_idx.is_empty() {
            return out;
        }
        let miss_texts: Vec<String> = miss_idx.iter().map(|&i| contents[i].clone()).collect();
        let fresh = self.embedder.embed(&miss_texts);
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        for (slot, vec) in miss_idx.into_iter().zip(fresh) {
            if let Some(v) = vec {
                cache.insert(content_key(&contents[slot]), v.clone());
                out[slot] = Some(v);
            }
        }
        out
    }

    /// Dense ranking of all active entries against `query`, best-first, by
    /// cosine. Empty when the query can't be embedded — the signal the caller
    /// uses to fall back to BM25-only.
    fn dense_ranking(&self, query: &str, rows: &[Value]) -> Vec<String> {
        let qvec = match self.embedder.embed(&[query.to_string()]).into_iter().next() {
            Some(Some(v)) => v,
            _ => return Vec::new(),
        };
        let contents: Vec<String> = rows
            .iter()
            .map(|r| r["content"].as_str().unwrap_or_default().to_string())
            .collect();
        let embs = self.embed_cached(&contents);

        let mut scored: Vec<(f32, &str)> = rows
            .iter()
            .zip(embs.iter())
            .filter_map(|(row, emb)| {
                let id = row["id"].as_str()?;
                let v = emb.as_ref()?;
                Some((cosine(&qvec, v), id))
            })
            .collect();
        // Descending similarity; stable enough for ranking purposes.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().map(|(_, id)| id.to_string()).collect()
    }
}

impl MemoryProvider for HybridMemoryProvider {
    fn name(&self) -> &str {
        "hybrid"
    }

    fn format_for_system_prompt(&self) -> String {
        self.inner.format_for_system_prompt()
    }

    fn view(&self, target: &str) -> Value {
        MemoryProvider::view(self.inner.as_ref(), target)
    }

    fn add(&self, target: &str, content: &str, kind: Option<&str>) -> Result<Value, String> {
        MemoryProvider::add(self.inner.as_ref(), target, content, kind)
    }

    fn replace(
        &self,
        target: &str,
        old_text: &str,
        content: &str,
        kind: Option<&str>,
    ) -> Result<Value, String> {
        MemoryProvider::replace(self.inner.as_ref(), target, old_text, content, kind)
    }

    fn supersede(
        &self,
        target: &str,
        old_text: &str,
        content: &str,
        kind: Option<&str>,
        harsh: bool,
    ) -> Result<Value, String> {
        MemoryProvider::supersede(self.inner.as_ref(), target, old_text, content, kind, harsh)
    }

    fn remove(&self, target: &str, old_text: &str) -> Result<Value, String> {
        MemoryProvider::remove(self.inner.as_ref(), target, old_text)
    }

    fn restore(&self, target: &str, old_text: &str) -> Result<Value, String> {
        MemoryProvider::restore(self.inner.as_ref(), target, old_text)
    }

    fn expand(&self, old_text: &str) -> Result<Value, String> {
        MemoryProvider::expand(self.inner.as_ref(), old_text)
    }

    fn record_outcome(&self, target: &str, old_text: &str, success: bool) -> Result<Value, String> {
        MemoryProvider::record_outcome(self.inner.as_ref(), target, old_text, success)
    }

    /// BM25 ∪ dense, fused with RRF. Falls back to the inner BM25 result
    /// verbatim when the query can't be embedded.
    fn search(&self, query: &str) -> Result<Value, String> {
        // Over-fetch the BM25 leg so fusion sees more than the final cap.
        let bm25 = self.inner.search_entries_limited(query, FUSION_BM25_POOL)?;
        let bm25_ranked: Vec<String> = bm25["results"]
            .as_array()
            .map(|rs| {
                rs.iter()
                    .filter_map(|r| r["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let rows = self.inner.active_search_rows()?;
        let dense_ranked = self.dense_ranking(query, &rows);

        let by_id: HashMap<&str, &Value> = rows
            .iter()
            .filter_map(|r| r["id"].as_str().map(|id| (id, r)))
            .collect();

        let ordered: Vec<String> = if dense_ranked.is_empty() {
            // No embedder signal — identical ordering to builtin BM25.
            bm25_ranked
        } else {
            rrf_fuse(&[bm25_ranked, dense_ranked], RRF_K)
        };
        let results: Vec<Value> = ordered
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).map(|v| (*v).clone()))
            .take(HYBRID_RESULT_LIMIT)
            .collect();

        Ok(serde_json::json!({
            "success": true,
            "query": query,
            "count": results.len(),
            "results": results,
        }))
    }

    // Lifecycle hooks delegate so write-time behavior (and any future
    // write-time indexing on the inner store) is preserved. CRUD above does
    // NOT self-fire on_memory_write (dirge-5feg) — the tool layer fires it.
    fn on_memory_write(&self, action: &str, target: &str, payload: &str) {
        self.inner.on_memory_write(action, target, payload);
    }

    fn on_session_end(&self, transcript: &str) {
        self.inner.on_session_end(transcript);
    }

    fn on_session_switch(&self, new_session_id: &str, parent_session_id: &str, reset: bool) {
        self.inner
            .on_session_switch(new_session_id, parent_session_id, reset);
    }

    fn on_pre_compress(&self, transcript: &str) -> String {
        self.inner.on_pre_compress(transcript)
    }
}

/// Default embedding model when the config names none — an OpenAI-compatible
/// id; override via `memory.embed_model` for a different backend.
pub const DEFAULT_EMBED_MODEL: &str = "text-embedding-3-small";

/// Per-request embeddings timeout. Bounds the worst case so a hung endpoint
/// can't stall a memory search (and thus the agent turn) indefinitely — on
/// timeout the request errors, the batch degrades to `None`, and search falls
/// back to BM25.
const EMBED_TIMEOUT_SECS: u64 = 10;

/// Construct an [`Embedder`] backed by an OpenAI-compatible `/v1/embeddings`
/// endpoint. `api_key` is `None` for keyless local servers (e.g. a local
/// embedding gateway). Returns `None` if the worker thread or HTTP client can't
/// be created — so the caller keeps the BM25-only store instead of losing the
/// whole memory subsystem (the spawn is fallible, and "degrade to BM25" must
/// hold even when the OS refuses a thread).
pub fn api_embedder(
    url: String,
    model: String,
    api_key: Option<String>,
) -> Option<Arc<dyn Embedder>> {
    match ApiEmbedder::new(url, model, api_key) {
        Ok(e) => Some(Arc::new(e)),
        Err(err) => {
            tracing::warn!(target: "dirge::memory_hybrid", error = %err, "embedder unavailable — staying BM25-only");
            None
        }
    }
}

/// Parse an OpenAI-compatible embeddings response into one optional vector per
/// input (indexed by the response's `index` field, so order scrambling is
/// tolerated). Pure, so the JSON contract is unit-tested without a network.
fn parse_embeddings(body: &Value, n: usize) -> Vec<Option<Vec<f32>>> {
    let mut out = vec![None; n];
    let Some(data) = body["data"].as_array() else {
        return out;
    };
    for (pos, item) in data.iter().enumerate() {
        // Prefer the explicit index; fall back to position.
        let idx = item["index"].as_u64().map(|i| i as usize).unwrap_or(pos);
        let emb: Option<Vec<f32>> = item["embedding"].as_array().map(|a| {
            a.iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect()
        });
        if idx < n
            && let Some(e) = emb
            && !e.is_empty()
        {
            out[idx] = Some(e);
        }
    }
    out
}

async fn fetch_embeddings(
    client: &reqwest::Client,
    url: &str,
    model: &str,
    api_key: &Option<String>,
    texts: &[String],
) -> Vec<Option<Vec<f32>>> {
    let mut req = client
        .post(url)
        .json(&serde_json::json!({ "model": model, "input": texts }));
    if let Some(k) = api_key {
        req = req.bearer_auth(k);
    }
    match req.send().await {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(body) => parse_embeddings(&body, texts.len()),
            Err(e) => {
                tracing::warn!(target: "dirge::memory_hybrid", error = %e, "embeddings response parse failed");
                vec![None; texts.len()]
            }
        },
        Err(e) => {
            tracing::warn!(target: "dirge::memory_hybrid", error = %e, "embeddings request failed");
            vec![None; texts.len()]
        }
    }
}

/// An [`Embedder`] over an HTTP embeddings API. `MemoryProvider::search` is
/// sync but called from inside the async runtime, and dirge ships only the
/// async reqwest client — so the HTTP runs on a DEDICATED worker thread with
/// its own current-thread runtime, and `embed` hands work over a channel and
/// blocks on the reply. This avoids the nested-runtime panic that
/// `Handle::block_on` would hit, and keeps the async-only HTTP stack usable
/// from the sync trait. Any failure degrades to `None` (BM25-only recall),
/// never an error.
struct ApiEmbedder {
    tx: std::sync::mpsc::Sender<EmbedJob>,
}

struct EmbedJob {
    texts: Vec<String>,
    reply: std::sync::mpsc::Sender<Vec<Option<Vec<f32>>>>,
}

impl ApiEmbedder {
    fn new(url: String, model: String, api_key: Option<String>) -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<EmbedJob>();
        // Build the HTTP client up front so a bad TLS/config surfaces here
        // (→ caller stays BM25-only) rather than per-request.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(EMBED_TIMEOUT_SECS))
            .build()
            .map_err(std::io::Error::other)?;
        std::thread::Builder::new()
            .name("dirge-embedder".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!(target: "dirge::memory_hybrid", error = %e, "embedder runtime build failed");
                        return;
                    }
                };
                while let Ok(job) = rx.recv() {
                    let result =
                        rt.block_on(fetch_embeddings(&client, &url, &model, &api_key, &job.texts));
                    let _ = job.reply.send(result);
                }
            })?;
        Ok(Self { tx })
    }
}

impl Embedder for ApiEmbedder {
    fn embed(&self, texts: &[String]) -> Vec<Option<Vec<f32>>> {
        let (reply, rx) = std::sync::mpsc::channel();
        let job = EmbedJob {
            texts: texts.to_vec(),
            reply,
        };
        if self.tx.send(job).is_err() {
            return vec![None; texts.len()];
        }
        // Bounded a hair above the per-request HTTP timeout so a wedged worker
        // can't park the calling thread forever; a timeout degrades to None.
        let wait = std::time::Duration::from_secs(EMBED_TIMEOUT_SECS + 5);
        rx.recv_timeout(wait)
            .unwrap_or_else(|_| vec![None; texts.len()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::dirge_paths::ProjectPaths;
    use crate::extras::memory_db::MemoryKind;

    #[test]
    fn parse_embeddings_maps_by_index_and_tolerates_gaps() {
        let body = serde_json::json!({
            "data": [
                {"index": 1, "embedding": [0.1, 0.2]},
                {"index": 0, "embedding": [0.3, 0.4]},
            ]
        });
        let out = parse_embeddings(&body, 3);
        assert_eq!(out[0], Some(vec![0.3, 0.4]), "index 0 placed correctly");
        assert_eq!(out[1], Some(vec![0.1, 0.2]), "out-of-order index respected");
        assert_eq!(out[2], None, "missing index stays None");
        // Malformed / empty bodies degrade to all-None, never panic.
        assert_eq!(
            parse_embeddings(&serde_json::json!({}), 2),
            vec![None, None]
        );
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        // Defensive: mismatched / zero vectors score 0, not NaN/panic.
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn rrf_rewards_agreement_and_merges_disjoint() {
        // B ranks high in both lists → should win over A (top of only one).
        let l1 = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let l2 = vec!["B".to_string(), "D".to_string(), "A".to_string()];
        let fused = rrf_fuse(&[l1, l2], RRF_K);
        assert_eq!(fused[0], "B", "agreed-upon key ranks first: {fused:?}");
        // Every key from both lists survives the fusion.
        for k in ["A", "B", "C", "D"] {
            assert!(fused.contains(&k.to_string()), "{k} missing: {fused:?}");
        }
    }

    /// A tiny concept embedder over the test vocabulary: each word maps to a
    /// fixed basis vector, and a text embeds to the (normalized) sum of its
    /// words' vectors. Synonyms share a basis dimension, so "compile" and
    /// "build" land near each other — a stand-in for real semantic embeddings,
    /// exactly as `compaction_recall` uses a faithful mock summarizer.
    struct ConceptEmbedder;

    impl ConceptEmbedder {
        fn concept(word: &str) -> Option<usize> {
            // Dimension index per concept; synonyms collapse to one dim.
            let w = word.to_lowercase();
            let dim = match w.as_str() {
                "build" | "compile" | "compiling" => 0,
                "project" | "binary" | "executable" => 1,
                "test" | "tests" | "testing" | "suite" => 2,
                "format" | "formatting" | "tidy" | "whitespace" | "indentation" => 3,
                "memory" | "recall" | "recollections" | "remember" => 4,
                "store" | "stored" | "persist" | "persists" | "saved" | "sqlite" => 5,
                _ => return None,
            };
            Some(dim)
        }
    }

    impl Embedder for ConceptEmbedder {
        fn embed(&self, texts: &[String]) -> Vec<Option<Vec<f32>>> {
            texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; 6];
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

    fn temp_store() -> (Arc<SqliteMemoryStore>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "dirge-hybrid-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let store = SqliteMemoryStore::load(&ProjectPaths::new(&dir)).unwrap();
        (Arc::new(store), dir)
    }

    fn ids(resp: &Value) -> Vec<String> {
        resp["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["content"].as_str().unwrap().to_string())
            .collect()
    }

    /// The headline claim: hybrid recovers a paraphrase query that pure BM25
    /// misses. "compile the binary" shares no token with "build the project",
    /// so BM25 returns nothing; the concept embedder puts them adjacent, so
    /// the fused result surfaces the entry.
    #[test]
    fn hybrid_recovers_a_paraphrase_bm25_misses() {
        let (store, dir) = temp_store();
        store
            .add_entry("memory", "build the project", Some(MemoryKind::Procedural))
            .unwrap();
        store
            .add_entry("memory", "run the test suite", Some(MemoryKind::Procedural))
            .unwrap();

        // BM25 alone: paraphrase has no shared token → no hit.
        let bm25 = store.search_entries("compile the binary").unwrap();
        assert!(
            !ids(&bm25).iter().any(|c| c == "build the project"),
            "precondition: BM25 misses the paraphrase",
        );

        // Hybrid: dense recall surfaces it.
        let hybrid = HybridMemoryProvider::new(store.clone(), Arc::new(ConceptEmbedder));
        let resp = hybrid.search("compile the binary").unwrap();
        assert!(
            ids(&resp).iter().any(|c| c == "build the project"),
            "hybrid must recover the paraphrase: {:?}",
            ids(&resp),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hybrid must not LOSE exact-match precision: a lexical query whose target
    /// BM25 ranks first still surfaces (fusion includes the BM25 list).
    #[test]
    fn hybrid_preserves_lexical_hits() {
        let (store, dir) = temp_store();
        store
            .add_entry("memory", "build the project", Some(MemoryKind::Procedural))
            .unwrap();
        store
            .add_entry("memory", "run the test suite", Some(MemoryKind::Procedural))
            .unwrap();
        let hybrid = HybridMemoryProvider::new(store.clone(), Arc::new(ConceptEmbedder));
        let resp = hybrid.search("test suite").unwrap();
        assert!(
            ids(&resp).iter().any(|c| c == "run the test suite"),
            "lexical hit preserved under fusion: {:?}",
            ids(&resp),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no embedder signal (every embed → None), search returns the inner
    /// BM25 result verbatim — graceful degradation to today's behavior.
    #[test]
    fn falls_back_to_bm25_without_embedder_signal() {
        struct NullEmbedder;
        impl Embedder for NullEmbedder {
            fn embed(&self, texts: &[String]) -> Vec<Option<Vec<f32>>> {
                vec![None; texts.len()]
            }
        }
        let (store, dir) = temp_store();
        store
            .add_entry("memory", "build the project", Some(MemoryKind::Procedural))
            .unwrap();
        let hybrid = HybridMemoryProvider::new(store.clone(), Arc::new(NullEmbedder));
        let bm25 = store.search_entries("build the project").unwrap();
        let hybrid_resp = hybrid.search("build the project").unwrap();
        assert_eq!(
            ids(&bm25),
            ids(&hybrid_resp),
            "null embedder → BM25 verbatim"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CRUD and reads delegate to the inner store.
    #[test]
    fn delegates_non_search_to_inner() {
        let (store, dir) = temp_store();
        let hybrid = HybridMemoryProvider::new(store.clone(), Arc::new(ConceptEmbedder));
        assert_eq!(hybrid.name(), "hybrid");
        hybrid.add("memory", "a delegated fact", None).unwrap();
        let view = MemoryProvider::view(&hybrid, "memory");
        assert!(
            view["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e.as_str().unwrap().contains("delegated fact")),
            "add routed to inner and is visible via view",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
