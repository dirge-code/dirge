//! Tree-sitter syntax validation for content that's about to be
//! written to disk. Phase 2 of `docs/AGENTIC_LOOP_PLAN.md`: catch
//! the LLM writing syntactically-broken code BEFORE the bytes
//! land in the filesystem, so the model sees the error in the
//! same turn and can self-correct (instead of writing broken code
//! and discovering it via `cargo check` two turns later).
//!
//! Called from `write::call`, `edit::call`, `apply_patch::call`.
//! Default-on when a tree-sitter language is registered for the
//! file's extension; default-off (returns no errors) otherwise.
//!
//! Per-feature gating: each language requires its corresponding
//! `semantic-<lang>` Cargo feature to compile in. Without any
//! feature, this module is a no-op stub.
//!
//! Error budget: capped at `MAX_ERRORS` per call so a totally-
//! broken file doesn't dump 1000 errors into the tool result.

use std::path::Path;

/// One syntax error discovered by tree-sitter. Carries enough
/// detail for the model to localize the fix without re-reading
/// the file.
#[derive(Debug, Clone)]
pub struct SyntaxError {
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number.
    pub column: usize,
    /// Short snippet of the problematic source range (≤ 80 chars
    /// or one line, whichever is shorter).
    pub snippet: String,
    /// Whether tree-sitter classified this as an ERROR node (true
    /// syntax error) or a MISSING node (tree-sitter inferred a
    /// missing token like `;`).
    pub is_missing: bool,
    /// For MISSING nodes, the token tree-sitter expected — its node
    /// kind, e.g. `"}"`, `")"`, `";"`. This is computed by the grammar,
    /// so it's accurate for every language. `None` for ERROR nodes.
    pub expected: Option<String>,
}

impl SyntaxError {
    /// Format for inclusion in a tool-error message. Names the expected
    /// token for MISSING nodes ("missing `}`") so the feedback is
    /// actionable across every tree-sitter language, not just Lisp.
    pub fn render(&self) -> String {
        match (self.is_missing, self.expected.as_deref()) {
            (true, Some(tok)) if !tok.is_empty() && tok != "ERROR" => format!(
                "  missing `{}` at {}:{}: {}",
                tok, self.line, self.column, self.snippet
            ),
            (true, _) => format!(
                "  missing token at {}:{}: {}",
                self.line, self.column, self.snippet
            ),
            (false, _) => format!(
                "  syntax error at {}:{}: {}",
                self.line, self.column, self.snippet
            ),
        }
    }
}

/// Cap on the number of errors surfaced per call. Tree-sitter can
/// cascade — one missing brace produces dozens of downstream
/// ERROR nodes — so a flat truncation keeps the tool result
/// readable.
const MAX_ERRORS: usize = 10;

/// Resolve the file extension to a tree-sitter Language. Returns
/// `None` for files we don't know how to parse, OR when the
/// matching `semantic-<lang>` feature isn't compiled in. The
/// caller should treat `None` as "skip validation" (silent
/// fall-through), not "error".
fn language_for_path(path: &Path) -> Option<tree_sitter::Language> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        #[cfg(feature = "semantic-rust")]
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),

        #[cfg(feature = "semantic-ts")]
        "ts" | "tsx" | "mts" | "cts" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),

        #[cfg(feature = "semantic-ts")]
        "js" | "jsx" | "mjs" | "cjs" => {
            // TSX grammar handles JSX too; close enough for syntax
            // validation. The semantic extractor uses a separate
            // JS adapter; we accept slightly higher false-negative
            // rate here in exchange for not pulling in a second
            // grammar crate just for syntax checking.
            Some(tree_sitter_typescript::LANGUAGE_TSX.into())
        }

        #[cfg(feature = "semantic-python")]
        "py" | "pyi" => Some(tree_sitter_python::LANGUAGE.into()),

        #[cfg(feature = "semantic-go")]
        "go" => Some(tree_sitter_go::LANGUAGE.into()),

        #[cfg(feature = "semantic-ruby")]
        "rb" | "rake" | "gemspec" => Some(tree_sitter_ruby::LANGUAGE.into()),

        #[cfg(feature = "semantic-java")]
        "java" => Some(tree_sitter_java::LANGUAGE.into()),

        #[cfg(feature = "semantic-c")]
        "c" => Some(tree_sitter_c::LANGUAGE.into()),

        #[cfg(feature = "semantic-cpp")]
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(tree_sitter_cpp::LANGUAGE.into()),

        #[cfg(feature = "semantic-clojure")]
        "clj" | "cljs" | "cljc" | "edn" | "bb" => Some(tree_sitter_clojure::LANGUAGE.into()),

        #[cfg(feature = "semantic-bash")]
        "sh" | "bash" => Some(tree_sitter_bash::LANGUAGE.into()),

        // NOTE: `.sql` is deliberately NOT registered here. This gate is a
        // write-time HARD block (`syntax_gate` → tool error on parse error),
        // and the generic tree-sitter-sequel grammar reports mainstream,
        // perfectly valid SQL as ERROR nodes — `CREATE PROCEDURE` (Postgres
        // 11+/MySQL) and the entire T-SQL dialect (`DECLARE @x`, `GO`) among
        // them. Blocking those writes leaves the agent unable to save valid
        // SQL with no recourse. SQL still gets semantic indexing via
        // `SqlAdapter`, which tolerates parse errors (emits a warning, indexes
        // what it can) rather than blocking. (dirge follow-up to #516)
        _ => None,
    }
}

