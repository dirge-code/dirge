//! Central soft-wrap helper for any long text the UI prints.
//!
//! Single chokepoint so every render path (question prompts, chamber
//! rows, free-form messages) shares the same wrap policy: word-aware
//! when whitespace is available, character-fallback for unbreakable
//! runs (URLs, paths, code), display-width-aware (CJK / emoji), and
//! continuation-indent support so wrapped option text lines up under
//! its first character.
//!
//! Policy:
//!   - Width is measured with `UnicodeWidthStr` so wide glyphs count
//!     correctly; the result is suitable for fixed-column layouts
//!     like chamber rows or aligned bullet lists.
//!   - Hard newlines in the input are preserved as line breaks.
//!   - Continuation lines (every wrapped line after the first) are
//!     prefixed with `continuation_indent` so a wrapped option's
//!     extra lines visually align under the option's body.
//!   - When a single word is longer than the width budget, break it
//!     at the width boundary (display-width aware) rather than
//!     overflowing.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// ANSI-aware iterator over the PAINTING characters of `s`: yields
/// `(byte_offset, char)` for every character the terminal actually draws,
/// skipping `\x1b[…m` (and other CSI) escape sequences entirely. The byte
/// offset indexes into `s`, so a caller can slice the original string (escapes
/// riding along) around a chosen character.
///
/// dirge-g007: the single source of the SGR-skip + UTF-8 walk. `visible_width`
/// sums it and markdown's word-wrap collects it for byte-offset slicing — both
/// used to hand-roll this same loop, so the SGR-miscount fix had to be repeated
/// per site.
pub fn visible_chars(s: &str) -> VisibleChars<'_> {
    VisibleChars { s, i: 0 }
}

/// Iterator returned by [`visible_chars`]. Allocation-free so hot wrap-budget
/// paths (`visible_width` per token) don't allocate.
pub struct VisibleChars<'a> {
    s: &'a str,
    i: usize,
}

impl Iterator for VisibleChars<'_> {
    type Item = (usize, char);

    fn next(&mut self) -> Option<(usize, char)> {
        let bytes = self.s.as_bytes();
        while self.i < bytes.len() {
            // Skip a CSI escape (`ESC [` … final byte 0x40..=0x7E) as
            // zero-width — color/bold/underline state, not painted cells.
            if bytes[self.i] == 0x1b && self.i + 1 < bytes.len() && bytes[self.i + 1] == b'[' {
                let mut j = self.i + 2;
                while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                    j += 1;
                }
                self.i = j.saturating_add(1).min(bytes.len());
                continue;
            }
            let step = match bytes[self.i] {
                b if b < 0x80 => 1,
                b if b < 0xC0 => 1,
                b if b < 0xE0 => 2,
                b if b < 0xF0 => 3,
                _ => 4,
            };
            let start = self.i;
            let end = (start + step).min(bytes.len());
            self.i = end;
            if let Some(ch) = self.s[start..end].chars().next() {
                return Some((start, ch));
            }
        }
        None
    }
}

/// Display width of `s` with ANSI SGR escape sequences treated as
/// zero-width. The terminal interprets `\x1b[…m` as state changes
/// (color / bold / underline) without emitting visible characters,
/// so they must not count toward wrap budgets or chamber padding.
/// `UnicodeWidthStr::width` alone counts `[31m` as 4 visible cells
/// because the ESC byte gets width 0 but the bracketed payload is
/// ordinary ASCII — that miscount caused chamber rows with embedded
/// SGR (diff backgrounds, syntax highlighting) to wrap or pad wrong.
pub fn visible_width(s: &str) -> usize {
    visible_chars(s)
        .map(|(_, ch)| ch.width().unwrap_or(0))
        .sum()
}

