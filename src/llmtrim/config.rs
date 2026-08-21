//! Pipeline configuration + embedded assets.
//!
//! Config resolves from `LLMTRIM_PRESET` / a `preset = "<name>"` key (a named profile) or the
//! per-stage flags in a TOML file (`LLMTRIM_CONFIG` or the platform config dir); with neither,
//! the default is `auto` (shape-routing). See [`DenseConfig::load`].

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The Stage D format legend, embedded at build time and injected into the prompt
/// so the model can read the columnar encoding (always include a legend).
/// Validated non-empty by `build.rs`.
pub const FORMAT_LEGEND: &str = include_str!("prompts/toon_legend.txt");

/// Per-stage enable flags and knobs. `DenseConfig::default()` is **`auto`** (shape-routing) —
/// the shipped default, matching `load()` and the live interceptor. The **lossless** baseline
/// (only quality-neutral lossless input compression: `hygiene`, `serialize`, exact-duplicate
/// `dedup`) is [`DenseConfig::lossless`] (= the `safe` preset). `auto` also turns on the lossy
/// stages the eval shows quality-safe — output control on every
/// shape, image downscale (quality-neutral by construction), and retrieve / skeleton / dedup /
/// tools per shape. The bar is cost-at-no-measured-quality-loss, **not** losslessness; a stage
/// is gated to opt-in only when it is *unmeasured* or *shown to regress*, not for being lossy.
#[derive(Debug, Clone, Serialize, Deserialize)]
// Explicit per-stage flags in a config file layer over the lossless baseline (auto off), not
// over the `auto` default — a file that sets only `hygiene = false` must not silently enable
// shape-routing. (The runtime default when there is *no* file is still `auto`; see `load`.)
#[serde(default = "DenseConfig::lossless")]
pub struct DenseConfig {
    /// Stage D — lossless data hygiene (minify, numeric trim, base64/data-URI strip).
    pub hygiene: bool,
    /// Stage D — columnar (TOON) serialization of uniform record arrays.
    pub serialize: bool,
    /// Stage F — request-shaping output controls (terse instruction, max_tokens).
    /// Opt-in: it changes the model's output behavior (visible to the caller), and
    /// its output-side savings aren't measured live by the offline gate.
    pub output_control: bool,
    /// Minimum rows a uniform array must have before columnar encoding is attempted.
    pub serialize_min_rows: usize,
    /// Stage F — optional hard output-token cap, imposed only when the request has none.
    pub output_max_tokens: Option<u64>,
    /// Stage F — output-control tier: `"terse"` (clean) or `"draft"` (Chain-of-Draft
    /// reasoning).
    pub output_level: String,
    /// Stage F — soft output-token budget injected into the prompt ("answer within
    /// N tokens"); complements the hard `output_max_tokens` cap.
    pub output_token_budget: Option<u64>,
    /// Stage F — instruct the model to emit minified code (arXiv:2508.13666; model-gated).
    pub output_compact_code: bool,
    /// Stage F — inject the agent-loop frugality directive on tool-call-shaped requests
    /// (steer trajectory toward info-per-call). Opt-in, model-gated; not in any preset until
    /// a full agent bench confirms it.
    pub output_frugal_tools: bool,
    /// Stage F — inject the anti-overthinking directive (arXiv:2606.00206) on prose requests
    /// that declare BOTH a quantized serving tier and a reasoning pass. Bench: gpt-oss-20b,
    /// n=40, −62.0% output tokens, quality retention +0.0pp. See `stages::output`.
    pub output_anti_overthink: bool,
    /// Stage D — also encode uniform arrays nested inside content JSON, not only
    /// when the whole content is an array.
    pub serialize_nested: bool,
    /// Stage D — encode a top-level uniform flat array as CSV instead of TOON (opt-in).
    pub serialize_csv: bool,
    /// Stage D — flatten nested-uniform records to dotted columns (`meta.region`) before
    /// columnar encoding. Information-preserving, structurally reshaped; opt-in.
    pub serialize_flatten: bool,
    /// Stage D — partition a heterogeneous record array into uniform groups by shape,
    /// each emitted as its own TOON table. Regroups rows; opt-in.
    pub serialize_buckets: bool,
    /// Stage — lossy down-sampling of record arrays longer than `json_crush_max_rows`:
    /// keep first/last + outliers (errors / rare values) + a query-biased sample.
    pub json_crush: bool,
    /// Row cap a record array is sampled down to when `json_crush` is on.
    pub json_crush_max_rows: usize,
    /// Stage D — strip embedded base64 blobs / `data:` URIs (≥200-char runs → a
    /// `[base64 elided: N]` marker). Lossy, but measured quality-neutral (+0.0pp on
    /// `bench/data/base64.jsonl`), so on in the `auto` presets; `safe` keeps blobs.
    pub strip_base64: bool,
    /// Stage D — opt-in lossy: round float numbers to this many significant figures.
    pub numeric_sig_figs: Option<u32>,
    /// Stage D — tokenizer-aware Unicode normalization: drop invisible/format waste,
    /// fold no-break spaces, NFKC-canonicalize. Meaning-preserving but not byte-
    /// reversible, so opt-in. Universal (biggest wins on non-ASCII / pasted text).
    pub normalize_unicode: bool,
    /// Stage B — lexical retrieval (BM25/TextRank top-k chunk selection). Lossy;
    /// off by default until the live quality gate exists.
    pub retrieve: bool,
    /// Stage B — fraction of chunks to keep when retrieving (0.0–1.0).
    pub retrieve_keep_ratio: f64,
    /// Stage B — only content segments at least this many chars are eligible for
    /// pruning; shorter segments are treated as the query.
    pub retrieve_min_segment_chars: usize,
    /// Stage B — reorder kept chunks into a head+tail U-shape by relevance, to
    /// counter the lost-in-the-middle effect (Liu et al. 2307.03172). Lossless
    /// (reorders, drops nothing extra); replaces positional elision markers with a
    /// single summary note.
    pub retrieve_reorder: bool,
    /// Stage B — MMR diversity-aware selection: when picking the top-k chunks,
    /// penalize ones redundant with already-kept chunks (Carbonell & Goldstein 1998).
    pub retrieve_mmr: bool,
    /// Stage B — MMR tradeoff: 1.0 = pure relevance, 0.0 = pure diversity.
    pub retrieve_mmr_lambda: f64,
    /// Stage B — chunk at sentence granularity (DSLR, arXiv:2407.03627) for finer pruning.
    pub retrieve_sentence: bool,
    /// Stage A — provider prefix caching (cache_control breakpoints). Lossless; off
    /// by default (cache writes cost more, so it only pays off on repeated prefixes).
    pub cache: bool,
    /// Stage A — maximum cache breakpoints to place (Anthropic allows up to 4).
    pub cache_max_breakpoints: usize,
    /// Stage A — inject a `prompt_cache_key` to pin the provider's automatic prefix cache
    /// to a stable identity. On by default, but the caller must turn it OFF for backends
    /// that validate the request body strictly: an OpenAI-compatible endpoint that does
    /// not know the field rejects the whole request (`property 'body.prompt_cache_key' is
    /// unsupported`) rather than ignoring it, so this is only safe where the parameter is
    /// documented. No-op on wire shapes that have no such key (Anthropic, Google).
    pub cache_prompt_key: bool,
    /// Stage A — TTL for an *automatic* (top-level `cache_control`) breakpoint: `"1h"`, or
    /// empty for the API default of 5 minutes. Does not touch the explicit per-block
    /// breakpoints `cache_max_breakpoints` governs, which stay at the default lifetime.
    pub cache_auto_ttl: String,
    /// Stage E — collapse exact-duplicate lines in content (with `[×N]` counts).
    pub dedup: bool,
    /// Stage E — also collapse near-duplicate lines (SimHash).
    pub dedup_near: bool,
    /// Stage E — max SimHash Hamming distance treated as a near-duplicate.
    pub dedup_near_max_distance: u32,
    /// Stage E+ — reversible n-gram abbreviation dictionary (lossless input).
    pub ngram: bool,
    /// Stage E+ — maximum abbreviation-dictionary entries to introduce.
    pub ngram_max_entries: usize,
    /// Stage G — static tool selection: keep only tools relevant to the request.
    pub tool_select: bool,
    /// Stage G — truncate verbose tool descriptions.
    pub tool_trim_desc: bool,
    /// Stage G — minify each tool's JSON Schema in place (drop `$schema`/`title`/`examples`,
    /// collapse single-element type arrays, dedup repeated property descriptions, trim per-
    /// property descriptions). The API-safe subset of TSCG (arXiv:2605.26165): stays valid JSON
    /// Schema the provider accepts for native function-calling. Semantics-preserving, so on by
    /// default wherever `tool_trim_desc` is.
    pub tool_minify_schema: bool,
    /// Stage G — max characters for a tool description when trimming.
    pub tool_max_desc_chars: usize,
    /// Stage T — tool-output compression: window logs / diffs / grep output coming back
    /// from tools (the agent read path). Lossy; off by default.
    pub toolout: bool,
    /// Stage T — upper bound on lines kept per tool-output segment (adaptive-budget cap).
    pub toolout_max_lines: usize,
    /// Stage T — skip tool-output segments shorter than this many lines.
    pub toolout_min_lines: usize,
    /// Stage T — fold parametric log-line runs with a lossless Drain template pass first.
    pub toolout_template: bool,
    /// Stage T — adaptive/aggressive split: `"adaptive"` (always window to the budget),
    /// `"aggressive"` (always signal-only: errors / changed lines / one match per file +
    /// a summary), or `"auto"` (decide per segment by noise density — the tuned default).
    /// Dropped lines are elided by position (`[… N lines omitted …]`); the agent re-runs
    /// the tool if it needs them.
    pub toolout_mode: String,
    /// Stage T — also compress content that is *not* a tool result: the user's own
    /// messages. Off; `true` restores the pre-fix behavior for an A/B control arm. A
    /// paste is not tool output, so the recovery header's "re-run the tool" is a lie,
    /// and the Drain fold path reaches the model with no marker at all (dirge-09e8).
    pub toolout_user_text: bool,
    /// Stage T — also fold and window code and file excerpts. Off; `true` restores the
    /// pre-fix behavior. The line ranking here is a *log* heuristic (errors 1.0,
    /// indented 0.5, else 0.3); on source it keeps a near-arbitrary few percent — a
    /// 1202-line read reached the model as 45 lines — and destroys the line numbers
    /// `edit_lines` / `line_hash` anchor on (dirge-09e8).
    pub toolout_code: bool,
    /// Stage T — elision-header wording: `"explicit"` (states that llmtrim authored the
    /// gaps and the underlying content is complete) or `"legacy"` (the pre-fix text,
    /// kept for the A/B control arm).
    pub toolout_header: String,
    /// Stage T — honor the `[dirge: verbatim …]` first-line opt-out a tool can stamp on
    /// its result when the model explicitly asked for uncompressed output.
    pub toolout_verbatim: bool,
    /// Stage C — skeletonize fenced code blocks (drop function bodies to stubs).
    /// Lossy; off by default.
    pub skeletonize: bool,
    /// Stage C — relevance-graded skeletonization (HCP, arXiv:2406.18294): the N
    /// functions whose identifiers most overlap the conversation query keep their full
    /// bodies; the rest are skeletonized. Counted across the whole request; 0 disables
    /// the keep-full tier (pure uniform skeletonization). Default 5.
    pub skeleton_keep_full_top_k: usize,
    /// Stage C — drop the *signature too* (not just the body) for functions with zero
    /// query overlap whose body exceeds `skeleton_drop_min_body_lines`. More aggressive
    /// than skeletonization; OFF by default (conservative — preserves the signature tier).
    pub skeleton_drop_unmatched: bool,
    /// Stage C — minimum body line count before a zero-overlap function is eligible to be
    /// dropped entirely (only when `skeleton_drop_unmatched`). Guards small functions whose
    /// signature is most of their tokens. Default 8.
    pub skeleton_drop_min_body_lines: usize,
    /// Stage C — minify fenced brace-language code: strip indentation + blank lines,
    /// protecting string literals (arXiv:2508.13666). Semantically lossless; opt-in.
    pub minify_code: bool,
    /// Stage H — multimodal: lower image detail tier + downscale embedded images.
    /// Lossy; off by default.
    pub multimodal: bool,
    /// Stage H — optionally force the OpenAI image detail tier (e.g. `"low"`).
    /// `None` leaves the caller's choice; downscaling to the provider cap still runs.
    pub image_detail: Option<String>,
    /// Meta: when true, ignore the flags above and route to the shape-matched preset
    /// per request (`route`). Set by `auto()` / `preset("auto")`; the runtime default
    /// when no config file is present. `false` keeps the explicit flags (incl. defaults).
    pub auto: bool,
    /// Serve-layer turn-stability memo (see [`crate::llmtrim::memo`]). When on, the proxy reuses an
    /// already-seen conversation prefix's compressed bytes verbatim across turns, so the
    /// provider prefix cache (Anthropic `cache_control`, OpenAI implicit) stays warm on agent
    /// loops — where 85–95% of the prompt is unchanged turn-to-turn. **Read only by the
    /// `serve` interceptor**; the stateless `compress_with_config` core ignores it (it has no
    /// cross-request memory), so it is inert for the CLI. On by default: the memo only ever
    /// replays bytes it itself produced for a byte-identical earlier message and the suffix
    /// still passes the input-token gate, so it can't worsen a request — at worst it does
    /// nothing (cold prefix / n-gram carve-out). In-memory only (SECURITY.md).
    pub memo: bool,
    /// Quality gate: after the token gate accepts a lossy *content* stage (retrieve,
    /// toolout), re-check that query-relevant source content survived
    /// (Grusky coverage ≥ `quality_gate::COVERAGE_THRESHOLD`) and revert the stage if it
    /// didn't — catching cuts that "save tokens" by deleting the answer.
    ///
    /// **Default ON** and intentionally not toggled by any preset: it only ever *reverts*
    /// an over-aggressive compression (the request reverts to its pre-stage form, which
    /// the token gate already proved valid), never breaks or shapes output. So leaving it
    /// on can only protect the response — the safe default for the "quality-gated, not
    /// lossless" promise. Set `quality_gate = false` to run the token gate alone.
    pub quality_gate: bool,
}