/// Walk the syntax tree and collect ERROR / MISSING nodes. Capped
/// at `MAX_ERRORS`. Each error includes line:col plus a short
/// source snippet so the model can localize without re-reading.
fn collect_errors(tree: &tree_sitter::Tree, source: &str) -> Vec<SyntaxError> {
    let mut errors: Vec<SyntaxError> = Vec::new();
    let cursor = tree.walk();
    let mut stack: Vec<tree_sitter::Node> = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if errors.len() >= MAX_ERRORS {
            break;
        }
        if node.is_error() || node.is_missing() {
            let start = node.start_position();
            let snippet = snippet_for(node, source);
            // For a MISSING node, `kind()` is the token the grammar
            // expected (e.g. "}") — the most actionable detail we have.
            let expected = if node.is_missing() {
                Some(node.kind().to_string())
            } else {
                None
            };
            errors.push(SyntaxError {
                line: start.row + 1,
                column: start.column + 1,
                snippet,
                is_missing: node.is_missing(),
                expected,
            });
            // Skip walking deeper inside an error node — the
            // children are noise once the parent is known to be
            // broken.
            continue;
        }
        let _ = cursor; // silence unused-variable when the loop walks via `node.child()`
        // Push children in reverse so the walk is left-to-right.
        for i in (0..node.child_count()).rev() {
            if let Some(child) = node.child(i) {
                stack.push(child);
            }
        }
    }
    errors
}

/// Best-effort short snippet for an error node. Returns the
/// node's source text trimmed to ≤ 80 chars on one line. Falls
/// back to the line containing the error when the node spans
/// multiple lines.
fn snippet_for(node: tree_sitter::Node, source: &str) -> String {
    let start = node.start_byte();
    let end = node.end_byte().min(source.len());
    if start >= end {
        // Missing nodes have zero byte span; pull the line they
        // sit on so the model can see context.
        let line_start = source[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = source[start..]
            .find('\n')
            .map(|i| start + i)
            .unwrap_or(source.len());
        return source[line_start..line_end]
            .chars()
            .take(80)
            .collect::<String>()
            .trim_end()
            .to_string();
    }
    let raw = &source[start..end];
    let line: String = raw.chars().take_while(|c| *c != '\n').collect();
    line.chars()
        .take(80)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Validate `content` against the tree-sitter grammar registered
/// for `path`'s extension. Returns `Ok(())` for clean parses, for
/// unknown extensions, and for any environment where the matching
/// `semantic-<lang>` feature isn't built. Returns `Err(Vec<...>)`
/// only when the grammar is available AND found real errors.
///
/// Designed as a CHEAP pre-write check — typical execution time
/// for a 10 KiB Rust file is <2ms on modern hardware. The call
/// site decides whether to surface the errors as a tool failure
/// (the safest default for `write` / `edit` / `apply_patch`).
pub fn check_syntax(path: &Path, content: &str) -> Result<(), Vec<SyntaxError>> {
    let Some(lang) = language_for_path(path) else {
        // No tree-sitter grammar for this extension. For languages we have
        // delimiter-lexing rules for (lisps without a grammar — .janet,
        // .fnl, .lisp, .scm, .rkt, .el, .cljd, .jdn), fall back to the
        // delimiter-balance scanner so the model still gets the actionable
        // "add N matching `)`" feedback instead of silence. Silence is what
        // pushes the model into counting delimiters by hand (dirge-gwpi).
        // For everything else (no grammar AND no lex rules) this is a no-op.
        return match lex_rules_for_path(path) {
            Some(rules) if delimiter_summary(content, rules).is_some() => {
                // Sentinel error: the actionable message is produced by
                // `format_errors` (which appends the delimiter summary). It
                // carries no line/col of its own — `format_errors` does not
                // render sentinels for the no-grammar path.
                Err(vec![SyntaxError {
                    line: 0,
                    column: 0,
                    snippet: String::new(),
                    is_missing: true,
                    expected: None,
                }])
            }
            _ => Ok(()),
        };
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang).is_err() {
        // Grammar version mismatch — skip rather than block the
        // write. Validation is best-effort.
        return Ok(());
    }
    let Some(tree) = parser.parse(content, None) else {
        return Ok(());
    };
    if !tree.root_node().has_error() {
        return Ok(());
    }
    let errors = collect_errors(&tree, content);
    if errors.is_empty() {
        // has_error() returned true but the walk didn't find any
        // — shouldn't happen but defensive.
        return Ok(());
    }
    Err(errors)
}

/// Per-language lexing rules for the delimiter-balance scan — just
/// enough to skip comments / strings / char-literals so the real
/// `()[]{}` are counted correctly.
///
/// Configured only for languages whose comment + string forms are
/// unambiguous to a simple scanner. JS/TS, Ruby, and Bash are
/// deliberately omitted: regex literals (`/…/`) and heredocs can fool a
/// non-parsing scanner into a *wrong* delimiter, and a misleading hint is
/// worse than none — those languages still get the base tree-sitter error
/// plus the named missing token. The scan only localizes an imbalance
/// that tree-sitter already flagged.
struct LexRules {
    line_comments: &'static [&'static str],
    /// `(open, close)` block-comment pairs.
    block_comments: &'static [(&'static str, &'static str)],
    nested_block_comments: bool,
    /// `(open, close, supports_backslash_escape)`. Checked in order, so
    /// longer delimiters (e.g. `"""`) must precede their prefixes (`"`).
    strings: &'static [(&'static str, &'static str, bool)],
    /// `'x'` single-char literals (C/C++/Rust/Go/Java). Rust lifetimes
    /// (`'a`) are detected and treated as ordinary tokens.
    char_squote: bool,
    /// `\x` char literals (Lisp): the char after a backslash is escaped.
    char_backslash: bool,
    /// `?x` / `?\x` char literals (Emacs Lisp): `?(` is the character `(`,
    /// not an opening delimiter.
    char_question: bool,
    /// Backtick long-strings (Janet): `` `...` ``, `` ``...`` `` — delimited
    /// by a run of N backticks, closed by the next run of N backticks. Raw
    /// (no escapes inside).
    long_string_backtick: bool,
}

const RULES_C: LexRules = LexRules {
    line_comments: &["//"],
    block_comments: &[("/*", "*/")],
    nested_block_comments: false,
    strings: &[("\"", "\"", true)],
    char_squote: true,
    char_backslash: false,
    char_question: false,
    long_string_backtick: false,
};
const RULES_RUST: LexRules = LexRules {
    line_comments: &["//"],
    block_comments: &[("/*", "*/")],
    nested_block_comments: true,
    // raw strings r#"…"# and r"…" precede the plain "…".
    strings: &[
        ("r#\"", "\"#", false),
        ("r\"", "\"", false),
        ("\"", "\"", true),
    ],
    char_squote: true,
    char_backslash: false,
    char_question: false,
    long_string_backtick: false,
};
const RULES_GO: LexRules = LexRules {
    line_comments: &["//"],
    block_comments: &[("/*", "*/")],
    nested_block_comments: false,
    strings: &[("`", "`", false), ("\"", "\"", true)],
    char_squote: true,
    char_backslash: false,
    char_question: false,
    long_string_backtick: false,
};
const RULES_JAVA: LexRules = LexRules {
    line_comments: &["//"],
    block_comments: &[("/*", "*/")],
    nested_block_comments: false,
    strings: &[("\"\"\"", "\"\"\"", true), ("\"", "\"", true)],
    char_squote: true,
    char_backslash: false,
    char_question: false,
    long_string_backtick: false,
};
const RULES_PYTHON: LexRules = LexRules {
    line_comments: &["#"],
    block_comments: &[],
    nested_block_comments: false,
    strings: &[
        ("\"\"\"", "\"\"\"", true),
        ("'''", "'''", true),
        ("\"", "\"", true),
        ("'", "'", true),
    ],
    char_squote: false,
    char_backslash: false,
    char_question: false,
    long_string_backtick: false,
};
// Clojure family + Fennel: `;` line comments, `"` strings, `\x` char
// literals (`\(`, `\space`). No block comments, no `?`/backtick forms.
const RULES_LISP: LexRules = LexRules {
    line_comments: &[";"],
    block_comments: &[],
    nested_block_comments: false,
    strings: &[("\"", "\"", true)],
    char_squote: false,
    char_backslash: true,
    char_question: false,
    long_string_backtick: false,
};
// Janet / JDN: `#` line comments (NOT `;`), `"` strings with `\` escapes,
// and backtick long-strings (`` `...` ``).
const RULES_JANET: LexRules = LexRules {
    line_comments: &["#"],
    block_comments: &[],
    nested_block_comments: false,
    strings: &[("\"", "\"", true)],
    char_squote: false,
    char_backslash: false,
    char_question: false,
    long_string_backtick: true,
};
// Scheme / Racket / Common Lisp: `;` line comments, nestable `#| ... |#`
// block comments, `"` strings, `\x` char escapes (`#\(`).
const RULES_SCHEME: LexRules = LexRules {
    line_comments: &[";"],
    block_comments: &[("#|", "|#")],
    nested_block_comments: true,
    strings: &[("\"", "\"", true)],
    char_squote: false,
    char_backslash: true,
    char_question: false,
    long_string_backtick: false,
};
// Emacs Lisp: `;` line comments, `"` strings, and `?x` / `?\x` char
// literals (`?(` is the character `(`, not an opener).
const RULES_ELISP: LexRules = LexRules {
    line_comments: &[";"],
    block_comments: &[],
    nested_block_comments: false,
    strings: &[("\"", "\"", true)],
    char_squote: false,
    char_backslash: true,
    char_question: true,
    long_string_backtick: false,
};

/// Lexing rules for a path's extension, or `None` when the delimiter
/// scan isn't trustworthy for that language (or it's unknown).
fn lex_rules_for_path(path: &Path) -> Option<&'static LexRules> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    Some(match ext.as_str() {
        "rs" => &RULES_RUST,
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => &RULES_C,
        "go" => &RULES_GO,
        "java" => &RULES_JAVA,
        "py" | "pyi" => &RULES_PYTHON,
        // Clojure family + Fennel.
        "clj" | "cljs" | "cljc" | "cljd" | "edn" | "bb" | "fnl" => &RULES_LISP,
        // Janet + Janet Data Notation (`#` comments, backtick long-strings).
        "janet" | "jdn" => &RULES_JANET,
        // Scheme / Racket / Common Lisp (`#| |#` block comments).
        "scm" | "ss" | "rkt" | "lisp" | "lsp" | "cl" => &RULES_SCHEME,
        // Emacs Lisp (`?x` char literals).
        "el" => &RULES_ELISP,
        _ => return None,
    })
}