/// Soft-wrap `text` to `max_width` columns. Returns one entry per
/// visual line. Hard newlines in the input become hard line breaks
/// in the output.
///
/// `continuation_indent` is prepended to every line after the first
/// of each logical line; pass `""` for no indent. The indent's own
/// width counts against `max_width`, so the wrapping accounts for
/// it. Passing an indent wider than `max_width` is treated as no
/// indent (degenerate config).
pub fn soft_wrap(text: &str, max_width: usize, continuation_indent: &str) -> Vec<String> {
    if max_width == 0 {
        return text.lines().map(|l| l.to_string()).collect();
    }
    // Wide-glyph floor: a CJK char or emoji has display width 2.
    // With `max_width == 1`, `break_long_token` can't place such a
    // glyph anywhere without overflowing the budget — the row-full
    // flush fires but the glyph is still pushed onto the
    // freshly-emptied row, producing a width-2 row on a width-1
    // budget. Bump the effective budget to 2 so wide glyphs always
    // have a home. Callers that genuinely want width 1 lose nothing
    // — a 1-cell terminal is unusable anyway.
    let max_width = max_width.max(2);
    let cont_w = UnicodeWidthStr::width(continuation_indent);
    let effective_indent = if cont_w >= max_width {
        ""
    } else {
        continuation_indent
    };
    let cont_w = UnicodeWidthStr::width(effective_indent);

    let mut out: Vec<String> = Vec::new();
    for raw_logical in text.split('\n') {
        // Strip a trailing `\r` so CRLF tool output (Windows logs,
        // some MCP servers) doesn't leak a carriage return into the
        // line buffer. A bare `\r` mid-line interpreted by the
        // terminal redraws the row from column 0, producing visible
        // overwrite artifacts; the selection char-offset math also
        // miscounts (`\r` counts as a char but draws 0 cells).
        let logical = raw_logical.strip_suffix('\r').unwrap_or(raw_logical);
        if logical.is_empty() {
            out.push(String::new());
            continue;
        }
        wrap_logical_line(logical, max_width, effective_indent, cont_w, &mut out);
    }
    out
}

fn wrap_logical_line(
    line: &str,
    max_width: usize,
    cont_indent: &str,
    cont_w: usize,
    out: &mut Vec<String>,
) {
    // Width budget on each output row depends on whether it's the
    // first row of this logical line (no indent) or a continuation
    // (indent counted).
    let mut current = String::new();
    let mut current_w = 0usize;
    let mut is_first_row = true;

    let push_row = |out: &mut Vec<String>, current: &mut String, is_first: &mut bool| {
        if *is_first {
            out.push(std::mem::take(current));
            *is_first = false;
        } else {
            let mut s = String::with_capacity(cont_indent.len() + current.len());
            s.push_str(cont_indent);
            s.push_str(current);
            out.push(s);
            current.clear();
        }
    };

    // Tokenize on whitespace runs. Each token carries its leading
    // whitespace (if any) so we can decide whether to break on the
    // space when the token is the first of a new row.
    let tokens = tokenize(line);
    for token in tokens {
        let budget = if is_first_row {
            max_width
        } else {
            max_width.saturating_sub(cont_w)
        };

        // ANSI-aware widths: `visible_width` skips `\x1b[…m` SGR
        // sequences so a token like `\x1b[48;5;52m-text\x1b[49m`
        // contributes only its visible character cells to the wrap
        // budget. Chamber rows that embed bg-color escapes were
        // being over-counted by 15+ cells per row and falsely
        // wrapped (which broke the escape from its closer, leaving
        // literal `[48;5;52m` text visible on the next row).
        let tok_w = visible_width(token.text);
        let ws_w = visible_width(token.leading_ws);

        // Start of a new row. Preserve leading whitespace on the
        // FIRST row of a logical line (callers commonly indent their
        // text by prepending spaces — option markers, confirmation
        // lines — and dropping that indent silently changes the
        // visual layout). On CONTINUATION rows (`!is_first_row`),
        // the cont_indent already supplies the leading visual
        // indent; an additional token-level leading_ws would
        // double-indent, so drop it there.
        if current.is_empty() {
            // Carry leading whitespace only on the first row of the
            // logical line. Empty-text tokens (trailing-only or
            // all-whitespace lines) ALSO go through here — we want
            // the ws preserved so an indented blank-padded line
            // doesn't collapse to an empty row (review #8).
            if is_first_row {
                if ws_w + tok_w <= budget {
                    current.push_str(token.leading_ws);
                    current.push_str(token.text);
                    current_w = ws_w + tok_w;
                } else if tok_w <= budget {
                    // ws_w pushed token over the budget; drop ws to
                    // preserve the token rather than break-and-lose.
                    current.push_str(token.text);
                    current_w = tok_w;
                } else {
                    break_long_token(
                        token.text,
                        budget,
                        max_width.saturating_sub(cont_w).max(1),
                        &mut current,
                        &mut current_w,
                        out,
                        cont_indent,
                        &mut is_first_row,
                    );
                }
            } else if tok_w <= budget {
                current.push_str(token.text);
                current_w = tok_w;
            } else {
                break_long_token(
                    token.text,
                    budget,
                    max_width.saturating_sub(cont_w).max(1),
                    &mut current,
                    &mut current_w,
                    out,
                    cont_indent,
                    &mut is_first_row,
                );
            }
            continue;
        }

        // Does the token (with its leading whitespace) fit on the
        // current row?
        if current_w + ws_w + tok_w <= budget {
            current.push_str(token.leading_ws);
            current.push_str(token.text);
            current_w += ws_w + tok_w;
        } else {
            // Flush the row, start a new continuation row.
            push_row(out, &mut current, &mut is_first_row);
            current_w = 0;
            let new_budget = max_width.saturating_sub(cont_w).max(1);
            if tok_w <= new_budget {
                current.push_str(token.text);
                current_w = tok_w;
            } else {
                break_long_token(
                    token.text,
                    new_budget,
                    new_budget,
                    &mut current,
                    &mut current_w,
                    out,
                    cont_indent,
                    &mut is_first_row,
                );
            }
        }
    }

    if !current.is_empty() || out.is_empty() {
        push_row(out, &mut current, &mut is_first_row);
    }
}

