//! Breadcrumb tool schemas for a small context window (dirge-tva8).
//!
//! # The problem
//!
//! dirge's opening request is large before the model has done anything.
//! Measured, same task and model, two configurations:
//!
//! | tools | first request |
//! | --- | --- |
//! | 34 (built-in only) | 16,172 prompt tokens |
//! | 75 (plus MCP servers) | 32,621 prompt tokens |
//!
//! The second exceeds a 32k window in its entirety — the run could not take a
//! single turn, and the only symptom was the context manager force-ending
//! every turn. Even the first spends a quarter of a 64k window before the task
//! is read.
//!
//! Almost all of it is tool schemas: descriptions written for a large model
//! with room to spare, several of which run to paragraphs with worked
//! examples.
//!
//! # What this does
//!
//! Trims each tool's description to its first sentence and each parameter's
//! description to a short clause, leaving names, types, required-ness and
//! enums untouched. The model keeps everything it needs to form a CALL — which
//! argument goes where, what values are legal — and loses the prose about when
//! to prefer one tool over another.
//!
//! That trade is deliberate and it is not free: the long descriptions exist
//! because they improve tool SELECTION. This is for the case where the
//! alternative is not "slightly worse selection" but "the prompt does not
//! fit", so it is off unless the window is genuinely small.
//!
//! # What it does NOT do
//!
//! It does not drop tools. Which tools a task needs is not knowable up front,
//! and a model that cannot see a tool cannot ask for it — the failure is
//! silent and looks like the model being incapable. Trimming prose degrades
//! gracefully; removing a tool does not.

use serde_json::Value;

/// Windows at or below this get breadcrumb schemas under `auto`.
///
/// Chosen from the measurement above: the built-in tool surface is ~16k
/// tokens, so at 64k it is a quarter of the window — tight but workable — and
/// at 32k it is half, which leaves too little for the task. The threshold sits
/// between them rather than at either, so a 64k local model keeps full
/// descriptions and a 32k one does not.
pub const SMALL_WINDOW_TOKENS: u64 = 48_000;

/// Longest description kept for a tool, in bytes. Enough for one real
/// sentence; anything longer is prose about when to choose the tool.
const TOOL_DESC_BYTES: usize = 180;

/// Longest description kept for a single parameter.
const PARAM_DESC_BYTES: usize = 80;

/// Whether breadcrumb schemas apply to a window of `ctx_max` tokens.
pub fn applies(ctx_max: u64) -> bool {
    ctx_max <= SMALL_WINDOW_TOKENS
}

/// Explicit on/off, overriding the window test. `None` (the default) decides
/// from the window.
static FORCED: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();

/// The resolved window, installed once at startup.
static WINDOW: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Install the window the tool surface is sized against, and any explicit
/// override, at startup.
///
/// Deliberately the SAME number the session gauge and the loop's compaction
/// math resolve — passed in rather than re-derived — so the tool surface can
/// never be sized against a different window than the one the run folds
/// against. Re-deriving it here is exactly the duplicate that drifts.
pub fn init(ctx_max: u64, forced: Option<bool>) {
    let _ = WINDOW.set(ctx_max);
    let _ = FORCED.set(forced);
}

/// The window in force, or the default when nothing was installed (tests, and
/// any host that never calls [`init`]).
pub fn window() -> u64 {
    *WINDOW
        .get()
        .unwrap_or(&crate::config::DEFAULT_CONTEXT_WINDOW)
}

/// Whether breadcrumb schemas are in force for this process.
pub fn in_force() -> bool {
    match FORCED.get().copied().flatten() {
        Some(explicit) => explicit,
        None => applies(window()),
    }
}

/// The first sentence of `text`, capped at `max_bytes`.
///
/// Sentence-first rather than a hard truncation because the first sentence of
/// a tool description is nearly always what the tool DOES, and the rest is
/// when to use it. Cutting mid-word and appending an ellipsis would keep the
/// same bytes and read as damage.
fn first_sentence(text: &str, max_bytes: usize) -> String {
    let text = text.trim();
    // A sentence ends at `. ` or a newline — not at a bare `.`, which also
    // appears in `file.rs`, `0.5`, and `e.g.`.
    let end = text
        .find(". ")
        .map(|i| i + 1)
        .or_else(|| text.find('\n'))
        .unwrap_or(text.len());
    let cut = end.min(max_bytes);
    let cut = crate::text::char_boundary_at_or_before(text, cut);
    text[..cut].trim_end().to_string()
}

/// A parameters schema with every `description` shortened.
///
/// Walks the whole tree, so nested object properties and array item schemas
/// are covered. Everything else — `type`, `enum`, `required`, `properties`,
/// `items` — is left exactly as it was: those are what make a call
/// well-formed, and trimming them would produce calls that fail validation,
/// trading a context problem for a correctness one.
fn compact_params(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if k == "description"
                    && let Some(s) = v.as_str()
                {
                    out.insert(
                        k.clone(),
                        Value::String(first_sentence(s, PARAM_DESC_BYTES)),
                    );
                    continue;
                }
                out.insert(k.clone(), compact_params(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(compact_params).collect()),
        other => other.clone(),
    }
}

/// Breadcrumb form of a tool description.
pub fn compact_description(description: &str) -> String {
    first_sentence(description, TOOL_DESC_BYTES)
}