/// `true` when the bytes at `i` begin with `p`.
fn starts_at(b: &[u8], i: usize, p: &str) -> bool {
    b[i..].starts_with(p.as_bytes())
}

/// Advance `*i` by `count` bytes, updating `line`/`col` over the skipped
/// bytes. Relative (not absolute) so call sites never read `i` while it's
/// mutably borrowed in the same call.
fn adv(b: &[u8], i: &mut usize, line: &mut usize, col: &mut usize, count: usize) {
    let to = i.saturating_add(count).min(b.len());
    while *i < to {
        if b[*i] == b'\n' {
            *line += 1;
            *col = 1;
        } else {
            *col += 1;
        }
        *i += 1;
    }
}

/// Structured result of the comment/string/char-aware delimiter scan.
/// Both the human summary ([`delimiter_summary`]) and the mechanical
/// repair ([`repair_delimiters`]) read from this, so they share one
/// understanding of what is and isn't a delimiter (dirge-p5fu).
enum DelimiterBalance {
    /// Every opener has its matching closer.
    Balanced,
    /// Openers left on the stack at EOF (bottom→top): `(open, line, col)`.
    /// Safely closable by appending the matching closers in reverse.
    Unclosed(Vec<(u8, usize, usize)>),
    /// A closer with no/mismatched opener at `(closer, line, col)`. NOT
    /// safely closable — appending can't fix a misplaced closer.
    Stray(char, usize, usize),
}

