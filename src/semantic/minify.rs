//! Tree-sitter source minification (token-efficient file I/O).
//!
//! Ported from vix `internal/daemon/treesitter.go`. Parses source with the
//! per-language grammar, collects leaf tokens (dropping comments + all
//! whitespace), then re-serializes with the MINIMUM spacing needed to keep
//! adjacent tokens from merging (`let x` must not become `letx`). The result
//! is re-parsed and rejected if minification introduced a syntax error.
//!
//! ## Language gating (deliberate)
//!
//! [`language_for_ext`] returns a grammar ONLY for languages that are both
//! compiled in (per the `semantic-*` cargo features) AND collapse-safe — i.e.
//! mandatory-terminator languages with NO automatic-semicolon-insertion, NO
//! preprocessor, and no significant whitespace (Rust, Java).
//!
//! Excluded until their per-language annotators are ported (see dirge-8e27):
//! Python (indentation), Go/Ruby (newline-driven auto-semicolon), Bash
//! (newlines), Clojure (whitespace-as-delimiter), Elixir, TypeScript/JavaScript
//! (ASI), and C/C++ (newline-terminated preprocessor directives). For those
//! (and any
//! unsupported extension) `language_for_ext` returns `None`, and callers MUST
//! fall back to a plain read. [`minify`] also returns `None` whenever the
//! input doesn't parse cleanly or the minified output fails re-validation, so
//! it never yields a half-minified / corrupted result.

// The minify primitive is exercised by its own tests now; the production
// callers (`read_minified` / `edit_minified`) land in dirge-759c / dirge-wxws.
// Until then this is a deliberately-exported-but-not-yet-integrated surface.
#![allow(dead_code)]

use tree_sitter::{Node, Parser};

/// A collected leaf token plus the separator emitted before the NEXT token.
struct Token {
    text: String,
    /// Separator to emit *after* this token, before the next one (e.g. `"\n"`
    /// after a kept comment so it can't comment out following code).
    trailing_sep: &'static str,
}

fn is_word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Operator-class chars where a following `.` would otherwise read as part of
/// the operator (e.g. `=.foo`). `?`/`!` are excluded so `?.`/`!.` chaining
/// isn't split.
fn is_operator_char(c: u8) -> bool {
    matches!(
        c,
        b'=' | b'+' | b'-' | b'*' | b'/' | b'<' | b'>' | b'&' | b'|' | b'^' | b'%' | b':' | b'~'
    )
}

/// The grammar for `ext`, or `None` when the language is unsupported, not
/// collapse-safe, or its `semantic-*` feature is disabled. `ext` may include a
/// leading `.`.
pub fn language_for_ext(ext: &str) -> Option<tree_sitter::Language> {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    match e.as_str() {
        #[cfg(feature = "semantic-rust")]
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
        #[cfg(feature = "semantic-java")]
        "java" => Some(tree_sitter_java::LANGUAGE.into()),
        // Intentionally NOT here (each needs newline-aware annotation in
        // dirge-8e27 before it's safe):
        //   - TypeScript/JavaScript: Automatic Semicolon Insertion — collapsing
        //     newlines can change semantics in a way that still PARSES (e.g.
        //     `a=b+c` <nl> `(d).foo()` → `a=b+c(d).foo()`), so re-validation
        //     can't catch it.
        //   - C/C++: the preprocessor (`#include`/`#define`) is newline-
        //     terminated; collapsing merges directives with the next line.
        //     (Re-validation catches it → safe fallback, but it means C/C++
        //     rarely minify in practice, so we don't advertise them.)
        _ => None,
    }
}

/// Minify `source` for the language inferred from `ext`. Returns `None` when:
/// the language isn't collapse-safe/supported, the source doesn't parse
/// cleanly (we never minify a file the grammar can't fully understand), or the
/// minified output fails re-validation. A `None` means "fall back to a plain
/// read" — never a corrupted result.
pub fn minify(ext: &str, source: &str) -> Option<String> {
    let language = language_for_ext(ext)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    // Only minify files the grammar parses cleanly — minifying a file with
    // pre-existing parse errors risks dropping/merging tokens unsafely.
    if root.has_error() {
        return None;
    }

    let src = source.as_bytes();
    let mut tokens: Vec<Token> = Vec::new();
    collect_leaves(root, src, &mut tokens);
    let out = render(&tokens);
    if out.is_empty() {
        return None;
    }

    // Re-validate: the minified output must still parse without new errors.
    let reparsed = parser.parse(&out, None)?;
    if reparsed.root_node().has_error() {
        return None;
    }
    Some(out)
}