impl Default for DenseConfig {
    /// The shipped default is `auto` (shape-routing), matching `load()` and the live
    /// interceptor. For the lossless-only baseline use [`DenseConfig::lossless`] (or the
    /// `safe` preset).
    fn default() -> Self {
        Self::auto()
    }
}

impl DenseConfig {
    /// The lossless-only baseline that every preset and `auto()` layer their flags over:
    /// only quality-neutral lossless input compression (`hygiene`, `serialize`, exact-duplicate
    /// `dedup`). This is the `safe` preset.
    pub fn lossless() -> Self {
        Self {
            hygiene: true,
            serialize: true,
            output_control: false,
            serialize_min_rows: 2,
            output_max_tokens: None,
            output_level: "terse".to_string(),
            output_token_budget: None,
            output_compact_code: false,
            output_frugal_tools: false,
            output_anti_overthink: false,
            serialize_nested: true,
            serialize_csv: false,
            serialize_flatten: false,
            serialize_buckets: false,
            json_crush: false,
            json_crush_max_rows: 50,
            strip_base64: false,
            numeric_sig_figs: None,
            normalize_unicode: false,
            retrieve: false,
            retrieve_keep_ratio: 0.5,
            retrieve_min_segment_chars: 600,
            retrieve_reorder: false,
            retrieve_mmr: false,
            retrieve_mmr_lambda: 0.5,
            retrieve_sentence: false,
            cache: false,
            cache_max_breakpoints: 4,
            cache_prompt_key: true,
            cache_auto_ttl: "1h".to_string(),
            dedup: true,
            dedup_near: false,
            dedup_near_max_distance: 3,
            ngram: false,
            ngram_max_entries: 32,
            tool_select: false,
            tool_trim_desc: false,
            tool_minify_schema: false,
            tool_max_desc_chars: 300,
            toolout: false,
            toolout_max_lines: 40,
            toolout_min_lines: 20,
            toolout_template: true,
            toolout_mode: "auto".to_string(),
            toolout_user_text: false,
            toolout_code: false,
            toolout_header: "explicit".to_string(),
            toolout_verbatim: true,
            skeletonize: false,
            skeleton_keep_full_top_k: 5,
            skeleton_drop_unmatched: false,
            skeleton_drop_min_body_lines: 8,
            minify_code: false,
            multimodal: false,
            image_detail: None,
            auto: false,
            memo: true,
            quality_gate: true,
        }
    }

    /// Load config. Resolution order:
    /// 1. `LLMTRIM_PRESET=<name>` env → that named profile.
    /// 2. A config file (`LLMTRIM_CONFIG` or the platform config dir) with a `preset = "<name>"`
    ///    key → that profile; or otherwise → the explicit per-stage flags in the file.
    /// 3. No env, no file → `auto` (shape-routing), the recommended default.
    ///
    /// Preset names: `auto` · `safe` · `rag` · `agent` · `code` · `aggressive` · `cache` ·
    /// `reasoning`. A `preset` key and raw flags are alternatives — `preset` wins (one knob
    /// instead of ~30); drop the `preset` key to hand-tune flags.
    pub fn load() -> Result<Self> {
        if let Some(name) = std::env::var("LLMTRIM_PRESET")
            .ok()
            .filter(|s| !s.is_empty())
        {
            return Self::preset(&name).with_context(|| {
                format!("unknown LLMTRIM_PRESET '{name}' (auto|safe|rag|agent|code|aggressive|cache|reasoning)")
            });
        }
        let Some(path) = config_path().filter(|p| p.exists()) else {
            return Ok(Self::auto());
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let value: toml::Value =
            toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        Self::from_toml_value(value).with_context(|| format!("invalid config {}", path.display()))
    }

    /// Resolve a parsed config: a `preset = "<name>"` key selects a named profile; otherwise
    /// the value is the explicit per-stage flags. Factored out so it is unit-testable.
    fn from_toml_value(value: toml::Value) -> Result<Self> {
        if let Some(name) = value.get("preset").and_then(toml::Value::as_str) {
            return Self::preset(name).with_context(|| format!("unknown preset '{name}'"));
        }
        // A file with no *compression* keys (empty, or only orthogonal keys like the
        // `RuntimeConfig` runtime settings) keeps the shipped `auto` shape-routing default.
        // Otherwise a file that sets only e.g. `capture_dir` would silently fall through to the
        // bare flag set (`auto = false`, everything-but-lossless off) and disable shape-routing —
        // a surprising downgrade for a key that has nothing to do with compression.
        if let Some(table) = value.as_table()
            && !table
                .keys()
                .any(|k| !RUNTIME_ONLY_KEYS.contains(&k.as_str()) && k != "preset")
        {
            return Ok(Self::auto());
        }
        value.try_into().context("config does not match the schema")
    }

    /// Config for the live interceptor: the same resolution as [`load`](Self::load), with no
    /// env/file falling back to `auto` (shape-routing). Safe for real clients — the breakers
    /// are in place: the `cache` stage skips client-managed `cache_control`, `retrieve`
    /// protects directive blocks, and `tool_select` never drops an already-invoked tool. A
    /// broken config is surfaced (not silently ignored) before falling back.
    pub fn load_for_interceptor() -> Self {
        Self::load().unwrap_or_else(|e| {
            eprintln!("llmtrim: {e}; using shape-routing defaults");
            Self::auto()
        })
    }

    /// The shape-routing config: at compress time, `route(request)` picks the preset
    /// (tools → agent, code → code, long-context+question → rag, else → aggressive).
    /// The recommended default — captures the per-shape wins without misfiring (RAG
    /// goes to `rag`, not blanket-aggressive). Still zero-model (structural detection).
    pub fn auto() -> Self {
        Self {
            auto: true,
            ..Self::lossless()
        }
    }

    /// A named bundle of stage flags layered over the lossless baseline, so callers opt into
    /// a workload profile without setting ~20 flags. `None` for an unknown name.
    pub fn preset(name: &str) -> Option<Self> {
        let mut c = Self::lossless();
        match name.to_ascii_lowercase().as_str() {
            // Defaults already = lossless input only (hygiene + serialize + exact dedup).
            "safe" | "lossless" => {}
            // Shape-routing meta-preset (resolved per request at compress time).
            "auto" => c.auto = true,
            // Frugality-only: no input compression, just the agent-loop directive. Isolates
            // the trajectory effect (info-per-call vs call-count) so a baseline-vs-frugal
            // agent bench measures the directive alone, not compression. Bench-only until it
            // proves out on tokens AND task success.
            "frugal" => c.output_frugal_tools = true,
            // RAG: training-free DSLR sentence pruning with a tight cap (0.35).
            // Bench-confirmed (hotpotqa n=20) to BEAT chunk-level on BOTH axes — cuts
            // more input (50% vs 43%) at less quality loss (−2.0pp vs −7.6pp) — because
            // it keeps the answer sentence inside an otherwise-irrelevant paragraph,
            // which chunk-level drops whole. `retrieve_sentence` reassembles in original
            // order, so no reorder.
            "rag" => {
                c.retrieve = true;
                c.retrieve_sentence = true;
                c.retrieve_keep_ratio = 0.35;
                // Long context can embed logs/diffs/grep dumps or huge JSON tables —
                // compress those too (shape-gated; prose is left to retrieve above).
                c.toolout = true;
                c.json_crush = true;
                // Output control on by default: terse output holds quality (often improves
                // it) and output tokens cost 3–5× input — the metric is cost-at-no-quality-
                // loss, not losslessness. `safe` is the lossless-only preset.
                c.output_control = true;
                // Image downscale to the provider's resolution cap — quality-neutral by
                // construction (the provider resizes to the same cap regardless), so the
                // model sees identical pixels for fewer upload bytes + image tokens.
                c.multimodal = true;
                // Elide base64 / data-URI blobs (≥200-char runs → `[base64 elided: N]`
                // marker) — measured quality-neutral (+0.0pp on bench/data/base64.jsonl):
                // such blobs are noise the model can't use. Lossy, so `safe` keeps them.
                c.strip_base64 = true;
                // Anti-overthinking directive: fires only when the request explicitly declares
                // BOTH a quantized serving tier and a reasoning pass (see `stages::output`), so
                // it is a no-op on the vast majority of RAG traffic and only ever adds an
                // instruction, never removes one.
                c.output_anti_overthink = true;
            }
            "agent" => {
                // Tool selection is first-turn-only (see `stages::tools::select_tools`): pruning
                // the `tools[]` block on later turns would churn the cached prompt prefix and
                // raise cost on an agent loop (issue #9). It still prunes the opening single-shot
                // request, where the saving is free. Trim + minify below are deterministic, so
                // they shrink the block without changing it turn-to-turn.
                c.tool_select = true;
                c.tool_trim_desc = true;
                // Minify tool schemas in place (API-safe TSCG subset): semantics-preserving, so
                // it rides with description trimming — pure win on the tool block agents resend.
                c.tool_minify_schema = true;
                c.cache = true;
                c.toolout = true; // window log/diff/grep tool results (the agent read path)
                c.serialize_flatten = true; // dot-flatten nested tool-result JSON
                c.serialize_buckets = true; // bucket heterogeneous record arrays
                c.json_crush = true; // sample huge record arrays to representatives
                // Terse output, FIRST TURN ONLY (gated in `stages::output`): later tool-call
                // replies leave nothing to trim, so terse across the whole loop gave ~no cost
                // benefit (glaive cost 5%) at neutral quality (n=39 +0.0pp, CI ±5.2 — the n=12
                // -8pp was noise; see bench/README). The opening turn is the one that emits real
                // prose (planning, explanation), so shape only that. Pending a first-turn-scoped
                // bench to confirm it beats the whole-loop 0.0%.
                c.output_control = true;
                c.multimodal = true; // downscale images to the provider cap (see `rag` note)
                c.strip_base64 = true; // elide base64 blobs (measured +0.0pp, see `rag` note)
                // Agent-loop frugality directive: fires only on the FIRST tool-call turn of a
                // task (first-turn-only + idempotent), so it costs ~one ~50-tok injection with no
                // per-turn cache churn. It's tail insurance, not a median saver — the real-CC A/B
                // is a median wash but caps the exploration blow-ups (search-heavy loops thrashing
                // into many narrow probes), with no task-success regression. Model-gated: capable
                // harnesses batch as asked, weaker ones ignore it harmlessly.
                c.output_frugal_tools = true;
                // ngram dropped: ~10–106 tok on agent traffic (bench) for an injected
                // glossary that mutates the prompt — not worth it. Opt in explicitly.
                // Anti-overthinking: only fires on the final non-tool-call turn of a loop
                // (the branch `frugal_tools` doesn't touch), same quantized+reasoning gate
                // as `rag`/`code`/`aggressive` below.
                c.output_anti_overthink = true;
            }
            "code" => {
                c.skeletonize = true;
                c.minify_code = true;
                // A coding turn often pastes a build log / diff / grep dump or a big JSON
                // config; these no-op on actual code (shape-gated), fire only when present.
                c.toolout = true;
                c.json_crush = true;
                c.output_control = true;
                c.multimodal = true; // downscale images to the provider cap (see `rag` note)
                c.strip_base64 = true; // elide base64 blobs (measured +0.0pp, see `rag` note)
                // `output_compact_code` (minified-output instruction) is NOT bundled:
                // the bench confirmed it costs pass@1 (humaneval −21.6pp, CI ±14.5 at
                // n=37). The −36% lever (arXiv:2508.13666) holds only via fine-tuning,
                // not a raw instruction to a small model. Opt in explicitly if wanted.
                c.output_anti_overthink = true; // quantized+reasoning gated, see `rag` note
            }
            "aggressive" => {
                c.retrieve = true;
                // DSLR sentence pruning with a TIGHT cap (0.35): prunes harder than
                // chunk-level but protects the answer sentence + boundaries, so it cuts
                // more input at less quality cost than dropping whole paragraphs.
                c.retrieve_sentence = true;
                c.retrieve_keep_ratio = 0.35;
                c.skeletonize = true;
                // Most aggressive skeleton tier: drop zero-overlap large bodies signature
                // and all (HCP — cross-file non-dependency code is mostly noise). Bundled
                // here only; the keep-full top-k upgrade is on for every skeletonizing preset.
                c.skeleton_drop_unmatched = true;
                c.minify_code = true;
                c.dedup_near = true;
                c.ngram = true;
                c.normalize_unicode = true;
                // First-turn-only, like `agent` — `select_tools` never prunes mid-loop, so the
                // cached prefix stays stable even under this preset's heavier compression (#9).
                c.tool_select = true;
                c.tool_trim_desc = true;
                c.tool_minify_schema = true; // API-safe TSCG schema minify (rides with trim)
                c.cache = true;
                c.toolout = true; // compress log/diff/grep tool results (auto split)
                c.serialize_flatten = true;
                c.serialize_buckets = true;
                c.json_crush = true;
                c.output_control = true;
                c.multimodal = true; // downscale images to the provider cap (see `rag` note)
                c.strip_base64 = true; // elide base64 blobs (measured +0.0pp, see `rag` note)
                c.output_anti_overthink = true; // quantized+reasoning gated, see `rag` note
            }
            // Cache-first: lossless input only (no retrieve/reorder that *varies* the
            // prefix per request) + Stage A cache discipline, so a repeated long prefix
            // (agent / RAG-over-fixed-context) is served from the prompt cache. The bench
            // `cache` corpus shows ~92% input served from cache, the biggest cost lever
            // for fixed-context workloads — bigger than squeezing tokens.
            "cache" => {
                c.cache = true;
            }
            // Reasoning: Chain-of-Draft output (terse ≤5-word steps). Focuses the model
            // and cuts output tokens; measured +17pp accuracy on GSM8K vs verbose CoT
            // (compression *helping* quality, not just preserving it).
            "reasoning" => {
                c.output_control = true;
                c.output_level = "draft".to_string();
            }
            _ => return None,
        }
        Some(c)
    }
}

fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LLMTRIM_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("llmtrim").join("config.toml"))
}