/// Scan source for a delimiter imbalance under `rules`. Comment/string/
/// char-literal aware so the real `()[]{}` are counted correctly. All
/// comment/string/delimiter syntax is ASCII, so a byte scan is safe.
fn delimiter_balance(content: &str, rules: &LexRules) -> DelimiterBalance {
    let b = content.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    let (mut line, mut col) = (1usize, 1usize);
    let mut stack: Vec<(u8, usize, usize)> = Vec::new(); // (open, line, col)

    'outer: while i < n {
        for lc in rules.line_comments {
            if starts_at(b, i, lc) {
                let to_eol = b[i..].iter().position(|&c| c == b'\n').unwrap_or(n - i);
                adv(b, &mut i, &mut line, &mut col, to_eol);
                continue 'outer;
            }
        }
        for (open, close) in rules.block_comments {
            if starts_at(b, i, open) {
                adv(b, &mut i, &mut line, &mut col, open.len());
                let mut depth = 1usize;
                while i < n && depth > 0 {
                    if rules.nested_block_comments && starts_at(b, i, open) {
                        depth += 1;
                        adv(b, &mut i, &mut line, &mut col, open.len());
                    } else if starts_at(b, i, close) {
                        depth -= 1;
                        adv(b, &mut i, &mut line, &mut col, close.len());
                    } else {
                        adv(b, &mut i, &mut line, &mut col, 1);
                    }
                }
                continue 'outer;
            }
        }
        for (open, close, esc) in rules.strings {
            if starts_at(b, i, open) {
                adv(b, &mut i, &mut line, &mut col, open.len());
                while i < n {
                    if *esc && b[i] == b'\\' {
                        adv(b, &mut i, &mut line, &mut col, 2);
                    } else if starts_at(b, i, close) {
                        adv(b, &mut i, &mut line, &mut col, close.len());
                        break;
                    } else {
                        adv(b, &mut i, &mut line, &mut col, 1);
                    }
                }
                continue 'outer;
            }
        }
        if rules.char_backslash && b[i] == b'\\' {
            adv(b, &mut i, &mut line, &mut col, 2);
            continue 'outer;
        }
        if rules.char_squote && b[i] == b'\'' {
            if b.get(i + 1) == Some(&b'\\') {
                // '\…': skip to the closing quote, honoring escapes.
                let mut j = i + 1;
                while j < n {
                    if b[j] == b'\\' {
                        j += 2;
                    } else if b[j] == b'\'' {
                        j += 1;
                        break;
                    } else {
                        j += 1;
                    }
                }
                let count = j - i;
                adv(b, &mut i, &mut line, &mut col, count);
                continue 'outer;
            } else if b.get(i + 2) == Some(&b'\'') {
                adv(b, &mut i, &mut line, &mut col, 3); // 'x'
                continue 'outer;
            } else {
                // Rust lifetime ('a) or stray quote — an ordinary token.
                adv(b, &mut i, &mut line, &mut col, 1);
                continue 'outer;
            }
        }
        if rules.char_question && b[i] == b'?' {
            // Emacs Lisp char literal: `?X` or `?\X` (e.g. `?(`, `?\(`).
            // The character — including a delimiter — is data, not an opener.
            if b.get(i + 1) == Some(&b'\\') {
                adv(b, &mut i, &mut line, &mut col, 3); // ?\X
            } else if i + 1 < n {
                adv(b, &mut i, &mut line, &mut col, 2); // ?X
            } else {
                adv(b, &mut i, &mut line, &mut col, 1);
            }
            continue 'outer;
        }
        if rules.long_string_backtick && b[i] == b'`' {
            // Janet long-string: a run of k backticks opens it; the next run
            // of ≥ k backticks closes it. Raw — no escapes inside.
            let mut k = 0usize;
            while i + k < n && b[i + k] == b'`' {
                k += 1;
            }
            adv(b, &mut i, &mut line, &mut col, k);
            while i < n {
                if b[i] == b'`' {
                    let mut j = 0usize;
                    while i + j < n && b[i + j] == b'`' {
                        j += 1;
                    }
                    if j >= k {
                        adv(b, &mut i, &mut line, &mut col, k);
                        break;
                    }
                    adv(b, &mut i, &mut line, &mut col, j);
                } else {
                    adv(b, &mut i, &mut line, &mut col, 1);
                }
            }
            continue 'outer;
        }
        match b[i] {
            b'(' | b'[' | b'{' => stack.push((b[i], line, col)),
            b')' | b']' | b'}' => {
                let want = match b[i] {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                match stack.last() {
                    Some(&(open, _, _)) if open == want => {
                        stack.pop();
                    }
                    _ => {
                        return DelimiterBalance::Stray(b[i] as char, line, col);
                    }
                }
            }
            _ => {}
        }
        adv(b, &mut i, &mut line, &mut col, 1);
    }

    if stack.is_empty() {
        DelimiterBalance::Balanced
    } else {
        DelimiterBalance::Unclosed(stack)
    }
}

/// Closing delimiter for an opener byte.
fn closer_for(open: u8) -> char {
    match open {
        b'(' => ')',
        b'[' => ']',
        _ => '}',
    }
}

/// Concrete, actionable summary of a delimiter imbalance — so the model
/// never has to count by hand. `None` when delimiters balance (the real
/// error is elsewhere). Wording is load-bearing (tests pin it).
fn delimiter_summary(content: &str, rules: &LexRules) -> Option<String> {
    match delimiter_balance(content, rules) {
        DelimiterBalance::Balanced => None,
        DelimiterBalance::Stray(c, line, col) => Some(format!(
            "Delimiter imbalance: unexpected `{c}` at line {line}, col {col} \
             with no matching open — remove an extra closer, or add the missing \
             opener before it."
        )),
        DelimiterBalance::Unclosed(stack) => {
            let (open, l, c) = stack[0];
            let openc = open as char;
            let close = closer_for(open);
            Some(format!(
                "Delimiter imbalance: {n} unclosed — the `{openc}` opened at line {l}, col {c} is \
                 never closed; add {n} matching `{close}` (do not count by hand — fix this delimiter).",
                n = stack.len()
            ))
        }
    }
}

/// Most non-blank lines allowed AFTER the outermost unclosed opener for the
/// imbalance to count as a trailing truncation. A genuine "forgot the closing
/// brace(s) at the end" leaves little content past the last open block; a
/// mid-file stray opener (the nested-fn corruption) leaves the whole rest of
/// the file after it. Conservative on purpose — when there's no language
/// server to verify the repair (see the LSP path), erring toward reject just
/// bounces the model's own text back for it to fix (dirge-a0nl).
const MAX_TRAILING_NONBLANK_LINES: usize = 10;

