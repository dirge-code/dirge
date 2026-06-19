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
//! Two modes, chosen per language (`annotate`), both semantics-preserving:
//!
//! - **Aggressive collapse** — drop all whitespace, re-spacing only to keep
//!   tokens from merging. Safe only for languages with mandatory terminators,
//!   no ASI, no preprocessor, no significant whitespace: **Rust, Java**, and
//!   **Go** (via `annotate_go`, which re-inserts Go's auto-`;`).
//! - **Gap-preserving** — keep the source whitespace verbatim (collapse only
//!   blank lines), strip comments. Universally safe; used for everything where
//!   whitespace/newlines carry meaning: Bash, Python, Ruby, Elixir, C/C++,
//!   TypeScript/JS, and Clojure (whitespace-as-delimiter; collapses to a single
//!   space since its newlines are insignificant).
//!
//! [`language_for_ext`] returns a grammar only for compiled-in languages (per
//! the `semantic-*` features); any other extension → `None` → callers fall
//! back to a plain read. [`minify`] also returns `None` when the input doesn't
//! parse cleanly or the minified output fails re-validation — never a
//! half-minified / corrupted result.

//! Wired into the agent via the `read_minified` / `edit_minified` tools
//! ([`crate::agent::tools::read_minified`], [`crate::agent::tools::edit_minified`]),
//! which call [`minify`] / [`apply_minified_edit`] / [`language_for_ext`].

use tree_sitter::{Node, Parser};

/// A collected leaf token plus its source position and the separator emitted
/// after it (set by per-language annotators).
struct Token {
    text: String,
    /// 0-based source line — trigger-based annotators (Go/Ruby) use line
    /// changes to decide where an auto-semicolon must be re-inserted.
    line: usize,
    /// Source byte range — gap-preserving annotators (Bash/Clojure) inspect
    /// the bytes between adjacent tokens to know whether the source had a
    /// separator there.
    byte_start: usize,
    byte_end: usize,
    /// Separator emitted *after* this token, before the next one. May be a
    /// computed string (e.g. Bash preserves the source whitespace run).
    separator: String,
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
        // Go is safe via `annotate_go` (re-inserts auto-semicolons before the
        // collapse).
        #[cfg(feature = "semantic-go")]
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        // Gap-preserving (annotate_gap_preserve): newline/whitespace-
        // significant languages, made safe by re-emitting the source
        // whitespace between tokens (provably non-corrupting).
        #[cfg(feature = "semantic-bash")]
        "sh" | "bash" => Some(tree_sitter_bash::LANGUAGE.into()),
        #[cfg(feature = "semantic-clojure")]
        "clj" | "cljs" | "cljc" | "edn" | "bb" => Some(tree_sitter_clojure::LANGUAGE.into()),
        #[cfg(feature = "semantic-python")]
        "py" => Some(tree_sitter_python::LANGUAGE.into()),
        #[cfg(feature = "semantic-ruby")]
        "rb" | "rake" | "gemspec" => Some(tree_sitter_ruby::LANGUAGE.into()),
        #[cfg(feature = "semantic-elixir")]
        "ex" | "exs" => Some(tree_sitter_elixir::LANGUAGE.into()),
        #[cfg(feature = "semantic-c")]
        "c" | "h" => Some(tree_sitter_c::LANGUAGE.into()),
        #[cfg(feature = "semantic-cpp")]
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some(tree_sitter_cpp::LANGUAGE.into()),
        #[cfg(feature = "semantic-ts")]
        "ts" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        #[cfg(feature = "semantic-ts")]
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        _ => None,
    }
}

/// Minify `source` for the language inferred from `ext`. Returns `None` when:
/// the language isn't collapse-safe/supported, the source doesn't parse
/// cleanly (we never minify a file the grammar can't fully understand), or the
/// minified output fails re-validation. A `None` means "fall back to a plain
/// read" — never a corrupted result.
pub fn minify(ext: &str, source: &str) -> Option<String> {
    minify_with_spans(ext, source).map(|(out, _)| out)
}