/// Config-file keys that belong to [`RuntimeConfig`], not the compression pipeline. Listed so
/// [`DenseConfig::from_toml_value`] treats a file that sets only these as "no compression keys"
/// and keeps the `auto` default, instead of silently downgrading shape-routing.
pub(crate) const RUNTIME_ONLY_KEYS: &[&str] = &[
    "extra_hosts",
    "exclude_providers",
    "exclude_hosts",
    "upstream_proxy",
    "capture_dir",
    "db_path",
    "no_update_check",
    "bind",
    "capture_max_mb",
    "breakdown_window",
    "retention_days",
    "max_rows",
    "max_breakdown_turns",
    "theme",
    "sub",
    "compact",
];

/// The resolved config-file path (`LLMTRIM_CONFIG`, else `$XDG_CONFIG_HOME`/`$HOME/.config` +
/// `llmtrim/config.toml`), or `None` if HOME/XDG are unset. Public so the CLI can show it and the
/// reroute mapping editor can persist to it.
pub fn config_file_path() -> Option<PathBuf> {
    config_path()
}

/// Persist the subscription reroute selection: set the top-level `sub` key to `provider` and write
/// the tier→model table under `[sub.<provider>.tiers]`. Other config keys/values are preserved
/// (the file is parsed as TOML, mutated, and re-serialized — comments and key order are not
/// preserved, matching a structured rewrite rather than [`save_theme`]'s surgical line edit).
/// Creates the file and its parent directory if absent.
pub fn write_sub_mapping(
    provider: &str,
    tiers: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no config path (HOME/XDG unset)"))?;
    write_sub_mapping_at(&path, provider, tiers)
}

/// Disable subscription reroute: set `sub.active = "off"` (the reader filters `off` to unset),
/// preserving the saved `[sub.<provider>.tiers]` mapping and `mode` so re-enabling with
/// `sub on` restores them. The provider being turned off is remembered in `sub.last` so a bare
/// `sub on` can bring it back. Goes through `edit_sub_table_at` like the other editors.
pub fn disable_sub() -> Result<()> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no config path (HOME/XDG unset)"))?;
    disable_sub_at(&path)
}

fn disable_sub_at(path: &std::path::Path) -> Result<()> {
    edit_sub_table_at(path, |t| {
        if let Some(active) = t.get("active").and_then(toml::Value::as_str)
            && active != "off"
            && !active.is_empty()
        {
            let last = active.to_string();
            t.insert("last".to_string(), toml::Value::String(last));
        }
        t.insert("active".to_string(), toml::Value::String("off".to_string()));
    })
}

/// Re-enable reroute to `provider` without rewriting its saved `[sub.<provider>.tiers]` preset
/// (unlike [`write_sub_mapping`], which resets it to defaults). Used by `sub on` with no explicit
/// provider, so a customized mapping survives an off/on cycle.
pub fn enable_sub(provider: &str) -> Result<()> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no config path (HOME/XDG unset)"))?;
    enable_sub_at(&path, provider)
}

fn enable_sub_at(path: &std::path::Path, provider: &str) -> Result<()> {
    let provider = provider.to_string();
    edit_sub_table_at(path, |t| {
        t.insert("active".to_string(), toml::Value::String(provider));
    })
}

/// The provider a bare `sub on` should restore: the currently-active provider if reroute is still
/// on, else the `sub.last` provider saved by [`disable_sub`]. `None` if reroute was never enabled.
pub fn sub_reenable_provider() -> Option<String> {
    sub_reenable_provider_at(&config_path()?)
}

fn sub_reenable_provider_at(path: &std::path::Path) -> Option<String> {
    let doc: toml::Value = toml::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let sub = doc.get("sub")?;
    // Bare-string form: `sub = "codex"`.
    if let Some(s) = sub.as_str() {
        return (s != "off" && !s.is_empty()).then(|| s.trim().to_ascii_lowercase());
    }
    let clean = |s: &str| {
        let s = s.trim();
        (s != "off" && !s.is_empty()).then(|| s.to_ascii_lowercase())
    };
    sub.get("active")
        .and_then(toml::Value::as_str)
        .and_then(clean)
        .or_else(|| {
            sub.get("last")
                .and_then(toml::Value::as_str)
                .and_then(clean)
        })
}

fn write_sub_mapping_at(
    path: &std::path::Path,
    provider: &str,
    tiers: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    // `sub` is stored as a table with `active` + per-provider `tiers` (the reader also accepts a
    // bare `sub = "codex"` string). Editing through `edit_sub_table_at` preserves any existing
    // `mode` and other providers' entries when the selection or preset is (re)written.
    let mut tiers_tbl = toml::Table::new();
    for (k, v) in tiers {
        tiers_tbl.insert(k.clone(), toml::Value::String(v.clone()));
    }
    let provider = provider.to_string();
    edit_sub_table_at(path, |t| {
        t.insert("active".to_string(), toml::Value::String(provider.clone()));
        let mut prov_tbl = toml::Table::new();
        prov_tbl.insert("tiers".to_string(), toml::Value::Table(tiers_tbl));
        t.insert(provider, toml::Value::Table(prov_tbl));
    })
}

/// Load the config doc, coerce `sub` into a table (migrating a bare `sub = "codex"` string to
/// `{ active = "codex" }`, and `"off"` to an empty table), hand that table to `edit`, then write
/// the whole doc back. Shared by the granular sub editors so each preserves the rest of the sub
/// table (active provider, mode, other map entries) and the rest of the config file.
fn edit_sub_table_at(path: &std::path::Path, edit: impl FnOnce(&mut toml::Table)) -> Result<()> {
    use anyhow::Context;
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml::Table = if existing.trim().is_empty() {
        toml::Table::new()
    } else {
        existing
            .parse()
            .context("existing config is not valid TOML")?
    };
    let mut sub_tbl = match doc.remove("sub") {
        Some(toml::Value::Table(t)) => t,
        Some(toml::Value::String(s)) if s != "off" && !s.is_empty() => {
            let mut t = toml::Table::new();
            t.insert("active".to_string(), toml::Value::String(s));
            t
        }
        _ => toml::Table::new(),
    };
    edit(&mut sub_tbl);
    doc.insert("sub".to_string(), toml::Value::Table(sub_tbl));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let serialized = toml::to_string_pretty(&doc).context("serializing config")?;
    std::fs::write(path, serialized).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Set the reroute mode: `fallback` = reroute only when Anthropic fails, `always` = reroute every
/// matching turn. No legacy spelling is accepted so the persisted configuration has one clear
/// vocabulary.
/// Preserves the active provider and every `[sub.<provider>.tiers]` entry.
pub fn write_sub_mode(fallback: bool) -> Result<()> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no config path (HOME/XDG unset)"))?;
    let mode = if fallback { "fallback" } else { "always" };
    edit_sub_table_at(&path, |t| {
        t.insert("mode".to_string(), toml::Value::String(mode.to_string()));
    })
}

/// Persist the ordered provider chain used by `sub mode fallback`.
pub fn write_sub_chain(providers: &[String]) -> Result<()> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no config path (HOME/XDG unset)"))?;
    let values = providers
        .iter()
        .map(|p| toml::Value::String(p.trim().to_ascii_lowercase()))
        .collect();
    edit_sub_table_at(&path, |t| {
        t.insert("chain".to_string(), toml::Value::Array(values));
    })
}

/// Set the Codex reasoning effort under `[sub.<provider>].effort` (or remove it for `none`),
/// preserving the mapping/mode. `level` is `none`/`low`/`medium`/`high`/`xhigh`.
pub fn write_sub_effort(provider: &str, level: &str) -> Result<()> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no config path (HOME/XDG unset)"))?;
    let (provider, level) = (provider.to_string(), level.to_ascii_lowercase());
    edit_sub_table_at(&path, |t| {
        let prov = t
            .entry(provider)
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(prov) = prov.as_table_mut() else {
            return;
        };
        if level == "none" || level.is_empty() {
            prov.remove("effort");
        } else {
            prov.insert("effort".to_string(), toml::Value::String(level));
        }
    })
}

/// Set a single `from → to` mapping entry under `[sub.<provider>.tiers]`, preserving the other
/// entries, the active provider, and the mode. `from` is a Claude tier name (`opus`/…) or an exact
/// incoming model id; both are lowercased to match the reader.
pub fn write_sub_map_entry(provider: &str, from: &str, to: &str) -> Result<()> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no config path (HOME/XDG unset)"))?;
    let (provider, from, to) = (
        provider.to_string(),
        from.to_ascii_lowercase(),
        to.to_string(),
    );
    edit_sub_table_at(&path, |t| {
        let prov = t
            .entry(provider)
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(prov) = prov.as_table_mut() else {
            return;
        };
        let tiers = prov
            .entry("tiers".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(tiers) = tiers.as_table_mut() {
            tiers.insert(from, toml::Value::String(to));
        }
    })
}

/// Remove a single `from` mapping entry under `[sub.<provider>.tiers]` (no-op if absent).
pub fn remove_sub_map_entry(provider: &str, from: &str) -> Result<()> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no config path (HOME/XDG unset)"))?;
    let (provider, from) = (provider.to_string(), from.to_ascii_lowercase());
    edit_sub_table_at(&path, |t| {
        if let Some(tiers) = t
            .get_mut(&provider)
            .and_then(toml::Value::as_table_mut)
            .and_then(|p| p.get_mut("tiers"))
            .and_then(toml::Value::as_table_mut)
        {
            tiers.remove(&from);
        }
    })
}