/// True when a delimiter imbalance looks like an END-OF-CONTENT truncation
/// (the model finished the logical content but dropped the trailing closers)
/// rather than a MID-FILE structural error. Closing a trailing truncation
/// appends to existing code; closing a mid-file stray opener would WRAP the
/// complete code after it into the wrongly-opened block — that's the
/// nested-`fn` corruption (Richie's report), structurally valid yet wrong, so
/// we reject it and let the model fix its own edit.
fn is_trailing_truncation(content: &str, stack: &[(u8, usize, usize)]) -> bool {
    let Some(&(_, open_line, _)) = stack.first() else {
        return false;
    };
    // `open_line` is 1-based; skipping that many lines drops everything up to
    // and including the opener's line, leaving only what follows it.
    let trailing_nonblank = content
        .lines()
        .skip(open_line)
        .filter(|l| !l.trim().is_empty())
        .count();
    trailing_nonblank <= MAX_TRAILING_NONBLANK_LINES
}

/// Mechanically close a delimiter imbalance that looks like a TRAILING
/// TRUNCATION — the model finished the logical content but dropped the closing
/// delimiters at the very end.
///
/// Safety (dirge-p5fu, hardened dirge-a0nl):
/// - Only the pure-unclosed case is touched; a stray/mismatched closer is left
///   for the model (the precise summary explains it).
/// - Only a trailing truncation qualifies ([`is_trailing_truncation`]). A
///   MID-FILE stray opener — with complete code after it — is rejected: closing
///   it would WRAP that code into the wrongly-opened block (the nested-`fn`
///   corruption), a structurally-valid-but-wrong result tree-sitter can't flag.
///   That's the structural guard that re-validation alone cannot give.
/// - For a trailing truncation the missing closers belong at EOF by
///   definition, so they're appended there (innermost first). Tree-sitter does
///   NOT reliably emit MISSING-`}` nodes for an unclosed block at EOF — it
///   marks the region ERROR — so MISSING-node-driven placement only matters for
///   mid-file closers, which this path rejects; revisit it when the LSP-backed
///   verifier lets us repair mid-file safely.
/// - The result is RE-VALIDATED with [`check_syntax`]; returned only if clean.
pub fn repair_delimiters(path: &Path, content: &str) -> Option<(String, String)> {
    let rules = lex_rules_for_path(path)?;
    let DelimiterBalance::Unclosed(stack) = delimiter_balance(content, rules) else {
        return None;
    };
    if !is_trailing_truncation(content, &stack) {
        return None;
    }
    // Append closers top-of-stack first (innermost closes first) — correct
    // placement for a trailing truncation.
    let closers: String = stack
        .iter()
        .rev()
        .map(|&(open, _, _)| closer_for(open))
        .collect();
    let repaired = format!("{content}{closers}");
    if check_syntax(path, &repaired).is_err() {
        return None;
    }
    let (open, l, c) = stack[0];
    let note = format!(
        "auto-closed {n} unclosed delimiter(s) at a trailing truncation: appended `{closers}` to \
         balance the `{openc}` opened at line {l}, col {c}. If that placement is wrong, resend the \
         corrected text.",
        n = stack.len(),
        openc = open as char,
    );
    Some((repaired, note))
}

/// Validate `content`; on a delimiter imbalance, try a mechanical close
/// before giving up. The single entry point the edit tools call.
pub enum SyntaxOutcome {
    /// Clean as written.
    Clean,
    /// Was unbalanced but mechanically closed. `content` is the text to
    /// write; `note` describes the fix (surface it on the success result).
    Repaired { content: String, note: String },
    /// Unrepairable. `message` is the formatted reject (file NOT written).
    Rejected { message: String },
}

/// Validate, and auto-repair a closable delimiter imbalance if possible.
/// Parity with the JSON truncation repair (dirge-p5fu).
pub fn validate_or_repair(path: &Path, content: &str) -> SyntaxOutcome {
    match check_syntax(path, content) {
        Ok(()) => SyntaxOutcome::Clean,
        Err(errors) => match repair_delimiters(path, content) {
            Some((repaired, note)) => SyntaxOutcome::Repaired {
                content: repaired,
                note,
            },
            None => SyntaxOutcome::Rejected {
                message: format_errors(path, content, &errors),
            },
        },
    }
}