/// Core minify that also returns the token→source span map (for
/// [`apply_minified_edit`]). See [`minify`] for the `None` semantics.
fn minify_with_spans(ext: &str, source: &str) -> Option<(String, Vec<Span>)> {
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
    annotate(ext, &mut tokens, src);
    let (out, spans) = render_with_spans(&tokens);
    if out.is_empty() {
        return None;
    }

    // Re-validate: the minified output must still parse without new errors.
    let reparsed = parser.parse(&out, None)?;
    if reparsed.root_node().has_error() {
        return None;
    }
    Some((out, spans))
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
                    line: node.start_position().row,
                    byte_start: node.start_byte(),
                    byte_end: node.end_byte(),
                    separator: String::new(),
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

/// Per-language separator annotation: re-insert the statement separators a
/// newline-significant language relies on, BEFORE whitespace is collapsed.
/// Languages with no annotator (Rust, Java) are left as-is. Ported from vix's
/// `annotate*` pass.
fn annotate(ext: &str, tokens: &mut [Token], source: &[u8]) {
    match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
        // Aggressive collapse is safe (Rust/Java handled by render directly).
        "go" => annotate_go(tokens),
        "clj" | "cljs" | "cljc" | "edn" | "bb" => annotate_clojure(tokens),
        // Newline/whitespace-significant languages: preserve the source
        // whitespace (keeps ASI/indentation/preprocessor structure intact),
        // collapse only blank lines, strip comments. Provably non-corrupting.
        "sh" | "bash" | "py" | "rb" | "rake" | "gemspec" | "ts" | "tsx" | "c" | "h" | "cpp"
        | "cc" | "cxx" | "hpp" | "hh" | "ex" | "exs" => annotate_gap_preserve(tokens, source),
        _ => {}
    }
}

/// Gap-preserving annotator (no silent-corruption risk): wherever the source
/// had whitespace between two tokens, re-emit it verbatim (collapsing runs of
/// blank lines to one). Keeping the source whitespace is the universally-safe
/// transform — it preserves ASI (TS/JS), indentation (Python), newline command
/// separators (Bash), and preprocessor line boundaries (C/C++) without needing
/// language-specific rules. Savings come from stripped comments + collapsed
/// blank lines. Ported from vix `annotateBash` (generalized).
fn annotate_gap_preserve(tokens: &mut [Token], source: &[u8]) {
    for i in 1..tokens.len() {
        let (start, end) = (tokens[i - 1].byte_end, tokens[i].byte_start);
        if start >= end || end > source.len() {
            continue;
        }
        // Keep only whitespace from the gap (comments were dropped already).
        let ws: Vec<u8> = source[start..end]
            .iter()
            .copied()
            .filter(|&c| c == b' ' || c == b'\t' || c == b'\n')
            .collect();
        if ws.is_empty() {
            continue;
        }
        // Collapse consecutive newlines to a single one.
        let mut collapsed = Vec::with_capacity(ws.len());
        let mut prev_nl = false;
        for &c in &ws {
            if c == b'\n' {
                if prev_nl {
                    continue;
                }
                prev_nl = true;
            } else {
                prev_nl = false;
            }
            collapsed.push(c);
        }
        tokens[i - 1].separator = String::from_utf8_lossy(&collapsed).into_owned();
    }
}

/// Gap-preserving annotator for Clojure (and the EDN/cljs/cljc/bb family).
/// Clojure is whitespace-as-delimiter: a symbol built from operator chars
/// (`->`, `<=`, `my-fn`) adjacent to another atom MUST keep its space, or two
/// atoms merge into one symbol (`-> x` => `->x`). The generic word-char
/// spacing can't guarantee that, so we re-insert a single space wherever the
/// source had any gap (whitespace or a stripped comment) between two tokens.
/// Newlines are insignificant, so a single space is enough. Provably safe:
/// never merges atoms, never adds a space the source didn't have.
fn annotate_clojure(tokens: &mut [Token]) {
    for i in 0..tokens.len().saturating_sub(1) {
        if tokens[i].byte_end < tokens[i + 1].byte_start {
            tokens[i].separator = " ".to_string();
        }
    }
}

/// Go auto-semicolon insertion: a newline after a "trigger" token (identifier,
/// literal, `)`/`]`/`}`, `++`/`--`) terminates the statement. We re-insert
/// that `;` wherever the next token is on a later line and isn't a closing
/// token — making the subsequent whitespace-collapse semantics-preserving.
/// Ported from vix `annotateGo` + `goSemicolonTrigger`/`isClosingToken`.
fn annotate_go(tokens: &mut [Token]) {
    for i in 0..tokens.len().saturating_sub(1) {
        let next_line = tokens[i + 1].line;
        let next_text = &tokens[i + 1].text;
        if next_line > tokens[i].line
            && go_semicolon_trigger(&tokens[i].text)
            && !is_closing_token(next_text)
        {
            tokens[i].separator = ";".to_string();
        }
    }
}