/// Recursively collect leaf tokens, dropping whitespace and comments.
fn collect_leaves(node: Node, src: &[u8], tokens: &mut Vec<Token>) {
    let kind = node.kind();
    // Whitespace leaf nodes some grammars expose — never emit.
    if matches!(kind, "\n" | "\t" | " ") {
        return;
    }
    // Comments are stripped entirely (token-efficiency is the point). We don't
    // keep them, so no trailing-newline bookkeeping is needed.
    if matches!(
        kind,
        "comment" | "line_comment" | "block_comment" | "multiline_comment"
    ) {
        return;
    }
    // Leaf node → emit its text. tree-sitter leaves are the tokens.
    if node.child_count() == 0 {
        if let Ok(text) = node.utf8_text(src) {
            if !text.is_empty() {
                tokens.push(Token {
                    text: text.to_string(),
                    trailing_sep: "",
                });
            }
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_leaves(child, src, tokens);
    }
}

/// Concatenate tokens, inserting a single space only where two tokens would
/// otherwise merge (word-char/word-char) or an operator would swallow a
/// leading `.`. Ported from vix `minifyTokens`.
fn render(tokens: &[Token]) -> String {
    let mut out = String::new();
    for (i, tok) in tokens.iter().enumerate() {
        if i > 0 {
            let prev = &tokens[i - 1];
            if !prev.trailing_sep.is_empty() {
                out.push_str(prev.trailing_sep);
            }
            if let Some(&last) = out.as_bytes().last() {
                let first = tok.text.as_bytes()[0];
                if last != b'\n' && is_word_char(last) && is_word_char(first) {
                    out.push(' ');
                } else if is_operator_char(last) && first == b'.' {
                    out.push(' ');
                }
            }
        }
        out.push_str(&tok.text);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_language_returns_none() {
        // Markdown/JSON/etc. have no collapse-safe grammar → caller falls back.
        assert!(language_for_ext("md").is_none());
        assert!(language_for_ext("json").is_none());
        assert!(minify("md", "# hi\n\nsome text").is_none());
    }

    #[test]
    fn whitespace_significant_langs_are_gated_out() {
        // These have grammars but are NOT collapse-safe — must stay None until
        // their annotators land (dirge-8e27), so they fall back to plain read.
        // ts/tsx: ASI; c/cpp/h/hpp: newline-terminated preprocessor.
        for ext in [
            "py", "sh", "rb", "go", "clj", "ex", "ts", "tsx", "c", "h", "cpp", "cc", "hpp",
        ] {
            assert!(
                language_for_ext(ext).is_none(),
                "{ext} must be gated out of naive minify"
            );
        }
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn rust_minify_strips_comments_and_collapses_safely() {
        let src =
            "// a comment\nfn  add ( a : i32 , b : i32 )  -> i32 {\n    // inner\n    a + b\n}\n";
        let out = minify("rs", src).expect("rust minifies");
        // Comments gone.
        assert!(!out.contains("comment"), "comments stripped: {out}");
        assert!(!out.contains("inner"));
        // Keyword/identifier boundary preserved (`fn add`, not `fnadd`).
        assert!(out.contains("fn add"), "word boundary kept: {out}");
        // Collapsed whitespace.
        assert!(!out.contains("  "), "no double spaces: {out}");
        // Still valid Rust (re-validation passed → Some was returned).
        // Sanity: re-minifying is idempotent-ish (parses again).
        assert!(minify("rs", &out).is_some(), "minified output still parses");
    }

    #[cfg(feature = "semantic-java")]
    #[test]
    fn java_minify_preserves_string_literals_and_boundaries() {
        let src = "// header\nclass A {\n    String s = \"hello  world\"; // keep spaces in string\n    int x = 1 ;\n}\n";
        let out = minify("java", src).expect("java minifies");
        assert!(!out.contains("header"), "comment stripped: {out}");
        assert!(
            !out.contains("keep spaces"),
            "trailing comment stripped: {out}"
        );
        // String literal content is a single leaf token → preserved verbatim.
        assert!(
            out.contains("\"hello  world\""),
            "string literal intact: {out}"
        );
        assert!(out.contains("class A"), "word boundary kept: {out}");
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn syntactically_broken_input_is_not_minified() {
        // Pre-existing parse error → None (fall back to plain read), never a
        // corrupted minify.
        assert!(minify("rs", "fn broken( {{{ ").is_none());
    }

    /// Proof on REAL repo files across the collapse-safe languages: each must
    /// minify (parse cleanly + re-validate → round-trip safe), shrink, and the
    /// minified output must itself re-parse. Prints the savings (run with
    /// `--nocapture`). Doubles as a real-code regression guard.
    #[test]
    fn minifies_real_repo_files() {
        let root = env!("CARGO_MANIFEST_DIR");
        let mut cases: Vec<(&str, &str)> = Vec::new();
        #[cfg(feature = "semantic-rust")]
        {
            cases.push(("src/agent/agent_loop/run.rs", "rs"));
            cases.push(("src/agent/tools/cache.rs", "rs"));
            cases.push(("src/semantic/minify.rs", "rs"));
        }

        assert!(!cases.is_empty(), "no collapse-safe grammar compiled in");
        let mut total_in = 0usize;
        let mut total_out = 0usize;
        for (rel, ext) in cases {
            let path = std::path::Path::new(root).join(rel);
            let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
            let min = minify(ext, &src)
                .unwrap_or_else(|| panic!("{rel} should minify (clean parse + revalidate)"));
            assert!(
                min.len() < src.len(),
                "{rel}: minified ({}) not smaller than source ({})",
                min.len(),
                src.len()
            );
            // Round-trip: the minified output must itself re-parse cleanly.
            assert!(
                minify(ext, &min).is_some(),
                "{rel}: minified output must re-parse"
            );
            let pct = 100.0 * (1.0 - min.len() as f64 / src.len() as f64);
            eprintln!(
                "minify {rel:50} {:>7} -> {:>7} bytes  ({pct:4.1}% saved)",
                src.len(),
                min.len()
            );
            total_in += src.len();
            total_out += min.len();
        }
        let pct = 100.0 * (1.0 - total_out as f64 / total_in.max(1) as f64);
        eprintln!(
            "minify {:50} {:>7} -> {:>7} bytes  ({pct:4.1}% saved)",
            "TOTAL", total_in, total_out
        );
    }
}