/// Breadcrumb form of a parameters schema.
pub fn compact_parameters(parameters: &Value) -> Value {
    compact_params(parameters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The WIRING, not the mechanism: `loop_tool_to_rig_definition` is the one
    /// point every tool becomes a provider schema, and it must consult this.
    ///
    /// Without this the module could be perfect and never called — the failure
    /// mode docs/verification-discipline.md calls "signal never fed", and the
    /// one a unit test of `compact_description` cannot see.
    ///
    /// `in_force()` reads a process-global installed once, so this drives the
    /// pure decision the call site makes rather than trying to install one.
    #[test]
    fn the_schema_builder_consults_the_breadcrumb_decision() {
        let src = include_str!("rig_stream_factory.rs");
        let f = src
            .split_once("pub fn loop_tool_to_rig_definition")
            .expect("the schema builder still exists under that name")
            .1;
        let body = &f[..f.find("\n}").unwrap_or(f.len())];
        assert!(
            body.contains("compact_schema::in_force()"),
            "the one place tools become provider schemas must ask whether \
             breadcrumbs apply, or this module is unreachable"
        );
        assert!(
            body.contains("compact_description") && body.contains("compact_parameters"),
            "and must apply both halves — a trimmed description with full \
             parameter prose saves almost nothing"
        );
    }

    #[test]
    fn the_threshold_splits_the_two_measured_cases() {
        assert!(applies(32_000), "a 32k window needs breadcrumbs");
        assert!(!applies(65_536), "a 64k window does not");
        assert!(!applies(128_000));
        assert!(!applies(1_000_000));
    }

    /// The first sentence is kept whole, and the rest — the prose about when
    /// to prefer this tool — is dropped.
    #[test]
    fn a_description_keeps_its_first_sentence() {
        let long = "Read a file from disk. Prefer this over bash cat because it \
                    returns line numbers, handles binary detection, and applies \
                    the read cache. Use `offset` and `limit` for large files.";
        assert_eq!(compact_description(long), "Read a file from disk.");
    }

    /// A period inside a filename, a version, or `e.g.` is not a sentence end —
    /// splitting there would cut a description to a fragment.
    #[test]
    fn a_period_that_is_not_a_sentence_end_does_not_split() {
        assert_eq!(
            compact_description("Edit main.rs in place."),
            "Edit main.rs in place."
        );
        assert_eq!(
            compact_description("Wait up to 0.5 seconds."),
            "Wait up to 0.5 seconds."
        );
    }

    /// A single sentence longer than the cap is still bounded, and the cut
    /// lands on a character boundary rather than splitting a codepoint.
    #[test]
    fn an_overlong_sentence_is_bounded_at_a_char_boundary() {
        let long = format!("Do {} things", "χ".repeat(400));
        let got = compact_description(&long);
        assert!(got.len() <= TOOL_DESC_BYTES);
        assert!(got.is_char_boundary(got.len()));
    }

    /// THE CONTRACT: everything needed to form a well-formed call survives.
    /// Trimming a type or an enum would trade a context problem for calls that
    /// fail validation, which is strictly worse.
    #[test]
    fn compacting_preserves_everything_a_call_needs() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the file. Must be absolute, \
                                    not relative; relative paths are rejected because \
                                    the agent's cwd can change mid-run."
                },
                "mode": {
                    "type": "string",
                    "enum": ["read", "write"],
                    "description": "What to do."
                },
                "opts": {
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "integer",
                            "description": "How many lines. Defaults to 2000 which is \
                                            usually plenty for source files."
                        }
                    }
                }
            },
            "required": ["path"]
        });

        let got = compact_parameters(&schema);

        assert_eq!(got["type"], "object");
        assert_eq!(got["required"], json!(["path"]));
        assert_eq!(got["properties"]["path"]["type"], "string");
        assert_eq!(got["properties"]["mode"]["enum"], json!(["read", "write"]));
        assert_eq!(
            got["properties"]["opts"]["properties"]["limit"]["type"],
            "integer"
        );

        // ...and the prose is trimmed, at every depth.
        assert_eq!(
            got["properties"]["path"]["description"],
            "Absolute path to the file."
        );
        assert_eq!(
            got["properties"]["opts"]["properties"]["limit"]["description"],
            "How many lines."
        );
        // A short description is untouched.
        assert_eq!(got["properties"]["mode"]["description"], "What to do.");
    }

    /// It has to actually save something, or it is complexity for nothing.
    /// The real read schema is the fixture because that is the tool whose
    /// description grew the most.
    #[test]
    fn compacting_a_realistic_schema_is_a_large_saving() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description":
                    "Absolute path to the file to read. Relative paths are rejected. \
                     Use find_files or glob first if you do not know the exact path, \
                     and prefer this tool over `bash cat` in every case." },
                "reason": { "type": "string", "description":
                    "One short sentence on why you are reading this file. Shown to \
                     the user in the activity ticker so they can follow what the \
                     agent is doing without reading every tool call." }
            },
            "required": ["path"]
        });
        let before = schema.to_string().len();
        let after = compact_parameters(&schema).to_string().len();
        assert!(
            after * 2 < before,
            "breadcrumbs must more than halve a prose-heavy schema: {before} → {after}"
        );
    }
}