/// Runtime settings orthogonal to the compression pipeline ([`DenseConfig`]). Each value
/// resolves **env-first, then a top-level key in the same config TOML, then a default** — the
/// generalized form of the original `retention_days` rule, so every runtime knob is settable
/// either way ("always handle both"). These keys are ignored by `DenseConfig` (it has no
/// `deny_unknown_fields`), so they coexist in the one config file. Parsed once via
/// [`RuntimeConfig::get`].
///
/// Intentionally *not* covered (env-only): `LLMTRIM_CONFIG` (points at this file — chicken/egg),
/// `LLMTRIM_HOME` (base dir resolved before config loads), `LLMTRIM_PROFILE` (dev timing
/// toggle), `LLMTRIM_VERSION` (internal updater handoff). None are persistable user settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
// Constructed only inside this crate (`resolve`/`load`); every other crate just reads fields
// off `RuntimeConfig::get()`. Marking it non-exhaustive keeps adding a new setting a
// non-breaking change instead of tripping cargo-semver-checks' `constructible_struct_adds_field`.
#[non_exhaustive]
pub struct RuntimeConfig {
    /// Extra exact LLM-API hosts to intercept beyond the built-in registry. Env
    /// `LLMTRIM_EXTRA_HOSTS` (comma-separated) replaces the file `extra_hosts` array.
    /// Normalized to lowercase plain hostnames; malformed/overbroad entries are dropped.
    /// Each entry widens the name-constrained MITM CA, so keep them exact (`llm.acme.com`,
    /// never a bare apex like `acme.com`).
    pub extra_hosts: Vec<String>,
    /// Upstream HTTP proxy URL (env `LLMTRIM_UPSTREAM_PROXY` / file `upstream_proxy`).
    pub upstream_proxy: Option<String>,
    /// QA capture corpus directory (env `LLMTRIM_CAPTURE_DIR` / file `capture_dir`).
    pub capture_dir: Option<PathBuf>,
    /// Ledger DB path (env `LLMTRIM_DB_PATH` / file `db_path`).
    pub db_path: Option<PathBuf>,
    /// Disable the passive update check. The env var is **presence-only**: setting
    /// `LLMTRIM_NO_UPDATE_CHECK` to *any* value (including `0` or empty) disables the check;
    /// leaving it unset is the only way to keep the check on. The config key `no_update_check`
    /// is a normal bool. (Preserves the prior `var_os(...).is_some()` behavior.)
    pub no_update_check: bool,
    /// Listen bind address, left unparsed (env `LLMTRIM_BIND` / file `bind`); the caller
    /// parses + validates it as an IP.
    pub bind: Option<String>,
    /// Capture corpus size ceiling in MB (env `LLMTRIM_CAPTURE_MAX_MB` / file
    /// `capture_max_mb`); `Some(0)` disables the cap, `None` means use the default.
    pub capture_max_mb: Option<u64>,
    /// Context-window override for the cost breakdown (env `LLMTRIM_BREAKDOWN_WINDOW` / file
    /// `breakdown_window`); positive only.
    pub breakdown_window: Option<i64>,
    /// Ledger age-retention in days (env `LLMTRIM_RETENTION_DAYS` / file `retention_days`);
    /// positive only (`None` = age retention off, row cap alone bounds the ledger).
    pub retention_days: Option<i64>,
    /// Ledger row cap — most-recent N compression events kept (env `LLMTRIM_MAX_ROWS` / file
    /// `max_rows`); positive only (`None` = use the built-in default).
    pub max_rows: Option<i64>,
    /// Retention cap for the per-source breakdown, in *turns* (env `LLMTRIM_MAX_BREAKDOWN_TURNS`
    /// / file `max_breakdown_turns`); positive only (`None` = use the built-in default). A turn
    /// fans out into many block rows, so this is the knob that governs breakdown-history depth
    /// and on-disk size.
    pub max_breakdown_turns: Option<i64>,
    /// Breakdown-TUI color theme (env `LLMTRIM_THEME` / file `theme`); a Catppuccin flavor
    /// name (`mocha`/`macchiato`/`frappe`/`latte`). The `t` key persists the user's choice
    /// here via [`save_theme`]. The TUI validates the name and falls back to its default.
    pub theme: Option<String>,
    /// Subscription reroute target (env `LLMTRIM_SUB` / file `sub`): `codex`, `kimi`, or `grok` reroutes
    /// intercepted Anthropic `/v1/messages` traffic to that subscription's backend instead of
    /// Anthropic (translating the request/response wire shapes). `off`/unset keeps the
    /// transparent compress-and-forward behavior. Lowercased; an unknown value is left as-is
    /// for the serve layer to reject with a clear error.
    pub sub: Option<String>,
    /// Tier→model overrides for the active `sub` provider, read from the config file table
    /// `[sub.<provider>.tiers]` (e.g. `[sub.codex.tiers]` with `opus = "gpt-5.5"`). Keys are the
    /// Claude tier names (`opus`/`sonnet`/`haiku`/`fable`) or exact incoming model ids; values are
    /// the upstream provider model id to route that tier to. Empty when unset — the serve layer
    /// fills any missing tier from the built-in default preset. File-only (the reroute mapping
    /// editor writes this table); there is no env form.
    pub sub_tiers: std::collections::BTreeMap<String, String>,
    /// Optional proxy-side override of the Codex reasoning effort (`low`/`medium`/`high`/`xhigh`,
    /// or `none`/unset). When unset, the reroute honors the client's own per-turn effort (Claude
    /// Code sends `output_config.effort`); when set, this forces one effort on every rerouted
    /// request. Env `LLMTRIM_CODEX_EFFORT` or file `sub.codex.effort`. Ignored by Kimi (single
    /// model, no reasoning knob).
    pub sub_effort: Option<String>,
    /// Reroute only when Anthropic fails. When `true`, the proxy forwards to Anthropic as usual
    /// and tries the ordered [`sub_chain`](Self::sub_chain) providers on a quota, overload, transport, or
    /// retryable upstream failure; when `false` (default), a set `sub` reroutes every matching
    /// turn. Env `LLMTRIM_SUB_MODE` (`always`/`fallback`) or the file key `sub.mode`.
    pub sub_fallback: bool,
    /// Ordered subscription providers to try in fallback mode. Env `LLMTRIM_SUB_CHAIN` or
    /// `[sub] chain = ["codex", "kimi"]`; when omitted, the active `sub` provider is used.
    pub sub_chain: Vec<String>,
    /// Whether to enable server-side continuation for Codex reroutes (`previous_response_id` +
    /// delta-only `input` on follow-up turns). This keeps the upstream conversation state warm and
    /// improves real prompt-cache / token reuse (the mechanism behind good `♻ % cached` numbers).
    /// Env `LLMTRIM_CODEX_PREVIOUS_RESPONSE_ID` (1/true/yes) or `[sub.codex] previous_response_id = true`.
    /// Defaults to `true`.
    pub sub_codex_previous_response_id: bool,
    /// Ordered alternative models for Claude Code compaction requests, read from
    /// `[compact] models = [...]`. The originally requested model is always appended by the
    /// interceptor as an implicit final fallback, so it is never stored here. Empty means compact
    /// model substitution is disabled. File-only: model routing is persistent policy, not an
    /// environment toggle.
    pub compact_models: Vec<String>,
}

impl RuntimeConfig {
    /// Process-wide instance, loaded once from env + the config file on first use. Env vars
    /// don't change mid-process, so caching is safe and keeps per-request paths (the capture
    /// cap) off the filesystem. Because it is cached for the process lifetime, **tests must use
    /// `resolve`, never `get`** — the first caller fixes the value for the
    /// whole test binary, so `get` can't observe a per-test environment.
    pub fn get() -> &'static RuntimeConfig {
        static CACHE: std::sync::OnceLock<RuntimeConfig> = std::sync::OnceLock::new();
        CACHE.get_or_init(Self::load)
    }

    /// Load from the real environment and config file (the one parse of the TOML).
    fn load() -> RuntimeConfig {
        Self::resolve(|k| std::env::var(k).ok(), cached_config_file())
    }

    /// Pure resolver: `env` looks up an environment variable, `file` is the parsed config TOML
    /// (if any). Factored out so the env-over-file precedence is unit-testable without touching
    /// the real environment or filesystem.
    fn resolve(env: impl Fn(&str) -> Option<String>, file: Option<&toml::Value>) -> RuntimeConfig {
        let env_set = |k: &str| env(k).filter(|s| !s.is_empty());
        let fstr = |key: &str| {
            file.and_then(|v| v.get(key))
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        };
        let fint = |key: &str| {
            file.and_then(|v| v.get(key))
                .and_then(toml::Value::as_integer)
        };
        let fbool = |key: &str| file.and_then(|v| v.get(key)).and_then(toml::Value::as_bool);
        let positive = |v: Option<i64>| v.filter(|n| *n > 0);

        let extra_hosts = resolve_str_list(
            env_set("LLMTRIM_EXTRA_HOSTS"),
            file,
            "extra_hosts",
            normalize_host,
        );

        RuntimeConfig {
            extra_hosts,
            upstream_proxy: env_set("LLMTRIM_UPSTREAM_PROXY").or_else(|| fstr("upstream_proxy")),
            capture_dir: env_set("LLMTRIM_CAPTURE_DIR")
                .or_else(|| fstr("capture_dir"))
                .map(PathBuf::from),
            db_path: env_set("LLMTRIM_DB_PATH")
                .or_else(|| fstr("db_path"))
                .map(PathBuf::from),
            no_update_check: env("LLMTRIM_NO_UPDATE_CHECK").is_some()
                || fbool("no_update_check").unwrap_or(false),
            bind: env_set("LLMTRIM_BIND").or_else(|| fstr("bind")),
            capture_max_mb: env_set("LLMTRIM_CAPTURE_MAX_MB")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .or_else(|| fint("capture_max_mb").and_then(|n| u64::try_from(n).ok())),
            breakdown_window: positive(
                env_set("LLMTRIM_BREAKDOWN_WINDOW")
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .or_else(|| fint("breakdown_window")),
            ),
            retention_days: positive(
                env_set("LLMTRIM_RETENTION_DAYS")
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .or_else(|| fint("retention_days")),
            ),
            max_rows: positive(
                env_set("LLMTRIM_MAX_ROWS")
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .or_else(|| fint("max_rows")),
            ),
            max_breakdown_turns: positive(
                env_set("LLMTRIM_MAX_BREAKDOWN_TURNS")
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .or_else(|| fint("max_breakdown_turns")),
            ),
            theme: env_set("LLMTRIM_THEME").or_else(|| fstr("theme")),
            sub: {
                let active = resolve_sub_provider(&env, file);
                active.filter(|s| s != "off" && !s.is_empty())
            },
            sub_tiers: resolve_sub_tiers(&env, file),
            sub_fallback: resolve_sub_fallback(&env, file),
            sub_chain: resolve_sub_chain(&env, file),
            sub_effort: resolve_sub_effort(&env, file),
            sub_codex_previous_response_id: resolve_sub_codex_continuation(&env, file),
            compact_models: resolve_compact_models(file),
        }
    }
}

/// The active reroute provider from env (`LLMTRIM_SUB`) or the config `sub` key. The file form is
/// either a bare string (`sub = "codex"`) or a table with an `active`/`provider` key
/// (`[sub]` \n `active = "codex"`), the latter being what [`write_sub_mapping`] emits so it can
/// also carry `[sub.<provider>.tiers]`. Lowercased.
fn resolve_sub_provider(
    env: &impl Fn(&str) -> Option<String>,
    file: Option<&toml::Value>,
) -> Option<String> {
    if let Some(v) = env("LLMTRIM_SUB").filter(|s| !s.is_empty()) {
        return Some(v.trim().to_ascii_lowercase());
    }
    let sub = file?.get("sub")?;
    let s = sub.as_str().or_else(|| {
        sub.get("active")
            .or_else(|| sub.get("provider"))
            .and_then(toml::Value::as_str)
    })?;
    Some(s.trim().to_ascii_lowercase())
}