fn go_semicolon_trigger(text: &str) -> bool {
    let Some(&last) = text.as_bytes().last() else {
        return false;
    };
    if is_word_char(last) {
        return true;
    }
    match last {
        b')' | b']' | b'}' | b'"' | b'\'' | b'`' => true,
        b'+' => text == "++",
        b'-' => text == "--",
        _ => false,
    }
}

fn is_closing_token(text: &str) -> bool {
    matches!(text, "}" | ")" | "]" | ",")
}

/// Maps a token's range in the minified output back to its original source
/// byte range. Used by [`apply_minified_edit`] to edit the original file from
/// a match against the minified form (no formatter needed).
struct Span {
    min_start: usize,
    min_end: usize,
    orig_start: usize,
    orig_end: usize,
}

/// Like [`render`] but also records, for each token, its byte range in the
/// minified output and in the original source. Separators (annotator-inserted
/// `;`/whitespace, word-boundary spaces) belong to no token and are therefore
/// not mappable — an edit match must align to token boundaries.
///
/// No trailing `trim`: the first token is emitted at offset 0 (separators only
/// appear *between* tokens) and nothing follows the last token, so the output
/// has no leading/trailing whitespace to trim — and trimming would invalidate
/// the recorded offsets.
fn render_with_spans(tokens: &[Token]) -> (String, Vec<Span>) {
    let mut out = String::new();
    let mut spans = Vec::with_capacity(tokens.len());
    for (i, tok) in tokens.iter().enumerate() {
        if i > 0 {
            let prev = &tokens[i - 1];
            if !prev.separator.is_empty() {
                out.push_str(&prev.separator);
            }
            if let Some(&last) = out.as_bytes().last() {
                let first = tok.text.as_bytes()[0];
                // Insert a separating space when joining would fuse two
                // tokens: word-char against word-char (would merge two
                // identifiers), or an operator against a leading `.`
                // (would change `a +.b` into `a+.b`).
                let word_join = last != b'\n' && is_word_char(last) && is_word_char(first);
                let dot_join = is_operator_char(last) && first == b'.';
                if word_join || dot_join {
                    out.push(' ');
                }
            }
        }
        let min_start = out.len();
        out.push_str(&tok.text);
        spans.push(Span {
            min_start,
            min_end: out.len(),
            orig_start: tok.byte_start,
            orig_end: tok.byte_end,
        });
    }
    (out, spans)
}

/// Why a minified edit could not be applied. The tool turns these into
/// model-facing guidance.
#[derive(Debug, PartialEq, Eq)]
pub enum MinifiedEditError {
    /// The file's language isn't minifiable (use the plain `edit` tool), or it
    /// doesn't parse cleanly.
    Unsupported,
    /// `old_text` wasn't found in the minified form.
    NotFound,
    /// `old_text` matched more than once — not unique.
    NotUnique,
    /// The match doesn't start/end on token boundaries (e.g. it begins at an
    /// annotator-inserted separator or splits a token). Include more context.
    NotAligned,
}