struct Token<'a> {
    leading_ws: &'a str,
    text: &'a str,
}

fn tokenize(line: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ws_start = i;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        let ws_end = i;
        let word_start = i;
        while i < bytes.len() && bytes[i] != b' ' && bytes[i] != b'\t' {
            // Advance by char boundary; bytes might be multi-byte.
            let ch_len = utf8_char_len(bytes[i]);
            i += ch_len;
            if i > bytes.len() {
                i = bytes.len();
            }
        }
        if word_start < bytes.len() {
            tokens.push(Token {
                leading_ws: &line[ws_start..ws_end],
                text: &line[word_start..i],
            });
        } else if ws_start < ws_end {
            // Trailing whitespace (no word follows). Treat as an
            // empty-text token so the indent isn't lost on the row.
            tokens.push(Token {
                leading_ws: &line[ws_start..ws_end],
                text: "",
            });
        }
    }
    tokens
}

fn utf8_char_len(first_byte: u8) -> usize {
    if first_byte < 0xC0 {
        // ASCII (<0x80), or a stray continuation byte (shouldn't
        // appear in a valid str): advance by 1 in both cases.
        1
    } else if first_byte < 0xE0 {
        2
    } else if first_byte < 0xF0 {
        3
    } else {
        4
    }
}

/// Break a single token wider than the row budget across rows.
/// Walks chars summing display widths so a multi-cell glyph never
/// overflows. The first row uses `first_budget`, every subsequent
/// row uses `continuation_budget` (which already excludes the
/// continuation indent width).
///
/// ANSI-aware: when we see an `\x1b[…m` SGR sequence, the whole
/// sequence is appended to `current` as zero-width content. This
/// keeps escape sequences atomic — a wrap mid-escape would leave
/// the terminal with a broken open SGR and the closing payload
/// would render as literal text on the next row.
#[allow(clippy::too_many_arguments)]
fn break_long_token(
    token: &str,
    first_budget: usize,
    continuation_budget: usize,
    current: &mut String,
    current_w: &mut usize,
    out: &mut Vec<String>,
    cont_indent: &str,
    is_first_row: &mut bool,
) {
    let bytes = token.as_bytes();
    let mut remaining_budget = first_budget;
    let mut i = 0;
    while i < bytes.len() {
        // Atomic ANSI SGR copy: ESC `[` … final-byte.
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            let end = j.saturating_add(1).min(bytes.len());
            current.push_str(&token[i..end]);
            i = end;
            continue;
        }
        let ch = token[i..].chars().next().unwrap_or('\u{FFFD}');
        let cw = ch.width().unwrap_or(0);
        if cw > remaining_budget {
            if *is_first_row {
                out.push(std::mem::take(current));
                *is_first_row = false;
            } else {
                let mut s = String::with_capacity(cont_indent.len() + current.len());
                s.push_str(cont_indent);
                s.push_str(current);
                out.push(s);
                current.clear();
            }
            *current_w = 0;
            remaining_budget = continuation_budget;
        }
        current.push(ch);
        *current_w += cw;
        remaining_budget = remaining_budget.saturating_sub(cw);
        i += ch.len_utf8();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_line_returns_unchanged() {
        let out = soft_wrap("hello world", 80, "");
        assert_eq!(out, vec!["hello world"]);
    }

    /// dirge-g007: the shared visible-char walk yields the painting chars with
    /// their byte offsets and skips SGR escapes — the exact contract both
    /// `visible_width` and markdown's word-wrap depend on.
    #[test]
    fn visible_chars_yields_offsets_and_skips_sgr() {
        let s = "a\x1b[31mb\x1b[0mç"; // 'a', red 'b', reset, 2-byte 'ç'
        let got: Vec<(usize, char)> = visible_chars(s).collect();
        assert_eq!(
            got.iter().map(|&(_, c)| c).collect::<Vec<_>>(),
            vec!['a', 'b', 'ç']
        );
        // Every offset indexes the start of its char in the ORIGINAL string,
        // so a caller can slice around a wrap point with escapes riding along.
        for &(off, ch) in &got {
            assert_eq!(s[off..].chars().next(), Some(ch));
        }
        // visible_width sums the same walk (escapes contribute 0).
        assert_eq!(visible_width(s), 3);
    }

    #[test]
    fn wraps_on_word_boundary_not_midword() {
        let out = soft_wrap("the quick brown fox jumps", 12, "");
        // Each line must NOT split mid-word.
        for line in &out {
            for word in line.split_whitespace() {
                assert!(word.len() <= 12, "word {word:?} fits its row");
            }
        }
        // First row should be "the quick" (9 chars), not "the quick br".
        assert_eq!(out[0], "the quick");
    }

    #[test]
    fn preserves_hard_newlines() {
        let out = soft_wrap("line one\nline two", 80, "");
        assert_eq!(out, vec!["line one", "line two"]);
    }

    #[test]
    fn applies_continuation_indent() {
        let out = soft_wrap("aaa bbb ccc ddd", 7, "  ");
        // First row no indent; subsequent rows get "  " prefix.
        assert_eq!(out[0], "aaa bbb");
        for line in &out[1..] {
            assert!(line.starts_with("  "));
        }
    }

    #[test]
    fn hard_breaks_unbreakable_long_token() {
        let out = soft_wrap("aaaaaaaaaaaaaaaaaa", 5, "");
        // 18-char token across 5-wide rows = 4 rows.
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(|l| l.len() <= 5));
    }

    #[test]
    fn empty_input_returns_one_empty_row() {
        let out = soft_wrap("", 80, "");
        assert_eq!(out, vec![""]);
    }

    #[test]
    fn zero_width_returns_unwrapped_lines() {
        let out = soft_wrap("anything goes", 0, "");
        assert_eq!(out, vec!["anything goes"]);
    }

    /// CJK glyphs are width-2. A 6-cell budget fits 3 CJK chars, not 6.
    #[test]
    fn respects_display_width_for_cjk() {
        let out = soft_wrap("中文测试abc", 6, "");
        // First row: at most 3 CJK chars (6 cells), or 2 CJK + " " + "ab".
        // What we care about: no row's display width exceeds 6.
        for line in &out {
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= 6,
                "row {line:?} width = {} <= 6",
                UnicodeWidthStr::width(line.as_str()),
            );
        }
    }

    /// Pathological indent wider than budget should fall back to no
    /// indent rather than spinning or panicking.
    #[test]
    fn indent_wider_than_width_degrades_gracefully() {
        let out = soft_wrap("aaa bbb ccc", 4, "            ");
        // Should still produce output and not panic.
        assert!(!out.is_empty());
    }

    /// Review #1: leading whitespace on the FIRST row must be
    /// preserved. Option-line rendering relies on `"  ▶ label"`
    /// keeping its `  ` margin; dropping it silently flattens the
    /// visual layout.
    #[test]
    fn preserves_leading_whitespace_on_first_row() {
        let out = soft_wrap("  ▶ hello world", 80, "");
        assert_eq!(out[0], "  ▶ hello world");
    }

    /// Review #4: a CRLF logical line yields the same wrap result
    /// as the LF variant; no `\r` in the output.
    #[test]
    fn strips_carriage_returns_from_crlf_input() {
        let out_lf = soft_wrap("first\nsecond", 80, "");
        let out_crlf = soft_wrap("first\r\nsecond", 80, "");
        assert_eq!(out_lf, out_crlf);
        for row in &out_crlf {
            assert!(!row.contains('\r'), "row {row:?} should have no CR");
        }
    }

    /// Review #7: documented floor on max_width — wide glyph must
    /// not produce a row exceeding the budget.
    #[test]
    fn wide_glyph_respects_max_width_at_floor() {
        let out = soft_wrap("中文", 1, "");
        for line in &out {
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= 2,
                "row {line:?} should not exceed effective floor of 2"
            );
        }
    }

    /// Review #8: an indented blank-padded logical line keeps its
    /// indentation. `"  "` (two spaces, no word) should NOT collapse
    /// to an empty row when it's the FIRST row.
    #[test]
    fn preserves_leading_whitespace_only_line() {
        let out = soft_wrap("  ", 80, "");
        assert_eq!(out[0], "  ");
    }

    /// User bug: chamber_row_with_bg embeds `\x1b[48;5;52m…\x1b[49m`.
    /// `UnicodeWidthStr::width` counts the bracketed payload as
    /// visible cells, so soft_wrap previously thought the row was
    /// 15+ cells wider than max_width and split it — breaking the
    /// escape from its closer and leaving literal `[49m` text on
    /// the next row.
    #[test]
    fn visible_width_skips_ansi_sgr() {
        // Plain text reference.
        let plain = "-    // ng_max = ceil(32 / 8) = 4";
        // Same content wrapped in bg-color SGR + reset.
        let styled = format!("\x1b[48;5;52m{}\x1b[49m", plain);
        let pw = visible_width(plain);
        let sw = visible_width(&styled);
        assert_eq!(pw, sw, "SGR escapes must not contribute to width");
    }

    /// Soft-wrap a chamber-row-shaped string that's already padded
    /// to exactly `max_width` cells of VISIBLE content but carries
    /// 15+ chars of SGR escapes. It must NOT split into two rows.
    #[test]
    fn ansi_padded_row_does_not_overwrap() {
        let inner = 60;
        // Visible width: 1 (`-`) + 4 (pad) + visible text + trailing
        // pad = exactly `inner`. Wrap into 60-wide rows.
        let visible_content: String = format!("-text{}", " ".repeat(inner - 5));
        assert_eq!(visible_width(&visible_content), inner);
        let row = format!("│ \x1b[48;5;52m{}\x1b[49m │", visible_content);
        // The full chamber row is `inner + 4` cells wide (visible).
        let out = soft_wrap(&row, inner + 4, "");
        assert_eq!(
            out.len(),
            1,
            "row with embedded SGR must not split: got {} rows",
            out.len()
        );
    }
}