/// The `[sub.<provider>.tiers]` overrides for the active provider (empty if none). Keys lowercased.
fn resolve_sub_tiers(
    env: &impl Fn(&str) -> Option<String>,
    file: Option<&toml::Value>,
) -> std::collections::BTreeMap<String, String> {
    let Some(provider) = resolve_sub_provider(env, file) else {
        return std::collections::BTreeMap::new();
    };
    sub_tiers_from_file(file, &provider)
}

/// Read `[sub.<provider>.tiers]` for a *specific* provider, independent of who is currently
/// active. Used when a window `/sub on grok` overrides a global `sub = codex`: the request is
/// routed to Grok and must map tiers from `[sub.grok.tiers]` (or Grok defaults), never from
/// Codex's mapping (which would send `gpt-5.6-terra` to the Grok backend).
pub fn sub_tiers_for(provider: &str) -> std::collections::BTreeMap<String, String> {
    let provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty() || provider == "off" {
        return std::collections::BTreeMap::new();
    }
    let file = config_path()
        .filter(|p| p.exists())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok());
    sub_tiers_from_file(file.as_ref(), &provider)
}

fn sub_tiers_from_file(
    file: Option<&toml::Value>,
    provider: &str,
) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    let Some(file) = file else {
        return map;
    };
    if let Some(tiers) = file
        .get("sub")
        .and_then(|v| v.get(provider))
        .and_then(|v| v.get("tiers"))
        .and_then(toml::Value::as_table)
    {
        for (k, v) in tiers {
            if let Some(model) = v.as_str() {
                map.insert(k.to_ascii_lowercase(), model.to_string());
            }
        }
    }
    map
}

/// Codex reasoning effort for the active provider: env `LLMTRIM_CODEX_EFFORT` wins, else the file
/// key `sub.<provider>.effort`. Lowercased; `none`/empty resolves to `None` (no reasoning).
fn resolve_sub_effort(
    env: &impl Fn(&str) -> Option<String>,
    file: Option<&toml::Value>,
) -> Option<String> {
    let clean = |s: String| {
        let s = s.trim().to_ascii_lowercase();
        (!s.is_empty() && s != "none").then_some(s)
    };
    if let Some(v) = env("LLMTRIM_CODEX_EFFORT").filter(|s| !s.is_empty()) {
        return clean(v);
    }
    let provider = resolve_sub_provider(env, file)?;
    file?
        .get("sub")
        .and_then(|v| v.get(&provider))
        .and_then(|v| v.get("effort"))
        .and_then(toml::Value::as_str)
        .map(str::to_string)
        .and_then(clean)
}

/// Whether reroute runs in `fallback` mode: env `LLMTRIM_SUB_MODE` (`fallback` → true,
/// `always` → false) wins, else the `sub.mode` table key. Default `false` (reroute always).
///
/// The pre-0.10 spellings (`on_error`/`on-error`/`onerror`) still resolve to `fallback` — they
/// mean the same thing, and silently reading them as `always` would flip an existing config into
/// rerouting *every* turn to the subscription provider. An unrecognized value is reported and
/// treated as `always` (the default), never as fallback.
fn parse_sub_mode(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "always" => Some(false),
        "fallback" => Some(true),
        "on_error" | "on-error" | "onerror" => {
            eprintln!(
                "llmtrim: sub mode '{}' is the old name for 'fallback' — run `llmtrim sub mode fallback` to update the config",
                raw.trim()
            );
            Some(true)
        }
        _ => None,
    }
}

fn resolve_sub_fallback(env: &impl Fn(&str) -> Option<String>, file: Option<&toml::Value>) -> bool {
    let resolve = |raw: &str, source: &str| {
        parse_sub_mode(raw).unwrap_or_else(|| {
            eprintln!(
                "llmtrim: unknown sub mode '{}' in {source} — using 'always' (expected always|fallback)",
                raw.trim()
            );
            false
        })
    };
    if let Some(v) = env("LLMTRIM_SUB_MODE").filter(|s| !s.is_empty()) {
        return resolve(&v, "LLMTRIM_SUB_MODE");
    }
    file.and_then(|v| v.get("sub"))
        .and_then(|v| v.get("mode"))
        .and_then(toml::Value::as_str)
        .map(|v| resolve(v, "[sub] mode"))
        .unwrap_or(false)
}

/// Ordered fallback providers from env or `[sub] chain`. Unknown entries are retained so the
/// serve layer can report them clearly; an empty/missing chain falls back to the active provider.
fn resolve_sub_chain(
    env: &impl Fn(&str) -> Option<String>,
    file: Option<&toml::Value>,
) -> Vec<String> {
    let parse = |raw: &str| {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect::<Vec<_>>()
    };
    if let Some(v) = env("LLMTRIM_SUB_CHAIN").filter(|s| !s.is_empty()) {
        return parse(&v);
    }
    let sub = file.and_then(|v| v.get("sub"));
    let mut chain = sub
        .and_then(|v| v.get("chain"))
        .map(|v| match v {
            toml::Value::Array(values) => values
                .iter()
                .filter_map(toml::Value::as_str)
                .flat_map(parse)
                .collect(),
            toml::Value::String(s) => parse(s),
            _ => Vec::new(),
        })
        .unwrap_or_default();
    if chain.is_empty()
        && let Some(active) = resolve_sub_provider(env, file)
        && active != "off"
    {
        chain.push(active);
    }
    chain
}

/// Codex continuation (previous_response_id deltas) enabled? Env `LLMTRIM_CODEX_PREVIOUS_RESPONSE_ID`
/// (truthy) wins, else `[sub.codex] previous_response_id = true` (or `continuation`).
///
/// Defaults to `false`: the ChatGPT backend accepts `previous_response_id` only over its
/// WebSocket transport, and rejects it on the HTTP `/responses` path we use with
/// `400 Unsupported parameter`. It is also incoherent with the `store: false` we send — there is
/// no server-side response to continue from. Left as an opt-in for a future WebSocket transport;
/// prefix caching via `prompt_cache_key` is what carries the cache today.
fn resolve_sub_codex_continuation(
    env: &impl Fn(&str) -> Option<String>,
    file: Option<&toml::Value>,
) -> bool {
    let is_true = |s: &str| {
        matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    };
    if let Some(v) = env("LLMTRIM_CODEX_PREVIOUS_RESPONSE_ID").filter(|s| !s.is_empty()) {
        return is_true(&v);
    }
    if let Some(sub) = file.and_then(|v| v.get("sub"))
        && let Some(c) = sub.get("codex").or_else(|| sub.get("chatgpt"))
    {
        if let Some(b) = c
            .get("previous_response_id")
            .or_else(|| c.get("continuation"))
            .and_then(toml::Value::as_bool)
        {
            return b;
        }
        if let Some(s) = c
            .get("previous_response_id")
            .or_else(|| c.get("continuation"))
            .and_then(toml::Value::as_str)
        {
            return is_true(s);
        }
    }
    false
}

/// Ordered compact-model alternatives from `[compact] models`. Preserve user order and retain
/// only the first occurrence of each non-empty entry. Invalid shapes disable substitution rather
/// than affecting ordinary compression or proxying.
fn resolve_compact_models(file: Option<&toml::Value>) -> Vec<String> {
    let Some(values) = file
        .and_then(|v| v.get("compact"))
        .and_then(|v| v.get("models"))
        .and_then(toml::Value::as_array)
    else {
        return Vec::new();
    };
    let mut models = Vec::new();
    for value in values {
        let Some(model) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        if !models.iter().any(|existing| existing == model) {
            models.push(model.to_string());
        }
    }
    models
}

/// Replace `[compact].models` while preserving every unrelated TOML value. The original client
/// model is deliberately absent: it is an invariant appended per request by the interceptor.
pub fn write_compact_models(models: &[String]) -> Result<()> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no config path (HOME/XDG unset)"))?;
    write_compact_models_at(&path, models)
}

/// Whether the config explicitly contains `[compact].models`, including an empty list used to
/// remember that setup's recommendation was declined.
pub fn compact_models_configured() -> bool {
    let Some(path) = config_path() else {
        return false;
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml::Value>().ok())
        .and_then(|doc| doc.get("compact").cloned())
        .and_then(|compact| compact.get("models").cloned())
        .is_some()
}

