//! Few-shot tool-use exemplars with lexical retrieval.
//!
//! Research on tool-calling shows large reliability gains from a handful
//! of in-context demonstrations (2–5 is the sweet spot). This module
//! holds a small curated corpus of worked tool-use examples and a
//! query-aware retriever that selects the most relevant ones for the
//! user's task, so only on-topic demonstrations are injected (not a
//! static wall of examples).
//!
//! Retrieval is **lexical**, scored with the in-repo `nucleo-matcher`
//! fuzzy matcher rather than embeddings: the default provider (DeepSeek)
//! exposes no embeddings endpoint, and the project deliberately excludes
//! local-embedding deps (see `Cargo.toml` CVE-2026 audit notes). Each
//! exemplar's keywords are scored against the user's task; the
//! top-scoring exemplars above a floor are returned.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// One worked demonstration of good tool use.
#[derive(Debug, Clone, Copy)]
pub struct Exemplar {
    /// Space-separated salient terms used for lexical matching against
    /// the user's task. Keep these the words a user would actually use.
    pub keywords: &'static str,
    /// One-line description of the task the demo solves.
    pub task: &'static str,
    /// The worked tool-call sequence (compact, arrow-separated).
    pub demo: &'static str,
}

/// Curated corpus. Small and tool-focused — each entry demonstrates the
/// correct *ordering* and *argument shape* for a common operation, which
/// is where open models most often slip.
pub const EXEMPLARS: &[Exemplar] = &[
    Exemplar {
        keywords: "edit change modify update fix function file code line replace",
        task: "Change code in an existing file",
        demo: "grep(pattern=\"fn handle_login\", path=\"src\") \
               → read(path=\"src/auth.rs\", offset=40, limit=60) \
               → edit(path=\"src/auth.rs\", old_text=\"<exact lines incl. indentation>\", \
               new_text=\"<replacement>\")  // read before editing; match text exactly",
    },
    Exemplar {
        keywords: "find locate search where defined symbol function grep usage references",
        task: "Locate where something is defined or used",
        demo: "grep(pattern=\"struct Config\", path=\"src\", context_lines=2)  \
               // use grep/find_files to locate first; read only the files it points to, \
               not the whole tree",
    },
    Exemplar {
        keywords: "rename refactor multiple files several across move apply patch",
        task: "Make related changes across several files at once",
        demo: "apply_patch(operations=[{update, path=\"src/a.rs\", old_text=\"OldName\", \
               new_text=\"NewName\"}, {update, path=\"src/b.rs\", ...}])  \
               // one apply_patch with ordered ops; stops on first failure",
    },
    Exemplar {
        keywords: "create new file write add module",
        task: "Create a brand-new file",
        demo: "write(path=\"src/feature.rs\", content=\"<plain UTF-8 source>\")  \
               // write only for new files / full rewrites; content is plain text, not JSON",
    },
    Exemplar {
        keywords: "run test build check command cargo npm pytest compile verify",
        task: "Run tests or a build and act on the result",
        demo: "bash(command=\"cargo test mymod 2>&1 | tail -20\")  \
               // pipe heavy output through tail/grep; read the failure and adapt — \
               do not re-run the identical command hoping it passes",
    },
    Exemplar {
        keywords: "read inspect multiple files independent parallel understand",
        task: "Inspect several independent files",
        demo: "read(path=\"src/a.rs\") and read(path=\"src/b.rs\") in parallel  \
               // independent reads go in one batch of parallel tool calls; \
               dependent calls run sequentially",
    },
];

/// Per-keyword floor: a keyword is only credited when it matches some
/// **single** query token at least this well. Matching per-token (not
/// against the whole task string) is what prevents a keyword's letters
/// from being gathered as a gappy subsequence spread across unrelated
/// words. Calibrated from observed nucleo scores: a clean word/prefix
/// token match scores ≳110, while cross-token noise stays well below.
const KEYWORD_MATCH_FLOOR: u32 = 100;