/// Convenience wrapper: format a `Vec<SyntaxError>` as a single
/// multi-line string suitable for inclusion in a tool error message. For
/// languages with reliable lexing, a delimiter-balance summary is
/// appended so the model gets an actionable "the `{` at line N is never
/// closed" instead of a bare line:col.
pub fn format_errors(path: &Path, content: &str, errors: &[SyntaxError]) -> String {
    // When a tree-sitter grammar exists, the errors come from it (and the
    // delimiter summary localizes them). For grammarless languages the
    // errors are sentinels from the delimiter-balance fallback — the summary
    // below IS the message, so don't claim tree-sitter and don't render the
    // empty sentinels. (dirge-gwpi)
    let has_grammar = language_for_path(path).is_some();
    let mut out = if has_grammar {
        format!(
            "Syntax check failed for {}: {} error(s) detected by tree-sitter. \
             Fix and re-submit. (This is a pre-write guard — the file was NOT modified.)\n",
            path.display(),
            errors.len(),
        )
    } else {
        format!(
            "Syntax check failed for {}: delimiters are unbalanced. \
             Fix and re-submit. (This is a pre-write guard — the file was NOT modified.)\n",
            path.display(),
        )
    };
    if has_grammar {
        for err in errors {
            out.push_str(&err.render());
            out.push('\n');
        }
        if errors.len() == MAX_ERRORS {
            out.push_str(&format!(
                "  …(truncated at {} errors; fix the listed issues and re-check)\n",
                MAX_ERRORS,
            ));
        }
    }
    if let Some(rules) = lex_rules_for_path(path)
        && let Some(summary) = delimiter_summary(content, rules)
    {
        out.push_str("  ");
        out.push_str(&summary);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---- Generalized delimiter scanner (no tree-sitter needed) ----

    #[test]
    fn render_names_the_expected_missing_token() {
        let e = SyntaxError {
            line: 5,
            column: 1,
            snippet: "}".into(),
            is_missing: true,
            expected: Some("}".into()),
        };
        assert_eq!(e.render(), "  missing `}` at 5:1: }");
        // ERROR node (not missing) keeps the generic phrasing.
        let e2 = SyntaxError {
            line: 1,
            column: 1,
            snippet: "@@@".into(),
            is_missing: false,
            expected: None,
        };
        assert!(e2.render().contains("syntax error at 1:1"));
    }

    /// The scanner must NOT cry imbalance on VALID code — each language's
    /// strings / comments / char-literals / raw-strings / lifetimes hide
    /// delimiters that would otherwise be miscounted. This is the safety
    /// property: a false hint is worse than none.
    #[test]
    fn balanced_code_yields_no_summary_per_language() {
        let rust = r####"
fn demo<'a>(x: &'a str) -> char {
    // a closing brace } in a line comment
    let s = "string with ( and { and }";
    let r = r#"raw with ) and ] and "#;
    let c = '}';
    let q = '\'';
    /* block ) with nested /* { */ still ) */
    if x.len() > 0 { '(' } else { ')' }
}
"####;
        assert_eq!(delimiter_summary(rust, &RULES_RUST), None, "rust");

        let c = r#"
int main(void) {
    char b = '{';      /* a ) in a block comment */
    printf("a(b)[c]"); // a } in a line comment
    return 0;
}
"#;
        assert_eq!(delimiter_summary(c, &RULES_C), None, "c");

        let go = "func f() {\n\ts := `raw ( { string`\n\tr := '}'\n\tm := map[int]int{}\n}\n";
        assert_eq!(delimiter_summary(go, &RULES_GO), None, "go");

        let java = "class A {\n  String t = \"\"\" ( { still fine \"\"\";\n  char c = ']';\n}\n";
        assert_eq!(delimiter_summary(java, &RULES_JAVA), None, "java");

        let py = "def f(x):\n    s = \"a{b}c\"\n    t = '''( { ['''\n    # comment with )\n    return [1, 2, {3: 4}]\n";
        assert_eq!(delimiter_summary(py, &RULES_PYTHON), None, "python");

        let lisp = r#"(defn f [x] (str "a(b)c" \( \) ))"#;
        assert_eq!(delimiter_summary(lisp, &RULES_LISP), None, "lisp");
    }

    #[test]
    fn unclosed_delimiter_is_localized_per_language() {
        // Rust: unclosed `(` and `{` — points at the first unclosed.
        let s = delimiter_summary("fn f() {\n    let x = (1 + 2;\n", &RULES_RUST)
            .expect("rust imbalance");
        assert!(s.contains("unclosed"), "{s}");

        // C: extra `}`.
        let s = delimiter_summary("int f() { return 0; }}\n", &RULES_C).expect("c extra");
        assert!(
            s.contains("unexpected") && s.contains("no matching open"),
            "{s}"
        );

        // Python: unclosed `[`.
        let s = delimiter_summary("xs = [1, 2,\n", &RULES_PYTHON).expect("py imbalance");
        assert!(s.contains("unclosed"), "{s}");
    }

    // ── Mechanical delimiter repair (dirge-p5fu) ─────────────────

    #[test]
    fn repair_closes_truncated_form() {
        // .janet has no tree-sitter grammar, so check_syntax uses the
        // comment/string-aware scanner — deterministic, no feature needed.
        let path = PathBuf::from("/tmp/x.janet");
        let (repaired, note) = repair_delimiters(&path, "(defn f [x]\n  (+ x 1")
            .expect("truncated form is repairable");
        assert_eq!(repaired, "(defn f [x]\n  (+ x 1))");
        assert!(
            check_syntax(&path, &repaired).is_ok(),
            "repaired must validate"
        );
        assert!(
            note.contains("auto-closed 2"),
            "note reports the fix: {note}"
        );
    }

    #[test]
    fn repair_ignores_delimiters_in_strings_and_comments() {
        // The `)` in the string and the `)` in the `#` comment must NOT be
        // counted — only the real unclosed `(` on the last line is, so
        // exactly one `)` is appended (and lands in code, not a comment).
        let path = PathBuf::from("/tmp/x.janet");
        let src = "(def s \"a ) b\")\n# comment )\n(+ 1 2";
        let (repaired, note) = repair_delimiters(&path, src).expect("string/comment-aware repair");
        assert_eq!(repaired, "(def s \"a ) b\")\n# comment )\n(+ 1 2)");
        assert!(check_syntax(&path, &repaired).is_ok());
        assert!(note.contains("auto-closed 1"), "{note}");
    }

    #[test]
    fn repair_declines_when_closer_would_land_in_a_comment() {
        // If the content ends inside a line comment, appending the closer at
        // EOF would bury it in the comment and not actually balance — the
        // re-validation gate must catch that and decline rather than emit
        // still-broken text.
        let path = PathBuf::from("/tmp/x.janet");
        assert!(repair_delimiters(&path, "(+ 1 2 # oops").is_none());
    }

    #[test]
    fn repair_refuses_stray_closer() {
        // A stray/mismatched closer is NOT a truncation — appending can't
        // fix a misplaced closer, so repair declines and the model gets the
        // precise summary instead.
        let path = PathBuf::from("/tmp/x.janet");
        assert!(repair_delimiters(&path, "(+ 1 2))").is_none());
    }

    #[test]
    fn repair_unavailable_without_lex_rules() {
        // No comment/string-aware rules → we can't trust the delimiter count,
        // so no repair (keeps the current reject behavior).
        let path = PathBuf::from("/tmp/x.thisisntreal");
        assert!(repair_delimiters(&path, "(((").is_none());
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn repair_closes_truncated_rust_and_validates() {
        let path = PathBuf::from("/tmp/x.rs");
        let (repaired, note) = repair_delimiters(&path, "fn main() {\n    let x = 1;\n")
            .expect("truncated rust block is repairable");
        assert_eq!(repaired, "fn main() {\n    let x = 1;\n}");
        assert!(check_syntax(&path, &repaired).is_ok());
        assert!(note.contains("auto-closed 1"), "{note}");
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn repair_declines_when_close_does_not_validate() {
        // Balancing the delimiters of `fn main( {` yields `fn main( {})`,
        // which tree-sitter still rejects (a block can't sit in a param
        // list). The re-validation gate must catch that and decline, so a
        // nonsense close never lands — the model gets the summary instead.
        let path = PathBuf::from("/tmp/x.rs");
        assert!(
            repair_delimiters(&path, "fn main( {").is_none(),
            "a close that doesn't actually parse must be refused"
        );
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn repair_rejects_mid_file_stray_opener_that_would_swallow_siblings() {
        // dirge-a0nl (Richie's report): an edit injected an extra `{` high in
        // the file with complete functions after it. Appending `}` at EOF
        // *parses* — Rust allows nested fns — but wraps every following item
        // inside the stray block: silent, structurally-valid corruption that
        // tree-sitter can't flag. The trailing-truncation guard must reject it
        // so the model fixes its own edit rather than getting a mangled file.
        let path = PathBuf::from("/tmp/x.rs");
        let mut src = String::from("fn stray() {\n");
        for i in 0..12 {
            src.push_str(&format!("fn f{i}() {{ let x = 1; }}\n"));
        }
        // The danger is real: the naive close DOES parse.
        assert!(
            check_syntax(&path, &format!("{src}}}")).is_ok(),
            "precondition: appending `}}` yields valid (nested-fn) Rust",
        );
        // But repair must refuse it.
        assert!(
            repair_delimiters(&path, &src).is_none(),
            "a mid-file stray opener with complete code after it must not be auto-closed",
        );
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn validate_or_repair_paths() {
        let path = PathBuf::from("/tmp/x.rs");
        assert!(matches!(
            validate_or_repair(&path, "fn main() {}\n"),
            SyntaxOutcome::Clean
        ));
        assert!(matches!(
            validate_or_repair(&path, "fn main() {\n  let x = 1;\n"),
            SyntaxOutcome::Repaired { .. }
        ));
        // Stray closer → unrepairable → rejected with the summary.
        match validate_or_repair(&path, "fn main() {}}\n") {
            SyntaxOutcome::Rejected { message } => {
                assert!(message.contains("Syntax check failed"), "{message}")
            }
            _ => panic!("stray closer must be rejected, not repaired"),
        }
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn format_errors_appends_summary_for_rust() {
        let path = PathBuf::from("/tmp/x.rs");
        let content = "fn f() {\n    let x = (1 + 2;\n"; // unclosed ( and {
        let errors = check_syntax(&path, content).expect_err("expected errors");
        let rendered = format_errors(&path, content, &errors);
        assert!(
            rendered.contains("Delimiter imbalance"),
            "rust error should carry the balance hint now too: {rendered}"
        );
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn clean_rust_passes() {
        let path = PathBuf::from("/tmp/foo.rs");
        assert!(check_syntax(&path, "fn main() {}\n").is_ok());
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn broken_rust_returns_errors() {
        let path = PathBuf::from("/tmp/foo.rs");
        // Missing closing brace.
        let result = check_syntax(&path, "fn main() {\n  let x = 1;\n");
        let errors = result.expect_err("expected syntax errors");
        assert!(!errors.is_empty());
    }

    #[test]
    fn unknown_extension_skips_silently() {
        let path = PathBuf::from("/tmp/foo.thisisntreal");
        assert!(check_syntax(&path, "(((((").is_ok());
    }

    #[test]
    fn sql_writes_are_not_hard_blocked() {
        // dirge follow-up to #516: `.sql` is intentionally not registered in
        // the write gate. The generic SQL grammar flags valid mainstream SQL
        // (CREATE PROCEDURE, T-SQL) as errors; blocking those writes would
        // strand the agent. These must pass the gate (semantic indexing still
        // surfaces issues as non-blocking warnings).
        let path = PathBuf::from("/tmp/proc.sql");
        assert!(
            check_syntax(
                &path,
                "CREATE PROCEDURE p() LANGUAGE SQL AS $$ SELECT 1; $$;\n"
            )
            .is_ok()
        );
        assert!(check_syntax(&path, "DECLARE @x INT = 1;\nGO\nSELECT @x;\n").is_ok());
        // Even a genuinely truncated statement isn't a hard block for SQL.
        assert!(check_syntax(&path, "CREATE TABLE users (id INT\n").is_ok());
    }

    #[test]
    fn no_extension_skips_silently() {
        let path = PathBuf::from("/tmp/Makefile");
        assert!(check_syntax(&path, "all:\n\techo hello\n").is_ok());
    }

    #[cfg(feature = "semantic-python")]
    #[test]
    fn broken_python_returns_errors() {
        let path = PathBuf::from("/tmp/foo.py");
        // Unclosed paren.
        let result = check_syntax(&path, "def foo(\n");
        let errors = result.expect_err("expected syntax errors");
        assert!(!errors.is_empty());
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn format_errors_includes_path_and_count() {
        let path = PathBuf::from("/tmp/x.rs");
        let result = check_syntax(&path, "fn main( { ");
        let errors = result.expect_err("expected errors");
        let rendered = format_errors(&path, "fn main( { ", &errors);
        assert!(rendered.contains("/tmp/x.rs"));
        assert!(rendered.contains("error(s) detected"));
    }

    #[test]
    fn lisp_summary_points_at_first_unclosed_open() {
        // `(defn f [x` — one unclosed `(` and one unclosed `[`.
        let s = delimiter_summary("(defn f [x\n  (+ x 1)", &RULES_LISP).expect("imbalanced");
        assert!(s.contains("unclosed"), "{s}");
        assert!(s.contains("line 1"), "should point at the first open: {s}");
    }

    #[test]
    fn lisp_summary_flags_extra_closer() {
        let s = delimiter_summary("(+ 1 2))", &RULES_LISP).expect("extra closer");
        assert!(s.contains("unexpected"), "{s}");
        assert!(s.contains("no matching open"), "{s}");
    }

    #[test]
    fn lisp_summary_is_none_when_balanced() {
        assert!(delimiter_summary("(defn f [x] (+ x 1))", &RULES_LISP).is_none());
        // Parens inside a string and a char literal don't count toward
        // balance — the outer form here is balanced.
        assert!(delimiter_summary(r#"(str "a(b)c" \()"#, &RULES_LISP).is_none());
        // A trailing comment's parens are ignored too.
        assert!(delimiter_summary("(+ 1 2) ; ) ) )", &RULES_LISP).is_none());
    }

    #[cfg(feature = "semantic-clojure")]
    #[test]
    fn format_errors_appends_delimiter_summary_for_clojure() {
        let path = PathBuf::from("/tmp/x.cljs");
        let content = "(defn f [x] (+ x 1)"; // one unclosed `(`
        let errors = check_syntax(&path, content).expect_err("expected errors");
        let rendered = format_errors(&path, content, &errors);
        assert!(
            rendered.contains("Delimiter imbalance"),
            "Clojure error should carry the paren-balance hint: {rendered}"
        );
    }

    // ---- Grammarless-lisp fallback (dirge-gwpi) ----
    // These languages have NO tree-sitter grammar, so check_syntax must
    // fall back to the delimiter scanner and still produce the actionable
    // "do not count by hand" message — otherwise the model writes a broken
    // file with zero feedback and resorts to counting parens itself.

    #[test]
    fn janet_unbalanced_flags_and_advises_not_to_count() {
        let path = PathBuf::from("/tmp/x.janet");
        let content = "(defn- f [x]\n  (+ x 1)\n"; // one unclosed `(`
        let errors = check_syntax(&path, content).expect_err("janet imbalance must be flagged");
        let msg = format_errors(&path, content, &errors);
        assert!(msg.contains("do not count by hand"), "{msg}");
        // No-grammar path must not falsely claim tree-sitter detected it.
        assert!(
            !msg.contains("tree-sitter"),
            "no-grammar path must not claim tree-sitter: {msg}"
        );
    }

    #[test]
    fn janet_balanced_passes() {
        let path = PathBuf::from("/tmp/x.janet");
        assert!(check_syntax(&path, "(defn- f [x] (+ x 1))\n").is_ok());
    }

    #[test]
    fn janet_hash_comment_parens_no_false_positive() {
        // Janet line comments are `#`, NOT `;`. Parens inside a `#` comment
        // must not be counted (RULES_LISP's `;` would miscount here).
        let path = PathBuf::from("/tmp/x.janet");
        let content = "# a comment with ( unbalanced paren\n(def x 1)\n";
        assert!(
            check_syntax(&path, content).is_ok(),
            "`#` comment parens must be ignored for Janet"
        );
    }

    #[test]
    fn janet_backtick_long_string_parens_no_false_positive() {
        let path = PathBuf::from("/tmp/x.janet");
        let content = "(def s `a long string with ( unbalanced paren`)\n";
        assert!(
            check_syntax(&path, content).is_ok(),
            "backtick long-string parens must be ignored for Janet"
        );
    }

    #[test]
    fn jdn_uses_janet_lexing() {
        let path = PathBuf::from("/tmp/x.jdn");
        assert!(check_syntax(&path, "# c (\n{:a 1}\n").is_ok());
    }

    #[test]
    fn fennel_unbalanced_flags() {
        let path = PathBuf::from("/tmp/x.fnl");
        let errors = check_syntax(&path, "(fn f [x]\n  (+ x 1)\n").expect_err("fennel imbalance");
        assert!(format_errors(&path, "(fn f [x]\n  (+ x 1)\n", &errors).contains("do not count"));
    }

    #[test]
    fn fennel_semicolon_comment_no_false_positive() {
        let path = PathBuf::from("/tmp/x.fnl");
        assert!(check_syntax(&path, "; comment with (\n(local x 1)\n").is_ok());
    }

    #[test]
    fn cljd_unbalanced_flags() {
        let path = PathBuf::from("/tmp/x.cljd");
        assert!(check_syntax(&path, "(defn f [x] (+ x 1)\n").is_err());
    }

    #[test]
    fn scheme_block_comment_parens_no_false_positive() {
        // Scheme/Racket use `#| ... |#` (nestable) block comments.
        let path = PathBuf::from("/tmp/x.scm");
        let content = "#| a block ( comment #| nested ) |# still |#\n(define x 1)\n";
        assert!(
            check_syntax(&path, content).is_ok(),
            "`#| |#` block-comment parens must be ignored"
        );
    }

    #[test]
    fn scheme_unbalanced_flags() {
        let path = PathBuf::from("/tmp/x.rkt");
        assert!(check_syntax(&path, "(define (f x)\n  (+ x 1)\n").is_err());
    }

    #[test]
    fn commonlisp_block_comment_no_false_positive() {
        let path = PathBuf::from("/tmp/x.lisp");
        assert!(check_syntax(&path, "#| ( |#\n(defun f () 1)\n").is_ok());
    }

    #[test]
    fn elisp_question_char_paren_no_false_positive() {
        // Elisp char literals: `?(` is the character `(`, not an opener.
        let path = PathBuf::from("/tmp/x.el");
        let content = "(setq c ?\\()\n"; // elisp `?\(` is the char `(`
        assert!(
            check_syntax(&path, content).is_ok(),
            "`?(`/`?\\(` char literals must not count as openers"
        );
    }

    #[test]
    fn elisp_unbalanced_flags() {
        let path = PathBuf::from("/tmp/x.el");
        assert!(check_syntax(&path, "(defun f ()\n  (+ 1 2)\n").is_err());
    }
}