fn write_compact_models_at(path: &std::path::Path, models: &[String]) -> Result<()> {
    use anyhow::Context;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let values: Vec<String> = models
        .iter()
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .fold(Vec::new(), |mut out, m| {
            if !out.iter().any(|existing| existing == m) {
                out.push(m.to_string());
            }
            out
        });
    let line = format!(
        "models = [{}]",
        values
            .iter()
            .map(|m| format!("{m:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut in_compact = false;
    let mut compact_found = false;
    let mut replaced = false;
    let mut out = Vec::new();
    for raw in existing.lines() {
        let trimmed = raw.trim();
        let section = trimmed.split('#').next().unwrap_or_default().trim();
        if section.starts_with('[') {
            if in_compact && !replaced {
                out.push(line.clone());
                replaced = true;
            }
            in_compact = section == "[compact]";
            compact_found |= in_compact;
        }
        if in_compact
            && trimmed
                .split('=')
                .next()
                .is_some_and(|key| key.trim() == "models")
        {
            out.push(line.clone());
            replaced = true;
        } else {
            out.push(raw.to_string());
        }
    }
    if !replaced {
        if !out.is_empty() && !out.last().is_some_and(|existing| existing.is_empty()) {
            out.push(String::new());
        }
        if !compact_found {
            out.push("[compact]".to_string());
        }
        out.push(line);
    }
    std::fs::write(path, format!("{}\n", out.join("\n")))
        .with_context(|| format!("writing {}", path.display()))
}

/// Persist the breakdown-TUI `theme` choice to the config file's top-level `theme` key,
/// preserving every other line (a surgical line edit, not a TOML re-serialize, so user
/// comments and key order survive). Creates the file (and its directory) if absent.
/// Best-effort by the caller: a failure to write should never crash the TUI.
pub fn save_theme(name: &str) -> Result<()> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no config path (HOME/XDG unset)"))?;
    save_theme_at(&path, name)
}

/// Path-taking core of [`save_theme`], factored out so the surgical line edit is unit-testable
/// without touching the real config location.
fn save_theme_at(path: &std::path::Path, name: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let line = format!("theme = \"{name}\"");
    let mut replaced = false;
    let mut out: Vec<String> = existing
        .lines()
        .map(|l| {
            if l.split('=').next().is_some_and(|k| k.trim() == "theme") {
                replaced = true;
                line.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !replaced {
        out.push(line);
    }
    let mut text = out.join("\n");
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

/// Canonicalize a user-supplied provider name to its wire-shape key (`openai` / `anthropic` /
/// `google`), accepting the [`ProviderKind`](crate::llmtrim::ir::ProviderKind) aliases (`claude`,
/// `gemini`, `gpt`, …). Returns `None` for an unknown name, which is silently dropped — same
/// policy as a malformed host in [`normalize_host`].
fn canon_provider(raw: &str) -> Option<String> {
    crate::llmtrim::ir::ProviderKind::from_str(raw.trim())
        .ok()
        .map(|k| k.as_str().to_string())
}

/// One env-first-then-file string-list setting: the env var (comma-split) replaces the file
/// array, each item passes through `norm` (validate/canonicalize, or drop), and survivors are
/// sorted + deduped. Shared by `extra_hosts` and the exclusion lists.
fn resolve_str_list(
    env_value: Option<String>,
    file: Option<&toml::Value>,
    file_key: &str,
    norm: fn(&str) -> Option<String>,
) -> Vec<String> {
    let raw: Vec<String> = match env_value {
        Some(s) => s.split(',').map(str::to_string).collect(),
        None => file
            .and_then(|v| v.get(file_key))
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut out: Vec<String> = raw.iter().filter_map(|s| norm(s)).collect();
    out.sort();
    out.dedup();
    out
}

/// The provider/host exclusion lists. Kept as its own type rather than fields on
/// [`RuntimeConfig`] — surfaced via the additive [`exclusions`] accessor — so the feature stays
/// backward-compatible with the published `llmtrim-core` API. A request whose host or resolved
/// wire-shape provider matches is forwarded verbatim (still MITM'd, just not compressed).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exclusions {
    /// Wire-shape provider names to exclude (`openai`/`anthropic`/`google`; the
    /// [`ProviderKind`](crate::llmtrim::ir::ProviderKind) aliases `claude`/`gemini`/`gpt` are accepted and
    /// canonicalized, unknowns dropped). Coarse by design: the proxy classifies a host by wire
    /// shape, so `openai` excludes *every* OpenAI-shaped host (OpenRouter, Groq, …), not just
    /// `api.openai.com`. Use [`Exclusions::hosts`] to exclude one host precisely.
    pub providers: Vec<String>,
    /// Exact hosts to exclude, normalized like `extra_hosts` (lowercase plain hostnames;
    /// malformed/overbroad entries dropped) and matched **exactly**, so excluding `api.openai.com`
    /// leaves other OpenAI-shaped hosts compressed.
    pub hosts: Vec<String>,
}

/// Pure resolver for the [`Exclusions`] lists. Env (`LLMTRIM_EXCLUDE_*`, comma-split) replaces the
/// file array per list; each item is canonicalized/normalized (or dropped).
fn resolve_exclusions(
    env: impl Fn(&str) -> Option<String>,
    file: Option<&toml::Value>,
) -> Exclusions {
    let env_set = |k: &str| env(k).filter(|s| !s.is_empty());
    Exclusions {
        providers: resolve_str_list(
            env_set("LLMTRIM_EXCLUDE_PROVIDERS"),
            file,
            "exclude_providers",
            canon_provider,
        ),
        hosts: resolve_str_list(
            env_set("LLMTRIM_EXCLUDE_HOSTS"),
            file,
            "exclude_hosts",
            normalize_host,
        ),
    }
}

/// Process-wide [`Exclusions`], loaded once from env + the config file like [`RuntimeConfig::get`].
/// Cached for the process lifetime, so **tests must call `resolve_exclusions` directly** rather
/// than this accessor.
pub fn exclusions() -> &'static Exclusions {
    static CACHE: std::sync::OnceLock<Exclusions> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| resolve_exclusions(|k| std::env::var(k).ok(), cached_config_file()))
}

/// The config TOML parsed once for the whole process (if it exists and parses), shared by
/// [`RuntimeConfig::load`] and [`exclusions`] so the file is read a single time.
fn cached_config_file() -> Option<&'static toml::Value> {
    static FILE: std::sync::OnceLock<Option<toml::Value>> = std::sync::OnceLock::new();
    FILE.get_or_init(load_config_file).as_ref()
}

/// Read + parse the config TOML (if it exists and parses). Wrapped by [`cached_config_file`].
fn load_config_file() -> Option<toml::Value> {
    config_path()
        .filter(|p| p.exists())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
}

/// Normalize + validate a user-supplied intercept host: trim a trailing dot, lowercase, and
/// accept only a plain multi-label DNS hostname (no scheme/path/port/wildcard/whitespace, at
/// least one dot so a bare TLD can't widen the CA, and not a numeric IP literal). Returns `None`
/// for anything unusable, which is silently dropped — a typo'd host simply isn't intercepted
/// (visible in captures), and the name-constrained CA is never widened by a malformed or
/// overbroad entry.
fn normalize_host(raw: &str) -> Option<String> {
    let h = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty()
        || h.starts_with('.')
        || h.starts_with('-')
        || !h.contains('.')
        || h.contains(['/', ':', ' ', '\t', '*', '@', '?'])
    {
        return None;
    }
    // Each dot-separated label: non-empty, ASCII alphanumeric or hyphen only.
    if !h
        .split('.')
        .all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    {
        return None;
    }
    // Reject IPv4 literals (all labels numeric): an IP in a DNS name-constraint is undefined
    // across TLS stacks, so it must never reach the CA's permitted subtrees.
    if h.split('.').all(|l| l.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    Some(h)
}

/// Ledger age-retention in days. Thin accessor over [`RuntimeConfig`] kept for the call sites
/// that only need this one value.
pub fn retention_days() -> Option<i64> {
    RuntimeConfig::get().retention_days
}

/// Configured ledger row cap, or `None` to fall back to the caller's built-in default.
pub fn max_rows() -> Option<i64> {
    RuntimeConfig::get().max_rows
}

/// Configured breakdown retention cap (in turns), or `None` to fall back to the built-in default.
pub fn max_breakdown_turns() -> Option<i64> {
    RuntimeConfig::get().max_breakdown_turns
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legend_is_embedded_and_nonempty() {
        assert!(!FORMAT_LEGEND.trim().is_empty());
        assert!(FORMAT_LEGEND.contains("TOON"));
    }

    #[test]
    fn sub_reads_bare_string_and_env_wins() {
        let file: toml::Value = toml::from_str("sub = \"codex\"").unwrap();
        let no_env = |_: &str| None;
        assert_eq!(
            resolve_sub_provider(&no_env, Some(&file)).as_deref(),
            Some("codex")
        );
        // env overrides the file, and `off` is normalized by the caller (resolve()).
        let env = |k: &str| (k == "LLMTRIM_SUB").then(|| "kimi".to_string());
        assert_eq!(
            resolve_sub_provider(&env, Some(&file)).as_deref(),
            Some("kimi")
        );
    }

    #[test]
    fn sub_reads_table_form_and_tiers() {
        let file: toml::Value = toml::from_str(
            "[sub]\nactive = \"codex\"\n[sub.codex.tiers]\nopus = \"gpt-5.5\"\nsonnet = \"gpt-5.4\"\n",
        )
        .unwrap();
        let no_env = |_: &str| None;
        assert_eq!(
            resolve_sub_provider(&no_env, Some(&file)).as_deref(),
            Some("codex")
        );
        let tiers = resolve_sub_tiers(&no_env, Some(&file));
        assert_eq!(tiers.get("opus").map(String::as_str), Some("gpt-5.5"));
        assert_eq!(tiers.get("sonnet").map(String::as_str), Some("gpt-5.4"));
    }

    #[test]
    fn write_then_read_sub_mapping_round_trips() {
        let dir = std::env::temp_dir().join(format!("llmtrim-sub-test-{}", std::process::id()));
        // dirge-m1ni: clear first — the name is keyed on a recyclable pid.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "preset = \"auto\"\n").unwrap();
        let mut tiers = std::collections::BTreeMap::new();
        tiers.insert("opus".to_string(), "gpt-5.5".to_string());
        tiers.insert("haiku".to_string(), "gpt-5.4-mini".to_string());
        write_sub_mapping_at(&path, "codex", &tiers).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let file: toml::Value = toml::from_str(&text).unwrap();
        let no_env = |_: &str| None;
        assert_eq!(
            resolve_sub_provider(&no_env, Some(&file)).as_deref(),
            Some("codex")
        );
        let read = resolve_sub_tiers(&no_env, Some(&file));
        assert_eq!(read.get("opus").map(String::as_str), Some("gpt-5.5"));
        assert_eq!(read.get("haiku").map(String::as_str), Some("gpt-5.4-mini"));
        // The pre-existing key survives the structured rewrite.
        assert_eq!(
            file.get("preset").and_then(toml::Value::as_str),
            Some("auto")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sub_off_on_cycle_preserves_provider_and_mapping() {
        let dir = std::env::temp_dir().join(format!("llmtrim-sub-onoff-{}", std::process::id()));
        // dirge-m1ni: clear first — the name is keyed on a recyclable pid.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // Enable codex with a customized mapping.
        let mut tiers = std::collections::BTreeMap::new();
        tiers.insert("opus".to_string(), "gpt-5.5-custom".to_string());
        write_sub_mapping_at(&path, "codex", &tiers).unwrap();
        assert_eq!(sub_reenable_provider_at(&path).as_deref(), Some("codex"));

        // Off remembers the provider as `last` and unsets `active`.
        disable_sub_at(&path).unwrap();
        let no_env = |_: &str| None;
        let file: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // `off` is the disabled marker (resolve() normalizes it to None downstream).
        assert_eq!(
            resolve_sub_provider(&no_env, Some(&file)).as_deref(),
            Some("off")
        );
        assert_eq!(sub_reenable_provider_at(&path).as_deref(), Some("codex"));

        // Bare `on` restores codex without wiping the custom mapping.
        let restore = sub_reenable_provider_at(&path).unwrap();
        enable_sub_at(&path, &restore).unwrap();
        let file: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            resolve_sub_provider(&no_env, Some(&file)).as_deref(),
            Some("codex")
        );
        let read = resolve_sub_tiers(&no_env, Some(&file));
        assert_eq!(read.get("opus").map(String::as_str), Some("gpt-5.5-custom"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sub_reenable_none_when_never_enabled() {
        let dir = std::env::temp_dir().join(format!("llmtrim-sub-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "preset = \"auto\"\n").unwrap();
        assert_eq!(sub_reenable_provider_at(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_continuation_is_off_unless_asked_for() {
        let no_env = |_: &str| None;
        // Off by default: the ChatGPT HTTP `/responses` path rejects `previous_response_id`
        // outright, so sending it on every follow-up turn 400s the whole session.
        assert!(!resolve_sub_codex_continuation(&no_env, None));
        let plain: toml::Value = toml::from_str("sub = \"codex\"").unwrap();
        assert!(!resolve_sub_codex_continuation(&no_env, Some(&plain)));
        // Still opt-in-able, for a transport that accepts it.
        let on: toml::Value = toml::from_str("[sub.codex]\nprevious_response_id = true\n").unwrap();
        assert!(resolve_sub_codex_continuation(&no_env, Some(&on)));
        // Env wins both ways.
        let env_on = |k: &str| (k == "LLMTRIM_CODEX_PREVIOUS_RESPONSE_ID").then(|| "1".to_string());
        assert!(resolve_sub_codex_continuation(&env_on, None));
        let env_off =
            |k: &str| (k == "LLMTRIM_CODEX_PREVIOUS_RESPONSE_ID").then(|| "0".to_string());
        assert!(!resolve_sub_codex_continuation(&env_off, Some(&on)));
    }

    #[test]
    fn sub_fallback_env_wins_over_file_mode() {
        let file: toml::Value =
            toml::from_str("[sub]\nactive = \"codex\"\nmode = \"fallback\"\n").unwrap();
        let no_env = |_: &str| None;
        assert!(resolve_sub_fallback(&no_env, Some(&file)));
        // Default is always (false) when no mode key.
        let plain: toml::Value = toml::from_str("sub = \"codex\"").unwrap();
        assert!(!resolve_sub_fallback(&no_env, Some(&plain)));
        // Env overrides the file both ways.
        let env_always = |k: &str| (k == "LLMTRIM_SUB_MODE").then(|| "always".to_string());
        assert!(!resolve_sub_fallback(&env_always, Some(&file)));
        let env_fallback = |k: &str| (k == "LLMTRIM_SUB_MODE").then(|| "fallback".to_string());
        assert!(resolve_sub_fallback(&env_fallback, Some(&plain)));
        // An unknown value is not fallback (defaults to always, the documented default).
        let env_unknown = |k: &str| (k == "LLMTRIM_SUB_MODE").then(|| "wat".to_string());
        assert!(!resolve_sub_fallback(&env_unknown, Some(&plain)));
    }

    #[test]
    fn sub_mode_legacy_on_error_still_means_fallback() {
        // A pre-0.10 config must not silently flip to rerouting every turn.
        let no_env = |_: &str| None;
        for legacy in ["on_error", "on-error", "onerror"] {
            let file: toml::Value =
                toml::from_str(&format!("[sub]\nactive = \"codex\"\nmode = \"{legacy}\"\n"))
                    .unwrap();
            assert!(resolve_sub_fallback(&no_env, Some(&file)), "{legacy}");
        }
        let env = |k: &str| (k == "LLMTRIM_SUB_MODE").then(|| "on-error".to_string());
        assert!(resolve_sub_fallback(&env, None));
    }

    #[test]
    fn sub_chain_reads_order_and_falls_back_to_active() {
        let file: toml::Value =
            toml::from_str("[sub]\nactive = \"codex\"\nchain = [\"kimi\", \"codex\"]\n").unwrap();
        let no_env = |_: &str| None;
        assert_eq!(
            resolve_sub_chain(&no_env, Some(&file)),
            vec!["kimi".to_string(), "codex".to_string()]
        );
        let plain: toml::Value = toml::from_str("sub = \"kimi\"").unwrap();
        assert_eq!(resolve_sub_chain(&no_env, Some(&plain)), vec!["kimi"]);
        let env =
            |key: &str| (key == "LLMTRIM_SUB_CHAIN").then(|| "codex, kimi, codex".to_string());
        assert_eq!(
            resolve_sub_chain(&env, Some(&file)),
            vec!["codex".to_string(), "kimi".to_string(), "codex".to_string()]
        );
    }

    #[test]
    fn granular_sub_editors_preserve_active_mode_and_entries() {
        let dir = std::env::temp_dir().join(format!("llmtrim-sub-edit-{}", std::process::id()));
        // dirge-m1ni: clear first — the name is keyed on a recyclable pid.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // Seed a full mapping + fallback mode.
        let mut tiers = std::collections::BTreeMap::new();
        tiers.insert("opus".to_string(), "gpt-5.5".to_string());
        write_sub_mapping_at(&path, "codex", &tiers).unwrap();
        edit_sub_table_at(&path, |t| {
            t.insert(
                "mode".to_string(),
                toml::Value::String("fallback".to_string()),
            );
        })
        .unwrap();

        // Add a free-form model→model entry via the granular editor.
        let (provider, from, to) = ("codex", "claude-sonnet-4", "gpt-5.4");
        let mut tt = std::collections::BTreeMap::new();
        tt.insert(from.to_string(), to.to_string());
        edit_sub_table_at(&path, |t| {
            let prov = t
                .entry(provider.to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            let tiers = prov
                .as_table_mut()
                .unwrap()
                .entry("tiers".to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            tiers
                .as_table_mut()
                .unwrap()
                .insert(from.to_string(), toml::Value::String(to.to_string()));
        })
        .unwrap();

        let file: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let no_env = |_: &str| None;
        // Active provider, mode, the seeded tier, and the new free-form id all coexist.
        assert_eq!(
            resolve_sub_provider(&no_env, Some(&file)).as_deref(),
            Some("codex")
        );
        assert!(resolve_sub_fallback(&no_env, Some(&file)));
        let read = resolve_sub_tiers(&no_env, Some(&file));
        assert_eq!(read.get("opus").map(String::as_str), Some("gpt-5.5"));
        assert_eq!(
            read.get("claude-sonnet-4").map(String::as_str),
            Some("gpt-5.4")
        );

        // Disabling reroute (active = "off", what `disable_sub` writes) must NOT wipe the mapping
        // or mode: re-enabling with the provider restores everything.
        edit_sub_table_at(&path, |t| {
            t.insert("active".to_string(), toml::Value::String("off".to_string()));
        })
        .unwrap();
        let off_file: toml::Value =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // `off` resolves to None once RuntimeConfig filters it, but the table is still there.
        assert_eq!(
            resolve_sub_provider(&no_env, Some(&off_file)).as_deref(),
            Some("off")
        );
        edit_sub_table_at(&path, |t| {
            t.insert(
                "active".to_string(),
                toml::Value::String("codex".to_string()),
            );
        })
        .unwrap();
        let back: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            resolve_sub_fallback(&no_env, Some(&back)),
            "mode survived off/on"
        );
        let restored = resolve_sub_tiers(&no_env, Some(&back));
        assert_eq!(restored.get("opus").map(String::as_str), Some("gpt-5.5"));
        assert_eq!(
            restored.get("claude-sonnet-4").map(String::as_str),
            Some("gpt-5.4"),
            "free-form entry survived off/on"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sub_effort_env_wins_and_write_round_trips() {
        // Env override wins over file.
        let file: toml::Value =
            toml::from_str("[sub]\nactive = \"codex\"\n[sub.codex]\neffort = \"low\"\n").unwrap();
        let no_env = |_: &str| None;
        assert_eq!(
            resolve_sub_effort(&no_env, Some(&file)).as_deref(),
            Some("low")
        );
        let env_high = |k: &str| (k == "LLMTRIM_CODEX_EFFORT").then(|| "high".to_string());
        assert_eq!(
            resolve_sub_effort(&env_high, Some(&file)).as_deref(),
            Some("high")
        );
        // `none` resolves to None (off).
        let none_env = |k: &str| (k == "LLMTRIM_CODEX_EFFORT").then(|| "none".to_string());
        assert_eq!(resolve_sub_effort(&none_env, Some(&file)), None);

        // Writer round-trips and preserves the mapping; writing `none` clears it.
        let dir = std::env::temp_dir().join(format!("llmtrim-effort-{}", std::process::id()));
        // dirge-m1ni: clear first — the name is keyed on a recyclable pid.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut tiers = std::collections::BTreeMap::new();
        tiers.insert("opus".to_string(), "gpt-5.5".to_string());
        write_sub_mapping_at(&path, "codex", &tiers).unwrap();
        edit_sub_table_at(&path, |t| {
            let prov = t
                .entry("codex".to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            prov.as_table_mut().unwrap().insert(
                "effort".to_string(),
                toml::Value::String("medium".to_string()),
            );
        })
        .unwrap();
        let f: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            resolve_sub_effort(&no_env, Some(&f)).as_deref(),
            Some("medium")
        );
        assert_eq!(
            resolve_sub_tiers(&no_env, Some(&f))
                .get("opus")
                .map(String::as_str),
            Some("gpt-5.5"),
            "effort write preserved the mapping"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lossless_baseline_enables_mvp_stages() {
        let c = DenseConfig::lossless();
        assert!(
            !c.auto,
            "lossless()/`safe` is the bare baseline, not shape-routing"
        );
        assert!(
            c.hygiene && c.serialize,
            "lossless input compression on by default"
        );
        assert!(
            c.dedup && !c.dedup_near,
            "exact dedup on by default (lossless); near-dedup opt-in (lossy)"
        );
        assert!(
            !c.output_control,
            "lossless()/`safe` is the lossless baseline — output shaping is on in the shipped `auto` default (via presets), not in this bare base"
        );
        assert!(
            !c.retrieve,
            "retrieval is opt-in (workload-dependent per eval)"
        );
        assert!(c.serialize_nested, "nested array encoding on by default");
        assert!(!c.strip_base64, "base64 strip is opt-in (lossy)");
        assert_eq!(c.serialize_min_rows, 2);
    }

    #[test]
    fn auto_routed_presets_enable_output_control() {
        // The metric is cost-at-no-quality-loss, not losslessness: every shape `auto`
        // routes to (agent / code / rag / aggressive) enables output control by default.
        // Lossless-only lives in `safe`, the opt-in mode.
        for p in ["code", "rag", "aggressive", "reasoning", "agent"] {
            assert!(
                DenseConfig::preset(p).unwrap().output_control,
                "preset `{p}` enables output control by default"
            );
        }
        // `agent` enables it, but the terse injection is first-turn-only there (see
        // `stages::output`): later tool-call turns leave nothing to shape.
        assert!(
            !DenseConfig::preset("safe").unwrap().output_control,
            "`safe` is the lossless mode — no output shaping"
        );
    }

    #[test]
    fn auto_routed_presets_downscale_images() {
        // Image downscale-to-cap is quality-neutral by construction (the provider resizes to
        // the same cap anyway) → on by default. `safe` leaves images byte-faithful, and the
        // genuinely-lossy `image_detail = "low"` stays opt-in.
        for p in ["agent", "code", "rag", "aggressive"] {
            assert!(
                DenseConfig::preset(p).unwrap().multimodal,
                "preset `{p}` downscales oversized images by default"
            );
        }
        assert!(!DenseConfig::preset("safe").unwrap().multimodal);
        assert!(
            DenseConfig::preset("aggressive")
                .unwrap()
                .image_detail
                .is_none()
        );
    }

    #[test]
    fn auto_routed_presets_strip_base64() {
        // Measured quality-neutral (+0.0pp on bench/data/base64.jsonl): blobs are noise the
        // model can't use, and elision leaves a marker. On by default; `safe` keeps blobs.
        for p in ["agent", "code", "rag", "aggressive"] {
            assert!(
                DenseConfig::preset(p).unwrap().strip_base64,
                "preset `{p}` elides base64 blobs by default"
            );
        }
        assert!(!DenseConfig::preset("safe").unwrap().strip_base64);
    }

    #[test]
    fn tool_minify_schema_rides_with_trim_desc() {
        // The API-safe schema minify (TSCG subset) is semantics-preserving, so it is bundled in
        // exactly the presets that already trim tool descriptions — `agent` and `aggressive`.
        for p in ["agent", "aggressive"] {
            let c = DenseConfig::preset(p).unwrap();
            assert!(
                c.tool_minify_schema && c.tool_trim_desc,
                "preset `{p}` minifies tool schemas alongside description trimming"
            );
        }
        // Presets that don't touch the tool block leave it off (no tool stage at all).
        for p in ["safe", "code", "rag"] {
            assert!(
                !DenseConfig::preset(p).unwrap().tool_minify_schema,
                "preset `{p}` does not minify tool schemas"
            );
        }
        // The `auto` default and the lossless baseline both leave it off.
        assert!(!DenseConfig::default().tool_minify_schema);
        assert!(!DenseConfig::lossless().tool_minify_schema);
    }

    #[test]
    fn agent_shrinks_the_tool_block_without_per_turn_churn() {
        // Issue #9: the tool block is part of the cached prompt prefix, so it must not change
        // turn-to-turn on an agent loop. The agent preset keeps the deterministic trim/minify
        // (cache-stable) and gates selection to the first turn only (byte-stability is proven
        // end-to-end by `agent_tool_block_is_byte_stable_across_turns` in the crate root). Lock
        // the flag lineup here.
        let agent = DenseConfig::preset("agent").unwrap();
        assert!(
            agent.tool_select && agent.tool_trim_desc && agent.tool_minify_schema,
            "agent shrinks the tool block (selection is first-turn-only; trim/minify are cache-stable)"
        );
    }

    #[test]
    fn agent_enables_frugal_directive_but_safe_stays_lossless() {
        // Auto routes tool-call traffic to `agent`, so the agent-loop frugality directive rides
        // there (first-turn-only tail insurance). `safe`/`lossless` is the clean baseline and must
        // never carry a behavioral directive — it's the reference the A/B measures against.
        assert!(
            DenseConfig::preset("agent").unwrap().output_frugal_tools,
            "agent (auto tool-call route) enables the frugality directive"
        );
        for p in ["safe", "lossless"] {
            assert!(
                !DenseConfig::preset(p).unwrap().output_frugal_tools,
                "{p} stays lossless — no behavioral directive"
            );
        }
    }

    #[test]
    fn config_selects_preset_by_name_else_flags() {
        // `preset = "name"` (env or file) selects a named profile — one knob, not ~30 flags.
        let agg = DenseConfig::from_toml_value(toml::from_str("preset = \"aggressive\"").unwrap())
            .unwrap();
        assert!(
            agg.output_control && agg.retrieve,
            "preset key selects the profile"
        );
        // Unknown preset is a surfaced error, not a silent default.
        assert!(
            DenseConfig::from_toml_value(toml::from_str("preset = \"nope\"").unwrap()).is_err()
        );
        // No preset key → explicit per-stage flags are parsed.
        let flags =
            DenseConfig::from_toml_value(toml::from_str("hygiene = false").unwrap()).unwrap();
        assert!(
            !flags.hygiene,
            "explicit flags parse when no preset is named"
        );
    }

    #[test]
    fn presets_layer_over_defaults() {
        assert!(DenseConfig::preset("nope").is_none());
        let rag = DenseConfig::preset("rag").unwrap();
        assert!(rag.retrieve && rag.retrieve_sentence && rag.hygiene && rag.dedup);
        assert!(
            (rag.retrieve_keep_ratio - 0.35).abs() < 1e-9,
            "tight sentence cap"
        );
        let code = DenseConfig::preset("code").unwrap();
        assert!(code.minify_code && code.skeletonize && code.output_control);
        assert!(
            !code.output_compact_code,
            "compact-code output dropped — bench-confirmed pass@1 harm"
        );
        let agg = DenseConfig::preset("aggressive").unwrap();
        assert!(agg.retrieve && agg.skeletonize && agg.ngram && agg.minify_code);

        // Unknown preset name → None (no silent fallback).
        assert!(DenseConfig::preset("ultra").is_none());

        // Cache-first: cache on, but the prefix-varying retrieve/reorder stay OFF.
        let cache = DenseConfig::preset("cache").unwrap();
        assert!(cache.cache && !cache.retrieve && !cache.retrieve_reorder);
        assert!(cache.hygiene && cache.serialize, "still lossless input");

        // Reasoning: Chain-of-Draft output.
        let reasoning = DenseConfig::preset("reasoning").unwrap();
        assert!(reasoning.output_control && reasoning.output_level == "draft");
    }

    #[test]
    fn partial_toml_fills_remaining_from_default() {
        let c: DenseConfig = toml::from_str("serialize = false\n").unwrap();
        assert!(!c.serialize);
        assert!(c.hygiene, "unset fields take the default");
        assert_eq!(c.serialize_min_rows, 2);
        assert!(
            !c.auto,
            "partial config deserialization fills from the lossless baseline, not auto"
        );
    }

    /// Resolve with no env set (every lookup returns `None`) against the given file TOML.
    fn resolve_file(toml_src: &str) -> RuntimeConfig {
        let value: toml::Value = toml::from_str(toml_src).unwrap();
        RuntimeConfig::resolve(|_| None, Some(&value))
    }

    /// Resolve with an explicit env map and the given file TOML.
    fn resolve_env(env: &[(&str, &str)], toml_src: &str) -> RuntimeConfig {
        let value: toml::Value = toml::from_str(toml_src).unwrap();
        let env: std::collections::HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        RuntimeConfig::resolve(|k| env.get(k).cloned(), Some(&value))
    }

    #[test]
    fn compact_models_preserve_order_and_deduplicate() {
        let c = resolve_file("[compact]\nmodels = [\"haiku\", \"sonnet\", \"haiku\", \"\"]\n");
        assert_eq!(c.compact_models, vec!["haiku", "sonnet"]);
    }

    #[test]
    fn compact_only_config_keeps_auto_routing() {
        let value: toml::Value =
            toml::from_str("[compact]\nmodels = [\"haiku\", \"sonnet\"]\n").unwrap();
        assert!(DenseConfig::from_toml_value(value).unwrap().auto);
    }

    #[test]
    fn compact_writer_preserves_existing_config() {
        let dir = std::env::temp_dir().join(format!("llmtrim-compact-test-{}", std::process::id()));
        // dirge-m1ni: clear first — the name is keyed on a recyclable pid.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "capture_dir = \"/captures\"\n[sub]\nactive = \"off\"\n[sub.codex.tiers]\nopus = \"gpt-test\"\n",
        )
        .unwrap();
        write_compact_models_at(&path, &["haiku".into(), "sonnet".into(), "haiku".into()]).unwrap();
        let file: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(resolve_compact_models(Some(&file)), vec!["haiku", "sonnet"]);
        assert_eq!(
            file.get("capture_dir").and_then(toml::Value::as_str),
            Some("/captures")
        );
        assert_eq!(
            file.get("sub")
                .and_then(|v| v.get("codex"))
                .and_then(|v| v.get("tiers"))
                .and_then(|v| v.get("opus"))
                .and_then(toml::Value::as_str),
            Some("gpt-test")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compact_writer_handles_commented_or_nonfinal_section() {
        let dir = std::env::temp_dir().join(format!(
            "llmtrim-compact-section-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# keep me\n[compact] # routing policy\n# models intentionally omitted\n[sub]\nactive = \"off\"\n",
        )
        .unwrap();
        write_compact_models_at(&path, &["haiku".into()]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let file: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(resolve_compact_models(Some(&file)), vec!["haiku"]);
        assert_eq!(text.matches("[compact]").count(), 1);
        assert!(text.contains("# keep me"));
        assert!(text.find("models =").unwrap() < text.find("[sub]").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_days_parses_positive_only() {
        assert_eq!(resolve_file("retention_days = 30").retention_days, Some(30));
        assert_eq!(
            resolve_file("retention_days = 0").retention_days,
            None,
            "0 disables age retention"
        );
        assert_eq!(resolve_file("retention_days = -5").retention_days, None);
        assert_eq!(resolve_file("hygiene = true").retention_days, None);
    }

    #[test]
    fn env_wins_over_file_for_every_runtime_setting() {
        let file = "\
            upstream_proxy = \"http://file:3128\"\n\
            capture_dir = \"/file/cap\"\n\
            db_path = \"/file/db.sqlite\"\n\
            no_update_check = false\n\
            bind = \"127.0.0.1\"\n\
            capture_max_mb = 10\n\
            retention_days = 7\n\
            max_rows = 1000\n\
            max_breakdown_turns = 100\n";
        let c = resolve_env(
            &[
                ("LLMTRIM_UPSTREAM_PROXY", "http://env:8080"),
                ("LLMTRIM_CAPTURE_DIR", "/env/cap"),
                ("LLMTRIM_DB_PATH", "/env/db.sqlite"),
                ("LLMTRIM_NO_UPDATE_CHECK", "1"),
                ("LLMTRIM_BIND", "0.0.0.0"),
                ("LLMTRIM_CAPTURE_MAX_MB", "99"),
                ("LLMTRIM_RETENTION_DAYS", "14"),
                ("LLMTRIM_MAX_ROWS", "2000"),
                ("LLMTRIM_MAX_BREAKDOWN_TURNS", "200"),
            ],
            file,
        );
        assert_eq!(c.upstream_proxy.as_deref(), Some("http://env:8080"));
        assert_eq!(c.capture_dir, Some(PathBuf::from("/env/cap")));
        assert_eq!(c.db_path, Some(PathBuf::from("/env/db.sqlite")));
        assert!(c.no_update_check);
        assert_eq!(c.bind.as_deref(), Some("0.0.0.0"));
        assert_eq!(c.capture_max_mb, Some(99));
        assert_eq!(c.retention_days, Some(14));
        assert_eq!(c.max_rows, Some(2000));
        assert_eq!(c.max_breakdown_turns, Some(200));
    }

    /// The two retention caps parse from env/file and reject non-positive values, like
    /// `retention_days`.
    #[test]
    fn row_caps_parse_positive_only() {
        assert_eq!(resolve_file("max_rows = 5000").max_rows, Some(5000));
        assert_eq!(resolve_file("max_rows = 0").max_rows, None);
        assert_eq!(resolve_file("max_rows = -1").max_rows, None);
        assert_eq!(
            resolve_file("max_breakdown_turns = 100000").max_breakdown_turns,
            Some(100_000)
        );
        assert_eq!(
            resolve_file("max_breakdown_turns = 0").max_breakdown_turns,
            None
        );
    }

    #[test]
    fn file_used_when_env_absent() {
        let c = resolve_file(
            "upstream_proxy = \"http://file:3128\"\nbind = \"::1\"\ncapture_max_mb = 0\n",
        );
        assert_eq!(c.upstream_proxy.as_deref(), Some("http://file:3128"));
        assert_eq!(c.bind.as_deref(), Some("::1"));
        assert_eq!(c.capture_max_mb, Some(0), "0 from file disables the cap");
    }

    #[test]
    fn no_update_check_true_on_env_presence_even_empty() {
        // var_os-style presence: any value (incl. empty) means "set".
        let c = resolve_env(
            &[("LLMTRIM_NO_UPDATE_CHECK", "")],
            "no_update_check = false",
        );
        assert!(c.no_update_check);
    }

    #[test]
    fn extra_hosts_env_replaces_file_and_normalizes() {
        // Env comma list wins over the file array; entries are lowercased, sorted, deduped.
        let c = resolve_env(
            &[(
                "LLMTRIM_EXTRA_HOSTS",
                "LLM.Acme.com, api.acme.com, llm.acme.com",
            )],
            "extra_hosts = [\"ignored.example\"]",
        );
        assert_eq!(c.extra_hosts, vec!["api.acme.com", "llm.acme.com"]);
    }

    #[test]
    fn extra_hosts_from_file_when_env_absent() {
        let c = resolve_file("extra_hosts = [\"llm.acme.com\", \"gw.example.net\"]");
        assert_eq!(c.extra_hosts, vec!["gw.example.net", "llm.acme.com"]);
    }

    /// Resolve the exclusion lists with an explicit env map and file TOML.
    fn exclusions_env(env: &[(&str, &str)], toml_src: &str) -> Exclusions {
        let value: toml::Value = toml::from_str(toml_src).unwrap();
        let env: std::collections::HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        resolve_exclusions(|k| env.get(k).cloned(), Some(&value))
    }

    #[test]
    fn exclude_providers_canonicalizes_and_drops_unknown() {
        // Aliases map to canonical names; unknown entries are dropped; sorted + deduped.
        let ex = exclusions_env(
            &[(
                "LLMTRIM_EXCLUDE_PROVIDERS",
                "claude, anthropic, gemini, bogus",
            )],
            "exclude_providers = [\"ignored\"]",
        );
        assert_eq!(ex.providers, vec!["anthropic", "google"]);
    }

    #[test]
    fn exclude_providers_from_file_when_env_absent() {
        let value = toml::from_str("exclude_providers = [\"openai\", \"claude\"]").unwrap();
        let ex = resolve_exclusions(|_| None, Some(&value));
        assert_eq!(ex.providers, vec!["anthropic", "openai"]);
    }

    #[test]
    fn exclude_hosts_normalizes_like_extra_hosts() {
        // Env wins over file; lowercased, sorted, deduped; malformed dropped.
        let ex = exclusions_env(
            &[(
                "LLMTRIM_EXCLUDE_HOSTS",
                "API.OpenAI.com, *.bad, openrouter.ai",
            )],
            "exclude_hosts = [\"ignored.example\"]",
        );
        assert_eq!(ex.hosts, vec!["api.openai.com", "openrouter.ai"]);
    }

    #[test]
    fn exclude_env_replaces_file_for_both_lists() {
        // Env (comma-split) wins over the file array for each list independently.
        let ex = exclusions_env(
            &[
                ("LLMTRIM_EXCLUDE_PROVIDERS", "openai"),
                ("LLMTRIM_EXCLUDE_HOSTS", "api.openai.com"),
            ],
            "exclude_providers = [\"anthropic\"]\nexclude_hosts = [\"api.anthropic.com\"]\n",
        );
        assert_eq!(ex.providers, vec!["openai"]);
        assert_eq!(ex.hosts, vec!["api.openai.com"]);
    }

    #[test]
    fn exclude_keys_keep_auto_shape_routing() {
        // A config that sets only the exclude keys must keep `auto` routing, not downgrade it.
        for src in [
            "exclude_providers = [\"anthropic\"]",
            "exclude_hosts = [\"api.anthropic.com\"]",
        ] {
            let c = DenseConfig::from_toml_value(toml::from_str(src).unwrap()).unwrap();
            assert!(c.auto, "exclude-only config `{src}` must keep auto routing");
        }
    }

    #[test]
    fn extra_hosts_drops_malformed_and_overbroad() {
        // No dot (bare TLD), scheme/path/port, wildcard, leading dot/hyphen, whitespace → dropped.
        for bad in [
            "com",
            "https://llm.acme.com",
            "llm.acme.com/v1",
            "llm.acme.com:443",
            "*.acme.com",
            ".acme.com",
            "-acme.com",
            "ac me.com",
            "1.2.3.4",   // IPv4 literal: undefined as a DNS name-constraint
            "127.0.0.1", // loopback IPv4
        ] {
            let c = resolve_env(&[("LLMTRIM_EXTRA_HOSTS", bad)], "");
            assert!(c.extra_hosts.is_empty(), "expected `{bad}` to be dropped");
        }
        // A trailing dot (FQDN form) is normalized away, not rejected.
        let c = resolve_env(&[("LLMTRIM_EXTRA_HOSTS", "llm.acme.com.")], "");
        assert_eq!(c.extra_hosts, vec!["llm.acme.com"]);
    }

    #[test]
    fn retention_key_does_not_disturb_compression_config() {
        // A config that only sets retention parses as the default compression config — the
        // key is orthogonal and must not be rejected or alter stage flags.
        let c =
            DenseConfig::from_toml_value(toml::from_str("retention_days = 30").unwrap()).unwrap();
        assert!(
            c.hygiene && c.serialize,
            "retention_days is ignored by DenseConfig"
        );
    }

    #[test]
    fn runtime_only_keys_keep_auto_shape_routing() {
        // A config that sets only RuntimeConfig keys (no compression keys) must keep the `auto`
        // shape-routing default, not silently fall through to the bare `auto = false` flag set.
        for src in [
            "capture_dir = \"/tmp/cap\"",
            "bind = \"0.0.0.0\"",
            "upstream_proxy = \"http://p:3128\"",
            "extra_hosts = [\"llm.acme.com\"]",
            "no_update_check = true",
            "db_path = \"/tmp/db\"\ncapture_max_mb = 100\nretention_days = 7",
        ] {
            let c = DenseConfig::from_toml_value(toml::from_str(src).unwrap()).unwrap();
            assert!(c.auto, "runtime-only config `{src}` must keep auto routing");
        }
        // A compression key alongside a runtime key still selects explicit flags (auto off).
        let c = DenseConfig::from_toml_value(
            toml::from_str("capture_dir = \"/tmp/cap\"\nhygiene = false").unwrap(),
        )
        .unwrap();
        assert!(
            !c.auto && !c.hygiene,
            "a compression key opts into explicit flags"
        );
    }
}