/// Retrieve up to `k` exemplars most relevant to `query`, best first.
/// Returns empty when no exemplar has a credited keyword — so an
/// unrelated task injects no examples at all.
pub fn retrieve(query: &str, k: usize) -> Vec<&'static Exemplar> {
    if query.trim().is_empty() || k == 0 {
        return Vec::new();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    // Tokenize the task once into lowercase alphanumeric words.
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect();

    let mut scored: Vec<(u32, &'static Exemplar)> = EXEMPLARS
        .iter()
        .map(|ex| (score_exemplar(&mut matcher, &tokens, ex), ex))
        .filter(|(s, _)| *s > 0)
        .collect();

    // Highest score first; stable so corpus order breaks ties.
    scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    scored.into_iter().take(k).map(|(_, ex)| ex).collect()
}

/// Relevance of one exemplar to the task `tokens`. Each of the
/// exemplar's keywords is scored against every query token; the keyword
/// is credited with its best per-token score, but only when that score
/// clears [`KEYWORD_MATCH_FLOOR`] (a real word/prefix hit). The credited
/// scores are summed, so a task mentioning several of an exemplar's
/// terms ranks above one mentioning a single term.
fn score_exemplar(matcher: &mut Matcher, tokens: &[String], ex: &Exemplar) -> u32 {
    ex.keywords
        .split_whitespace()
        .map(|kw| {
            let pat = Pattern::parse(kw, CaseMatching::Ignore, Normalization::Smart);
            let best = tokens
                .iter()
                .filter_map(|tok| {
                    let mut buf: Vec<char> = Vec::new();
                    pat.score(Utf32Str::new(tok, &mut buf), matcher)
                })
                .max()
                .unwrap_or(0);
            if best >= KEYWORD_MATCH_FLOOR { best } else { 0 }
        })
        .sum()
}

/// Render retrieved exemplars into the injected guidance block. Returns
/// `None` when `exemplars` is empty so callers can skip injection.
pub fn format_block(exemplars: &[&Exemplar]) -> Option<String> {
    if exemplars.is_empty() {
        return None;
    }
    let mut out = String::from(
        "[Tool-use examples] Relevant demonstrations of effective tool use for a task \
         like this. Follow the same ordering and argument shapes; they are illustrative, \
         not instructions to run.\n",
    );
    for ex in exemplars {
        out.push_str("\n- ");
        out.push_str(ex.task);
        out.push_str(":\n  ");
        out.push_str(ex.demo);
        out.push('\n');
    }
    Some(out)
}

/// Convenience: retrieve + format in one call for a given task query.
pub fn block_for_task(query: &str, k: usize) -> Option<String> {
    format_block(&retrieve(query, k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_task_retrieves_edit_exemplar_first() {
        let hits = retrieve("change the handle_login function in the auth file", 2);
        assert!(!hits.is_empty(), "edit task should retrieve something");
        assert!(
            hits[0].task.to_lowercase().contains("change code"),
            "top hit should be the edit exemplar, got: {}",
            hits[0].task
        );
    }

    #[test]
    fn test_running_task_retrieves_test_exemplar() {
        let hits = retrieve("run the cargo test suite and fix failures", 2);
        assert!(
            hits.iter()
                .any(|e| e.task.to_lowercase().contains("run tests")),
            "should surface the test-running exemplar"
        );
    }

    #[test]
    fn multifile_rename_retrieves_apply_patch_exemplar() {
        let hits = retrieve("rename a symbol across several files", 3);
        assert!(
            hits.iter()
                .any(|e| e.task.to_lowercase().contains("several files")),
            "should surface the apply_patch exemplar"
        );
    }

    #[test]
    fn unrelated_task_retrieves_nothing() {
        // No tool-operation vocabulary → no exemplars injected.
        let hits = retrieve("what is your favorite color", 3);
        assert!(
            hits.is_empty(),
            "off-topic task should inject no exemplars, got {} ({:?})",
            hits.len(),
            hits.iter().map(|e| e.task).collect::<Vec<_>>()
        );
    }

    #[test]
    fn k_caps_the_number_returned() {
        let hits = retrieve(
            "find locate read edit change run test create file rename",
            2,
        );
        assert!(hits.len() <= 2, "k must cap results, got {}", hits.len());
    }

    #[test]
    fn empty_query_returns_empty() {
        assert!(retrieve("", 3).is_empty());
        assert!(retrieve("   ", 3).is_empty());
    }

    #[test]
    fn format_block_none_when_empty() {
        assert!(format_block(&[]).is_none());
    }

    #[test]
    fn format_block_carries_tag_and_demos() {
        let hits = retrieve("edit a function in a file", 2);
        let block = format_block(&hits).expect("non-empty");
        assert!(
            block.contains("[Tool-use examples]"),
            "block must carry the tag"
        );
        assert!(
            block.contains("→") || block.contains("("),
            "block must include demo calls"
        );
    }

    #[test]
    fn block_for_task_threads_through() {
        assert!(block_for_task("rename across several files", 2).is_some());
        assert!(block_for_task("tell me a joke", 2).is_none());
    }
}