/// Apply an edit expressed against the file's MINIFIED form to the ORIGINAL
/// source: locate `old_minified` in the minified text, map its (unique,
/// token-aligned) span back to the original byte range, and splice `new_text`
/// in there — leaving all surrounding original formatting untouched and
/// needing no formatter. The caller is responsible for syntax-checking and
/// writing the result.
pub fn apply_minified_edit(
    ext: &str,
    source: &str,
    old_minified: &str,
    new_text: &str,
) -> Result<String, MinifiedEditError> {
    if old_minified.is_empty() {
        return Err(MinifiedEditError::NotFound);
    }
    let (minified, spans) = minify_with_spans(ext, source).ok_or(MinifiedEditError::Unsupported)?;

    match minified.matches(old_minified).count() {
        0 => return Err(MinifiedEditError::NotFound),
        1 => {}
        _ => return Err(MinifiedEditError::NotUnique),
    }
    let m_start = minified.find(old_minified).unwrap();
    let m_end = m_start + old_minified.len();

    let orig_start = spans
        .iter()
        .find(|s| s.min_start == m_start)
        .ok_or(MinifiedEditError::NotAligned)?
        .orig_start;
    let orig_end = spans
        .iter()
        .find(|s| s.min_end == m_end)
        .ok_or(MinifiedEditError::NotAligned)?
        .orig_end;

    if orig_start > orig_end
        || orig_end > source.len()
        || !source.is_char_boundary(orig_start)
        || !source.is_char_boundary(orig_end)
    {
        return Err(MinifiedEditError::NotAligned);
    }

    let mut result = String::with_capacity(source.len() - (orig_end - orig_start) + new_text.len());
    result.push_str(&source[..orig_start]);
    result.push_str(new_text);
    result.push_str(&source[orig_end..]);
    Ok(result)
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
    fn non_source_extensions_are_gated_out() {
        // No tree-sitter grammar → None → caller falls back to a plain read.
        for ext in ["md", "json", "txt", "toml", "yaml", "yml", "lock", "png"] {
            assert!(
                language_for_ext(ext).is_none(),
                "{ext} has no grammar; must fall back"
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

    #[cfg(feature = "semantic-go")]
    #[test]
    fn go_minify_reinserts_auto_semicolons() {
        // Two newline-terminated statements: without the annotator, collapse
        // would merge them into invalid Go (`x:=1 y:=2`). annotate_go must
        // re-insert the `;` Go's lexer would, so the collapsed output parses.
        let src = "package main\nfunc main() {\n\tx := 1\n\ty := 2\n\t_ = x + y\n}\n";
        let out = minify("go", src).expect("go minifies via annotate_go");
        assert!(out.contains("x:=1;"), "auto-semicolon re-inserted: {out}");
        assert!(!out.contains("comment"));
        // Round-trips: the collapsed Go re-parses cleanly (minify revalidates).
        assert!(minify("go", &out).is_some(), "minified Go re-parses: {out}");
    }

    #[cfg(feature = "semantic-go")]
    #[test]
    fn go_minify_round_trips_with_comments_and_blocks() {
        let src = "package main\n// doc\nimport \"fmt\"\nfunc greet(n string) string {\n\tif n == \"\" {\n\t\treturn \"hi\"\n\t}\n\treturn fmt.Sprintf(\"hi %s\", n)\n}\n";
        let out = minify("go", src).expect("go minifies");
        assert!(!out.contains("doc"), "comment stripped: {out}");
        assert!(out.contains("func greet"), "word boundary kept: {out}");
        assert!(out.contains("\"hi %s\""), "string literal intact: {out}");
        assert!(minify("go", &out).is_some(), "re-parses: {out}");
    }

    #[test]
    fn go_semicolon_trigger_matches_spec() {
        assert!(go_semicolon_trigger("x")); // identifier
        assert!(go_semicolon_trigger("123")); // literal
        assert!(go_semicolon_trigger(")"));
        assert!(go_semicolon_trigger("}"));
        assert!(go_semicolon_trigger("++"));
        assert!(go_semicolon_trigger("--"));
        assert!(!go_semicolon_trigger("+")); // bare operator: no ASI
        assert!(!go_semicolon_trigger("{"));
        assert!(!go_semicolon_trigger(""));
    }

    #[cfg(feature = "semantic-clojure")]
    #[test]
    fn clojure_keeps_operator_symbol_boundaries() {
        // The hazard: `-> x` must NOT collapse to `->x` (which would read as a
        // single symbol). The gap-preserving annotator keeps the space.
        let src = "; a comment\n(defn f [x]\n  (-> x\n      inc\n      (+ 2)))\n";
        let out = minify("clj", src).expect("clojure minifies");
        assert!(!out.contains("comment"), "comment stripped: {out}");
        assert!(out.contains("-> x"), "operator/atom boundary kept: {out}");
        assert!(!out.contains("->x"), "must not merge symbols: {out}");
        assert!(out.contains("defn f"), "atoms stay separated: {out}");
        // Round-trips.
        assert!(
            minify("clj", &out).is_some(),
            "minified clojure re-parses: {out}"
        );
    }

    #[cfg(feature = "semantic-clojure")]
    #[test]
    fn clojure_no_space_around_delimiters() {
        let src = "(list 1 2 3)\n";
        let out = minify("clj", src).expect("minifies");
        // No spurious space after `(` or before `)` (source had none there).
        assert!(
            out.contains("(list 1 2 3)"),
            "delimiter adjacency preserved: {out}"
        );
    }

    #[cfg(feature = "semantic-bash")]
    #[test]
    fn bash_preserves_command_newlines() {
        // Bash newlines separate commands — gap preservation keeps them, so
        // `echo a` / `echo b` don't merge into one command.
        let src = "# comment\necho a\n\n\necho b\n";
        let out = minify("sh", src).expect("bash minifies");
        assert!(!out.contains("comment"), "comment stripped: {out}");
        assert!(out.contains("echo a"), "{out}");
        assert!(out.contains("echo b"), "{out}");
        // The two commands remain newline-separated (not `echo aecho b`).
        assert!(!out.contains("aecho"), "commands stay separated: {out:?}");
        assert!(minify("sh", &out).is_some(), "re-parses: {out}");
    }

    #[cfg(feature = "semantic-ts")]
    #[test]
    fn typescript_gap_preserve_keeps_asi_newlines() {
        // ASI hazard: `a = b` <nl> `(c).d()` must NOT become one statement.
        // Gap-preserve keeps the newline, so ASI still fires.
        let src = "// c\nconst a = 1\nconst b = 2\nconsole.log(a + b)\n";
        let out = minify("ts", src).expect("ts minifies (gap-preserve)");
        assert!(!out.contains("// c"), "comment stripped: {out}");
        assert!(out.contains('\n'), "newlines preserved for ASI: {out:?}");
        assert!(minify("ts", &out).is_some(), "re-parses: {out}");
    }

    #[cfg(feature = "semantic-c")]
    #[test]
    fn c_gap_preserve_keeps_preprocessor_lines() {
        // The preprocessor is newline-terminated; gap-preserve keeps those
        // newlines so `#include <stdio.h>` doesn't merge with the next line.
        let src = "#include <stdio.h>\nint main(void) {\n    return 0; // ok\n}\n";
        let out = minify("c", src).expect("c minifies (gap-preserve)");
        assert!(
            out.contains("#include <stdio.h>"),
            "preproc intact: {out:?}"
        );
        assert!(out.contains('\n'), "preproc newline kept: {out:?}");
        assert!(!out.contains("// ok"), "comment stripped: {out}");
        assert!(minify("c", &out).is_some(), "re-parses: {out}");
    }

    #[cfg(feature = "semantic-python")]
    #[test]
    fn python_gap_preserve_keeps_indentation() {
        let src = "# doc\ndef f(x):\n    if x:\n        return 1\n    return 0\n";
        let out = minify("py", src).expect("python minifies (gap-preserve)");
        assert!(!out.contains("# doc"), "comment stripped: {out}");
        // Indentation is syntax in Python — it must survive verbatim.
        assert!(
            out.contains("        return 1"),
            "indentation preserved: {out:?}"
        );
        assert!(minify("py", &out).is_some(), "re-parses: {out}");
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn syntactically_broken_input_is_not_minified() {
        // Pre-existing parse error → None (fall back to plain read), never a
        // corrupted minify.
        assert!(minify("rs", "fn broken( {{{ ").is_none());
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn minified_edit_maps_back_to_original_preserving_formatting() {
        let src = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        // Sanity on the minified shape the model would match against.
        assert_eq!(minify("rs", src).unwrap(), "fn main(){let x=1;let y=2;}");

        // Edit expressed against the minified form; only the matched original
        // span changes, surrounding indentation/newlines are untouched.
        let out = apply_minified_edit("rs", src, "let x=1", "let x = 42").unwrap();
        assert_eq!(out, "fn main() {\n    let x = 42;\n    let y = 2;\n}\n");
        // The edited file still parses.
        assert!(minify("rs", &out).is_some());
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn minified_edit_rejects_not_found_not_unique_and_misaligned() {
        let src = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        assert_eq!(
            apply_minified_edit("rs", src, "nonexistent", "x"),
            Err(MinifiedEditError::NotFound)
        );
        // "let " (with the word-boundary space) occurs twice → not unique.
        assert_eq!(
            apply_minified_edit("rs", src, "let ", "x"),
            Err(MinifiedEditError::NotUnique)
        );
        // Starts mid-token ("ain(" is inside "main(") → not token-aligned.
        assert_eq!(
            apply_minified_edit("rs", src, "ain(", "x"),
            Err(MinifiedEditError::NotAligned)
        );
        assert_eq!(
            apply_minified_edit("rs", src, "", "x"),
            Err(MinifiedEditError::NotFound)
        );
    }

    #[test]
    fn minified_edit_unsupported_language() {
        assert_eq!(
            apply_minified_edit("md", "# hi\n", "hi", "bye"),
            Err(MinifiedEditError::Unsupported)
        );
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
