use compact_str::CompactString;
use crossterm::style::Color;
use pulldown_cmark::{CodeBlockKind, Event, Tag, TagEnd};

use super::highlight;
use super::renderer::LineEntry;
use crate::ui::ansi;

/// Build a foreground-color ANSI SGR from a crossterm `Color`.
/// Returns an empty string for `Reset` so the caller can prepend it
/// unconditionally without spurious resets.
pub(super) fn ansi_fg(c: Color) -> String {
    match c {
        Color::Reset => String::new(),
        Color::Black => "\x1b[30m".into(),
        Color::DarkGrey => "\x1b[90m".into(),
        Color::Red => "\x1b[31m".into(),
        Color::DarkRed => "\x1b[31m".into(),
        Color::Green => "\x1b[32m".into(),
        Color::DarkGreen => "\x1b[32m".into(),
        Color::Yellow => "\x1b[33m".into(),
        Color::DarkYellow => "\x1b[33m".into(),
        Color::Blue => "\x1b[34m".into(),
        Color::DarkBlue => "\x1b[34m".into(),
        Color::Magenta => "\x1b[35m".into(),
        Color::DarkMagenta => "\x1b[35m".into(),
        Color::Cyan => "\x1b[36m".into(),
        Color::DarkCyan => "\x1b[36m".into(),
        Color::White => "\x1b[37m".into(),
        Color::Grey => "\x1b[37m".into(),
        Color::Rgb { r, g, b } => format!("\x1b[38;2;{};{};{}m", r, g, b),
        Color::AnsiValue(v) => format!("\x1b[38;5;{}m", v),
    }
}

/// Walk a string char-by-char skipping ANSI SGR escapes
/// (`\x1b[…m`). Returns visible characters in order so the wrap math
/// can count display cells correctly. Inline emphasis / inline code
/// / link spans put ANSI escapes inside the accumulator; counting
/// those as width would wrap the prose 5 cols too early per `\x1b[…m`.
fn iter_visible_chars(s: &str) -> Vec<(usize, char)> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        // Skip ANSI SGR / CSI: ESC `[` … `m` (or any final byte 0x40–0x7e).
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            i = j.saturating_add(1).min(bytes.len());
            continue;
        }
        let step = match bytes[i] {
            b if b < 0x80 => 1,
            b if b < 0xC0 => 1,
            b if b < 0xE0 => 2,
            b if b < 0xF0 => 3,
            _ => 4,
        };
        // Reconstruct the char from the slice.
        if let Some(c) = s[i..i + step.min(bytes.len() - i)].chars().next() {
            out.push((i, c));
        }
        i += step;
    }
    out
}

fn word_wrap(text: &str, max_width: usize) -> Vec<CompactString> {
    if text.is_empty() {
        return vec![CompactString::new("")];
    }
    // ANSI-aware: count VISIBLE chars (ignoring escape sequences) for
    // the width budget. Wrap by visible-char index, then slice the
    // original string by the corresponding byte offsets so escapes
    // ride along with the text they wrap.
    let visible = iter_visible_chars(text);
    if visible.len() <= max_width {
        return vec![CompactString::from(text)];
    }
    let mut lines: Vec<CompactString> = Vec::new();
    let mut start_visible = 0usize;
    while start_visible < visible.len() {
        let end_visible = (start_visible + max_width).min(visible.len());
        // Word boundary: walk backward to the last space at or before
        // `end_visible`. If none, hard-break at end_visible.
        let mut break_at = end_visible;
        if end_visible < visible.len() {
            for i in (start_visible..end_visible).rev() {
                if visible[i].1 == ' ' {
                    break_at = i + 1;
                    break;
                }
            }
            if break_at == start_visible {
                break_at = end_visible;
            }
        }
        // Map visible indices back to byte offsets in the original
        // string. The slice INCLUDES any ANSI escapes that lived
        // between `start_visible..break_at`.
        let start_byte = visible[start_visible].0;
        let end_byte = if break_at < visible.len() {
            visible[break_at].0
        } else {
            text.len()
        };
        lines.push(CompactString::from(&text[start_byte..end_byte]));
        start_visible = break_at;
    }
    lines
}

/// Shared line-flush: split `acc` on `\n`, trim a trailing `\r`, render empty
/// lines as a bare blank and non-empty lines word-wrapped to the available
/// width with `prefix` prepended to EVERY wrapped chunk (continuation rows keep
/// the prefix). The wrap width subtracts the prefix's display width (clamped to
/// >=1 so a `prefix` as wide as `max_width` can't drive word_wrap into a
/// non-advancing loop). `flush_acc` delegates with an empty prefix; the
/// blockquote bar delegates with `"│ "`.
fn flush_prefixed(
    acc: &str,
    color: Color,
    prefix: &str,
    max_width: usize,
    out: &mut Vec<LineEntry>,
) {
    if acc.is_empty() {
        return;
    }
    let prefix_w = iter_visible_chars(prefix).len();
    let inner_w = max_width.saturating_sub(prefix_w).max(1);
    for line in acc.split('\n') {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            out.push(LineEntry {
                text: CompactString::new(""),
                color,
            });
        } else {
            for chunk in word_wrap(trimmed, inner_w) {
                let text = if prefix.is_empty() {
                    chunk
                } else {
                    CompactString::from(format!("{}{}", prefix, chunk))
                };
                out.push(LineEntry { text, color });
            }
        }
    }
}

fn flush_acc(acc: &str, color: Color, max_width: usize, out: &mut Vec<LineEntry>) {
    flush_prefixed(acc, color, "", max_width, out);
}

/// Render a blockquote paragraph's accumulated text with a `│ ` chamber bar on
/// EVERY wrapped line (continuation rows keep the bar). Content is wrapped
/// first, then prefixed, so a long quoted line splits at word boundaries with
/// the bar carried onto each chunk. Empty lines render as a bare blank.
///
/// This runs at paragraph-end (the blockquote's text lives in a paragraph, so
/// `TagEnd::Paragraph` is where `acc` is non-empty) — previously the bar code
/// sat in `TagEnd::BlockQuote`, which fires AFTER the paragraph already flushed
/// `acc`, so it was dead and blockquotes rendered as bar-less dim prose.
fn flush_blockquote(acc: &str, max_width: usize, out: &mut Vec<LineEntry>) {
    // Delegates to `flush_prefixed` with the `│ ` chamber bar. The prefix-width
    // clamp there guards against word_wrap looping when max_width < 2.
    flush_prefixed(acc, crate::ui::theme::dim(), "│ ", max_width, out);
}

fn bullet_prefix(in_blockquote: bool) -> &'static str {
    if in_blockquote { "  ┊ " } else { "  • " }
}

/// Render a markdown table as `| col | col |` rows with a separator
/// line below the header. Columns are padded so the right borders
/// align. Caps each cell's display at the available width so a
/// long cell doesn't break alignment. No-ops when both header and
/// rows are empty.
fn render_table(
    header: &[String],
    rows: &[Vec<String>],
    max_width: usize,
    base_color: Color,
    out: &mut Vec<LineEntry>,
) {
    use unicode_width::UnicodeWidthStr;
    if header.is_empty() && rows.is_empty() {
        return;
    }
    // Compute per-column max DISPLAY width — emoji like ✅ occupy 2
    // terminal cells but only 1 `char`. Counting chars previously
    // produced narrow columns and the right border slid left by one
    // per emoji. `unicode_width::UnicodeWidthStr::width` returns the
    // terminal-cell width.
    let ncols = header
        .len()
        .max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if ncols == 0 {
        return;
    }
    let mut widths = vec![0usize; ncols];
    for (i, cell) in header.iter().enumerate() {
        widths[i] = widths[i].max(cell.as_str().width());
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.as_str().width());
        }
    }
    // Minimum column width — ragged rows (one row has fewer cells
    // than `ncols`) would otherwise leave `widths[i] = 0` for the
    // missing columns, breaking the separator line + right-border
    // alignment. Guarantee at least 1 char per column.
    for w in widths.iter_mut() {
        *w = (*w).max(1);
    }
    // Cap any single column to avoid one runaway cell blowing the
    // line width. Distribute available width: target inner width =
    // max_width - 4 (for outer `| ` + ` |`), minus 3*(ncols-1) for
    // ` | ` separators. Cells get clipped to fit.
    let inner = max_width.saturating_sub(2 * 2);
    let sep_overhead = if ncols > 1 { 3 * (ncols - 1) } else { 0 };
    let cell_budget = inner.saturating_sub(sep_overhead);
    let per_col = cell_budget.checked_div(ncols).unwrap_or(0);
    for w in widths.iter_mut() {
        if per_col > 0 && *w > per_col {
            *w = per_col;
        }
    }

    // `fit` pads or truncates `cell` to occupy exactly `w` terminal
    // cells. Char-based length undercounts emoji; we accumulate the
    // per-character display width and pad with the actual cell deficit
    // so right borders line up under cells of any width mix.
    let fit = |cell: &str, w: usize| -> String {
        use unicode_width::UnicodeWidthChar;
        let mut out = String::with_capacity(cell.len() + w);
        let mut used = 0usize;
        for c in cell.chars() {
            let cw = c.width().unwrap_or(0);
            // Truncate with ellipsis if the next char would overflow.
            // The ellipsis itself is 1 cell wide.
            if used + cw > w {
                if w >= 1 && used < w {
                    out.push('…');
                    used += 1;
                }
                break;
            }
            out.push(c);
            used += cw;
        }
        for _ in used..w {
            out.push(' ');
        }
        out
    };

    let render_row = |row: &[String], widths: &[usize]| -> String {
        let mut s = String::with_capacity(max_width);
        s.push_str("│ ");
        for (i, width) in widths.iter().enumerate() {
            if i > 0 {
                s.push_str(" │ ");
            }
            let cell = row.get(i).map(String::as_str).unwrap_or("");
            s.push_str(&fit(cell, *width));
        }
        s.push_str(" │");
        s
    };

    let sep = {
        let mut s = String::with_capacity(max_width);
        s.push('├');
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                s.push('┼');
            }
            for _ in 0..(w + 2) {
                s.push('─');
            }
        }
        s.push('┤');
        s
    };

    if !header.is_empty() {
        out.push(LineEntry {
            text: CompactString::new(render_row(header, &widths)),
            color: crate::ui::theme::header(),
        });
        out.push(LineEntry {
            text: CompactString::new(&sep),
            color: crate::ui::theme::dim(),
        });
    }
    for row in rows {
        out.push(LineEntry {
            text: CompactString::new(render_row(row, &widths)),
            color: base_color,
        });
    }
    out.push(LineEntry {
        text: CompactString::new(""),
        color: base_color,
    });
}

/// Render markdown text to styled line entries. `base_color` is the
/// body / paragraph color — the agent's voice. Highlights (headings,
/// code blocks, blockquotes, accents, dim/trailer text) still go
/// through their dedicated `theme::*` accessors, so a single
/// `base_color` swap shifts only the body text while keeping the
/// inline emphasis hierarchy intact.
///
/// Streams that share the markdown engine (Token, Reasoning) pass
/// their stream-specific base color here. Inline ANSI sequences for
/// bold / italic / strikethrough / inline-code ride along inside
/// each LineEntry's text, so visual hierarchy is preserved
/// regardless of the chosen base color.
pub fn markdown_to_styled(text: &str, max_width: usize, base_color: Color) -> Vec<LineEntry> {
    if text.is_empty() {
        return Vec::new();
    }

    // Strip control characters (C0, DEL, C1, ESC) from the input
    // BEFORE markdown parsing. The LLM can embed escape sequences
    // (OSC, DCS, CSI cursor moves, etc.) and bare control bytes
    // (BEL, FF, SO/SI) in its response text; if we pass them
    // through pulldown_cmark they survive into the LineEntry text
    // that `paint_line` sends to `ansi_to_tui`. While
    // `ansi_to_tui` v8 strips non-SGR escapes, bare C0 controls
    // and edge cases like truncated sequences can still reach
    // ratatui's buffer. Sanitizing here catches ALL paths that
    // render markdown — agent streaming, Done handler, resumed
    // session history, slash commands — without sprinkling calls
    // at each call site. Preserve `\n` (markdown line breaks)
    // and `\t` (code-block indentation in fenced blocks).
    let text = ansi::strip_escapes(text, ansi::StripPolicy::KEEP_BOTH);

    // Enable GFM tables so `Tag::Table*` events actually fire.
    // Without this, table syntax falls back to plain paragraphs and
    // the table never reaches `render_table`.
    let mut opts = pulldown_cmark::Options::empty();
    opts.insert(pulldown_cmark::Options::ENABLE_TABLES);
    let parser = pulldown_cmark::Parser::new_ext(&text, opts);
    let mut result = Vec::new();
    let mut acc = String::new();

    let mut in_heading = false;
    let mut heading_level: u32 = 1;
    let mut in_code_block = false;
    let mut code_block_lang = String::new();
    // Inline-style state. Pulldown emits `Start(Emphasis)` …
    // texts/codes … `End(Emphasis)`. We embed ANSI escapes directly
    // in `acc` for inline spans; `word_wrap` is ANSI-aware so the
    // escapes don't count toward the wrap width.
    let mut emphasis_depth: u32 = 0;
    let mut strong_depth: u32 = 0;
    let mut strikethrough_depth: u32 = 0;
    let mut in_blockquote = false;
    let mut ordered_list = false;
    let mut list_item_count: u64 = 0;
    // Table accumulation: pulldown_cmark emits TableHead → (Row × N
    // cells) for the header row, then more TableRow blocks for body.
    // We collect cells into `current_cell`, rows into `current_row`,
    // and the whole table into `table_header` + `table_rows`, then
    // render with column-aligned padding when the table ends.
    let mut in_table = false;
    let mut in_table_head = false;
    let mut current_cell = String::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut table_header: Vec<String> = Vec::new();
    let mut table_rows: Vec<Vec<String>> = Vec::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    // A blank `>` line between two quoted blocks: render a bare
                    // blank between them so they read as separate blocks. Gate on
                    // blockquote state (not a `│` prefix) — a quoted block may end
                    // in a list item (`  ┊ ` prefix) or heading, and a non-quote
                    // line that happens to start with `│` must not inject a blank.
                    // Skip when the previous line is already blank.
                    if in_blockquote
                        && result
                            .last()
                            .is_some_and(|e: &LineEntry| !e.text.is_empty())
                    {
                        result.push(LineEntry {
                            text: CompactString::new(""),
                            color: crate::ui::theme::dim(),
                        });
                    }
                }
                Tag::Heading { level, .. } => {
                    flush_acc(&acc, base_color, max_width, &mut result);
                    acc.clear();
                    in_heading = true;
                    heading_level = level as u32;
                }
                Tag::CodeBlock(kind) => {
                    flush_acc(&acc, base_color, max_width, &mut result);
                    acc.clear();
                    in_code_block = true;
                    code_block_lang.clear();
                    if let CodeBlockKind::Fenced(info) = kind {
                        code_block_lang.push_str(info.as_ref());
                    }
                }
                Tag::BlockQuote(_) => {
                    flush_acc(&acc, base_color, max_width, &mut result);
                    acc.clear();
                    in_blockquote = true;
                }
                Tag::List(t) => {
                    ordered_list = t.is_some();
                    list_item_count = 0;
                }
                Tag::Item => {
                    flush_acc(&acc, base_color, max_width, &mut result);
                    acc.clear();
                    list_item_count += 1;
                }
                Tag::FootnoteDefinition(_) => {}
                Tag::Table(_) => {
                    flush_acc(&acc, base_color, max_width, &mut result);
                    acc.clear();
                    in_table = true;
                    table_header.clear();
                    table_rows.clear();
                }
                Tag::TableHead => {
                    in_table_head = true;
                    current_row.clear();
                }
                Tag::TableRow => {
                    current_row.clear();
                }
                Tag::TableCell => {
                    current_cell.clear();
                }
                // Inline emphasis: open an ANSI dim+italic span. Italic
                // works on most modern terminals (iTerm2, alacritty,
                // kitty, foot). Falls back to no-op on older ones —
                // text stays readable, just not italic.
                Tag::Emphasis => {
                    if !in_table && !in_code_block {
                        acc.push_str("\x1b[3m");
                    }
                    emphasis_depth += 1;
                }
                // Bold: ANSI 1.
                Tag::Strong => {
                    if !in_table && !in_code_block {
                        acc.push_str("\x1b[1m");
                    }
                    strong_depth += 1;
                }
                // Strikethrough: ANSI 9 (universal but some terminals
                // ignore). Reset with 29.
                Tag::Strikethrough => {
                    if !in_table && !in_code_block {
                        acc.push_str("\x1b[9m");
                    }
                    strikethrough_depth += 1;
                }
                // Links: paint the link text with the accent color +
                // underline. Pulldown will emit the text in between
                // and a TagEnd::Link to close.
                Tag::Link { .. } if !in_table && !in_code_block => {
                    acc.push_str("\x1b[4m");
                    acc.push_str(&ansi_fg(crate::ui::theme::accent()));
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => {
                    if in_blockquote {
                        flush_blockquote(&acc, max_width, &mut result);
                    } else {
                        flush_acc(&acc, base_color, max_width, &mut result);
                    }
                    acc.clear();
                }
                TagEnd::Heading(_) => {
                    // Per-level color: H1 brightest (accent / banner),
                    // H2 header tone, H3+ dim header so the visual
                    // hierarchy is legible even on a monochrome 80s
                    // phosphor screen. Bold via ANSI 1 amplifies H1.
                    let color = match heading_level {
                        1 => crate::ui::theme::accent(),
                        2 => crate::ui::theme::header(),
                        _ => crate::ui::theme::header(),
                    };
                    let prefix = match heading_level {
                        1 => "\x1b[1m", // bold
                        2 => "\x1b[1m", // bold
                        _ => "",
                    };
                    let line = format!("{}{}\x1b[0m", prefix, acc);
                    if in_blockquote {
                        // Quoted heading: carry the `│ ` bar so the whole quote
                        // stays contained. The bold ANSI rides along inside the
                        // text. Suppress the trailing blank — TagEnd::BlockQuote
                        // closes the quote with its own blank.
                        flush_blockquote(&line, max_width, &mut result);
                    } else {
                        flush_acc(&line, color, max_width, &mut result);
                        result.push(LineEntry {
                            text: CompactString::new(""),
                            color: base_color,
                        });
                    }
                    acc.clear();
                    in_heading = false;
                }
                TagEnd::CodeBlock => {
                    // Route through the per-language regex highlighter.
                    // Unknown language → falls back to a single dim
                    // span per line (same look as before this change).
                    let body = acc.trim_end_matches('\n').to_string();
                    let highlighted = highlight::highlight_code(&body, &code_block_lang);
                    // Inside a quote, every code row carries the `│ ` bar (in
                    // place of the 2-space gutter) so the block stays contained.
                    let gutter = if in_blockquote { "│ " } else { "  " };
                    // Wrap long code lines to the content width instead of
                    // emitting one over-wide row per source line. The chat
                    // painter draws one screen row per LineEntry and CLIPS to
                    // width (chat.rs), so an unwrapped long line is cut off —
                    // the "I can only see one sentence" report. `word_wrap` is
                    // ANSI-aware (visible-char width, escapes ride with their
                    // text) and breaks at spaces between tokens, so the
                    // per-span coloring survives the wrap.
                    let gutter_w = iter_visible_chars(gutter).len();
                    let inner_w = max_width.saturating_sub(gutter_w).max(1);
                    for spans in highlighted {
                        if spans.is_empty() {
                            result.push(LineEntry {
                                text: if in_blockquote {
                                    CompactString::new("│ ")
                                } else {
                                    CompactString::new("")
                                },
                                color: crate::ui::theme::tool(),
                            });
                            continue;
                        }
                        // Build the colored content (no gutter), then wrap it,
                        // carrying the gutter onto every wrapped row. The color
                        // field is a fallback for terminals that strip ANSI; the
                        // embedded escapes drive the actual paint.
                        let mut content = String::new();
                        for s in &spans {
                            content.push_str(&ansi_fg(s.color));
                            content.push_str(&s.text);
                            content.push_str("\x1b[0m");
                        }
                        for chunk in word_wrap(&content, inner_w) {
                            let mut row = String::from(gutter);
                            row.push_str(&chunk);
                            // Defensive reset so a hard break mid-span can't
                            // bleed color into the row's padding.
                            row.push_str("\x1b[0m");
                            result.push(LineEntry {
                                text: CompactString::from(row),
                                color: crate::ui::theme::tool(),
                            });
                        }
                    }
                    acc.clear();
                    code_block_lang.clear();
                    in_code_block = false;
                    // Outside a quote, close the block with a blank. Inside a
                    // quote, TagEnd::BlockQuote supplies the closing blank.
                    if !in_blockquote {
                        result.push(LineEntry {
                            text: CompactString::new(""),
                            color: base_color,
                        });
                    }
                }
                TagEnd::BlockQuote(_) => {
                    // Paragraphs inside the quote already rendered with the bar
                    // at `TagEnd::Paragraph`; this flushes any straggler content
                    // not wrapped in a paragraph (defensive — normally `acc` is
                    // empty here) and closes the block with a blank line.
                    flush_blockquote(&acc, max_width, &mut result);
                    acc.clear();
                    in_blockquote = false;
                    result.push(LineEntry {
                        text: CompactString::new(""),
                        color: base_color,
                    });
                }
                TagEnd::Item => {
                    let color = if in_blockquote {
                        crate::ui::theme::dim()
                    } else {
                        base_color
                    };
                    let bullet = if ordered_list {
                        format!(" {}. ", list_item_count)
                    } else {
                        bullet_prefix(in_blockquote).to_string()
                    };
                    // Continuation lines indent to where the item text
                    // starts (under the bullet's right edge), not the
                    // bullet glyph itself, so multi-line items read as
                    // a coherent block.
                    let cont_indent: String =
                        std::iter::repeat_n(' ', bullet.chars().count()).collect();
                    let inner_w = max_width.saturating_sub(bullet.chars().count());
                    let mut item_lines = Vec::new();
                    let mut first_chunk_in_item = true;
                    for line in acc.split('\n') {
                        let trimmed = line.trim_end_matches('\r');
                        if trimmed.is_empty() {
                            item_lines.push(LineEntry {
                                text: CompactString::new(""),
                                color,
                            });
                            continue;
                        }
                        for chunk in word_wrap(trimmed, inner_w) {
                            let prefix = if first_chunk_in_item {
                                first_chunk_in_item = false;
                                bullet.as_str()
                            } else {
                                cont_indent.as_str()
                            };
                            item_lines.push(LineEntry {
                                text: CompactString::from(format!("{}{}", prefix, chunk)),
                                color,
                            });
                        }
                    }
                    result.extend(item_lines);
                    acc.clear();
                }
                TagEnd::List(_) => {
                    ordered_list = false;
                    list_item_count = 0;
                    result.push(LineEntry {
                        text: CompactString::new(""),
                        color: base_color,
                    });
                }
                TagEnd::FootnoteDefinition => {}
                TagEnd::Table => {
                    render_table(
                        &table_header,
                        &table_rows,
                        max_width,
                        base_color,
                        &mut result,
                    );
                    in_table = false;
                }
                TagEnd::TableHead => {
                    table_header = std::mem::take(&mut current_row);
                    in_table_head = false;
                }
                TagEnd::TableRow if !in_table_head => {
                    table_rows.push(std::mem::take(&mut current_row));
                }
                TagEnd::TableCell => {
                    current_row.push(std::mem::take(&mut current_cell));
                }
                TagEnd::Emphasis => {
                    emphasis_depth = emphasis_depth.saturating_sub(1);
                    if !in_table && !in_code_block {
                        acc.push_str("\x1b[23m"); // italic off
                    }
                }
                TagEnd::Strong => {
                    strong_depth = strong_depth.saturating_sub(1);
                    if !in_table && !in_code_block {
                        acc.push_str("\x1b[22m"); // bold/dim off (normal intensity)
                    }
                }
                TagEnd::Strikethrough => {
                    strikethrough_depth = strikethrough_depth.saturating_sub(1);
                    if !in_table && !in_code_block {
                        acc.push_str("\x1b[29m"); // strike off
                    }
                }
                TagEnd::Link if !in_table && !in_code_block => {
                    // Underline off, then restore the stream's BASE color —
                    // not `\x1b[39m` (terminal default), which would leave the
                    // rest of the line mis-colored (dirge-08zx).
                    acc.push_str("\x1b[24m");
                    acc.push_str(&ansi_fg(base_color));
                }
                _ => {}
            },
            Event::Text(t) => {
                if in_table {
                    current_cell.push_str(&t);
                } else {
                    // In or out of a code block, plain text accumulates
                    // verbatim — the surrounding rendering state (set by
                    // Start/End code-block events) handles styling.
                    acc.push_str(&t);
                }
            }
            Event::Code(t) => {
                if in_table {
                    current_cell.push_str(&t);
                } else if in_code_block {
                    acc.push_str(&t);
                } else {
                    // Inline `code` span: paint with the tool color +
                    // wrap in literal backticks so readers still see
                    // the markdown delimiters. Reset back to default
                    // foreground after so the surrounding paragraph
                    // color resumes. The flush_acc fallback color
                    // applies whenever ANSI is stripped.
                    acc.push_str(&ansi_fg(crate::ui::theme::tool()));
                    acc.push('`');
                    acc.push_str(&t);
                    acc.push('`');
                    // Restore the stream's BASE color, not terminal default
                    // (`\x1b[39m`) — otherwise the rest of the paragraph after
                    // an inline code span renders in the wrong color
                    // (dirge-08zx). The trailing `\x1b[0m` at line flush still
                    // guarantees no cross-line bleed.
                    acc.push_str(&ansi_fg(base_color));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_table {
                    // Break inside a table cell would smear the cell
                    // across multiple lines and misalign the row.
                    // Markdown spec doesn't allow real newlines in
                    // table cells — substitute a space so the visible
                    // content stays on one line.
                    current_cell.push(' ');
                } else {
                    acc.push('\n');
                }
            }
            Event::Rule => {
                flush_acc(&acc, base_color, max_width, &mut result);
                acc.clear();
                let rule: String = std::iter::repeat_n('─', max_width.min(40)).collect();
                result.push(LineEntry {
                    text: CompactString::from(rule),
                    color: crate::ui::theme::dim(),
                });
                result.push(LineEntry {
                    text: CompactString::new(""),
                    color: base_color,
                });
            }
            Event::Html(t) => {
                acc.push_str(&t);
            }
            Event::InlineHtml(t) => {
                acc.push_str(&t);
            }
            Event::FootnoteReference(t) => {
                acc.push_str(&t);
            }
            Event::TaskListMarker(checked) => {
                if checked {
                    acc.push_str("[x]");
                } else {
                    acc.push_str("[ ]");
                }
            }
            _ => {}
        }
    }

    if !acc.is_empty() {
        if in_blockquote {
            // Unterminated quote (pulldown normally closes it, but be safe).
            flush_blockquote(&acc, max_width, &mut result);
        } else {
            let color = if in_code_block {
                crate::ui::theme::tool()
            } else if in_heading {
                crate::ui::theme::header()
            } else {
                base_color
            };
            flush_acc(&acc, color, max_width, &mut result);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn texts(rows: &[LineEntry]) -> Vec<String> {
        rows.iter().map(|r| r.text.as_str().to_string()).collect()
    }

    /// A multi-line blockquote renders EVERY line (not just the first), each
    /// carrying the `│ ` chamber bar. Guards both the never-reproduced
    /// "blockquote cuts off after the first line" report and the actual bug
    /// found: the bar was dead code (rendered after the paragraph flushed).
    #[test]
    fn multiline_blockquote_keeps_all_lines_with_bar() {
        let input = "> First sentence here.\n> Second sentence here.\n> Third sentence here.";
        let rendered = markdown_to_styled(input, 80, crate::ui::theme::agent());
        let bar_lines: Vec<String> = texts(&rendered)
            .into_iter()
            .filter(|t| t.starts_with('│'))
            .collect();
        assert_eq!(
            bar_lines.len(),
            3,
            "all three quote lines present: {bar_lines:?}"
        );
        assert!(bar_lines[0].contains("First sentence here."));
        assert!(bar_lines[1].contains("Second sentence here."));
        assert!(bar_lines[2].contains("Third sentence here."));
    }

    /// A long single-line blockquote wraps at word boundaries and the bar rides
    /// onto every continuation row (no bar-less wrapped tail).
    #[test]
    fn long_blockquote_wraps_with_bar_on_every_row() {
        let long = "> ".to_string()
            + "Detail-oriented full-stack developer with experience building and \
               deploying web applications from concept to launch.";
        let rendered = markdown_to_styled(&long, 40, crate::ui::theme::agent());
        let bar_lines: Vec<String> = texts(&rendered)
            .into_iter()
            .filter(|t| t.starts_with('│'))
            .collect();
        assert!(
            bar_lines.len() >= 2,
            "long quote wrapped to multiple rows: {bar_lines:?}"
        );
        for l in &bar_lines {
            assert!(
                l.starts_with("│ "),
                "every wrapped row keeps the bar: {l:?}"
            );
        }
    }

    /// Two quoted paragraphs (blank `>` line between) render with a bare blank
    /// separator, both carrying the bar.
    #[test]
    fn multi_paragraph_blockquote_separates_paragraphs() {
        let input = "> Para one.\n>\n> Para two.";
        let rendered = markdown_to_styled(input, 80, crate::ui::theme::agent());
        let t = texts(&rendered);
        let bars: Vec<&String> = t.iter().filter(|x| x.starts_with('│')).collect();
        assert_eq!(bars.len(), 2, "both paragraphs render with a bar: {t:?}");
        // A bare blank line sits between the two quoted paragraphs.
        let p1 = t.iter().position(|x| x.contains("Para one.")).unwrap();
        let p2 = t.iter().position(|x| x.contains("Para two.")).unwrap();
        assert!(
            t[p1 + 1..p2].iter().any(|x| x.is_empty()),
            "blank separator between quoted paragraphs: {t:?}",
        );
    }

    /// A heading inside a blockquote (`> # Title`) must carry the `│ ` bar so
    /// the whole quote reads as one contained block — not a bar-less heading
    /// followed by barred prose.
    #[test]
    fn quoted_heading_carries_bar() {
        let input = "> # Heading\n> body text here";
        let rendered = markdown_to_styled(input, 80, crate::ui::theme::agent());
        let t = texts(&rendered);
        // Every non-empty rendered line must start with the bar.
        for line in t.iter().filter(|l| !l.is_empty()) {
            assert!(
                line.starts_with('│'),
                "quoted line missing bar: {line:?} in {t:?}"
            );
        }
        assert!(
            t.iter().any(|l| l.contains("Heading")),
            "heading text present: {t:?}"
        );
        assert!(
            t.iter().any(|l| l.contains("body text here")),
            "body text present: {t:?}"
        );
    }

    /// A fenced code block inside a blockquote carries the `│ ` bar on every
    /// code line so the quote stays visually contained.
    #[test]
    fn quoted_code_block_carries_bar() {
        let input = "> ```\n> let x = 1;\n> let y = 2;\n> ```";
        let rendered = markdown_to_styled(input, 80, crate::ui::theme::agent());
        let t = texts(&rendered);
        let code_lines: Vec<&String> = t
            .iter()
            .filter(|l| l.contains("let x") || l.contains("let y"))
            .collect();
        assert_eq!(code_lines.len(), 2, "both code lines present: {t:?}");
        for line in &code_lines {
            assert!(
                line.starts_with('│'),
                "quoted code line missing bar: {line:?}"
            );
        }
    }

    /// A quoted paragraph following a quoted LIST item must not run together —
    /// the list bullet uses a `  ┊ ` prefix (not `│`), so the separator gate
    /// must key on blockquote state, not the `│` prefix.
    #[test]
    fn quoted_list_then_paragraph_does_not_run_together() {
        let input = "> - item one\n> - item two\n>\n> Following paragraph.";
        let rendered = markdown_to_styled(input, 80, crate::ui::theme::agent());
        let t = texts(&rendered);
        let item_pos = t
            .iter()
            .position(|x| x.contains("item two"))
            .expect("list item present");
        let para_pos = t
            .iter()
            .position(|x| x.contains("Following paragraph."))
            .expect("paragraph present");
        assert!(
            t[item_pos + 1..para_pos].iter().any(|x| x.is_empty()),
            "blank separator between quoted list and quoted paragraph: {t:?}"
        );
    }

    /// A blockquote rendered at width < 2 must terminate: the bar overhead
    /// leaves inner_w=0 without the clamp, and word_wrap at 0 cannot advance.
    #[test]
    fn blockquote_width_below_two_does_not_loop() {
        // inner_w would be 0 without the clamp; word_wrap at 0 can fail to
        // advance. This must terminate and still render something.
        let rendered = markdown_to_styled("> hi there", 1, crate::ui::theme::agent());
        assert!(!rendered.is_empty(), "must render without hanging");
    }

    /// Each rendered row must occupy the same number of terminal
    /// cells so the right border `│` lines up vertically.
    fn assert_rows_same_width(rows: &[LineEntry]) {
        let widths: Vec<usize> = rows.iter().map(|r| r.text.as_str().width()).collect();
        if widths.is_empty() {
            return;
        }
        let first = widths[0];
        for (i, w) in widths.iter().enumerate() {
            assert_eq!(
                *w, first,
                "row {i} has width {w}, expected {first} (matching first row).\nrow: {:?}",
                rows[i].text,
            );
        }
    }

    /// Tables with emoji cells used to misalign the right border:
    /// `chars().count()` undercounted emoji like ✅ (2-cell-wide)
    /// as 1 char, leaving the right border 1 cell short.
    #[test]
    fn table_with_emoji_aligns_right_border() {
        let header = vec![
            "File".to_string(),
            "Action".to_string(),
            "Status".to_string(),
        ];
        let rows = vec![
            vec!["a.rs".to_string(), "CREATE".to_string(), "✅".to_string()],
            vec!["b.rs".to_string(), "MODIFY".to_string(), "✅".to_string()],
            vec!["c.rs".to_string(), "DELETE".to_string(), "❌".to_string()],
        ];
        let mut out = Vec::new();
        render_table(&header, &rows, 80, crate::ui::theme::agent(), &mut out);
        // Drop the trailing empty line; everything else must align.
        let non_empty: Vec<&LineEntry> = out.iter().filter(|e| !e.text.is_empty()).collect();
        let owned: Vec<LineEntry> = non_empty.into_iter().cloned().collect();
        assert_rows_same_width(&owned);
    }

    /// Plain ASCII rows continue to align (regression guard).
    #[test]
    fn table_with_ascii_only_aligns() {
        let header = vec!["a".to_string(), "bb".to_string()];
        let rows = vec![
            vec!["1".to_string(), "22".to_string()],
            vec!["3".to_string(), "44".to_string()],
        ];
        let mut out = Vec::new();
        render_table(&header, &rows, 80, crate::ui::theme::agent(), &mut out);
        let owned: Vec<LineEntry> = out.iter().filter(|e| !e.text.is_empty()).cloned().collect();
        assert_rows_same_width(&owned);
    }

    /// Mixed-width column (1-cell chars + emoji) still aligns.
    #[test]
    fn table_with_mixed_cell_widths_aligns() {
        let header = vec!["Name".to_string(), "Status".to_string()];
        let rows = vec![
            vec!["short".to_string(), "✅ OK".to_string()],
            vec!["longer one".to_string(), "—".to_string()],
            vec!["🚀 emoji-first".to_string(), "?".to_string()],
        ];
        let mut out = Vec::new();
        render_table(&header, &rows, 80, crate::ui::theme::agent(), &mut out);
        let owned: Vec<LineEntry> = out.iter().filter(|e| !e.text.is_empty()).cloned().collect();
        assert_rows_same_width(&owned);
    }

    /// ANSI-aware word_wrap: an inline `code` span embeds ANSI
    /// escapes (`\x1b[…m`). Counting those toward the width budget
    /// would wrap prose 5+ cells too early per escape. Verify the
    /// wrap budget is consumed only by visible characters.
    #[test]
    fn word_wrap_skips_ansi_escapes_for_width() {
        let plain = "the quick brown fox jumps over the lazy dog";
        let styled = "the quick brown \x1b[1mfox\x1b[22m jumps over the lazy dog";
        let p = word_wrap(plain, 20);
        let s = word_wrap(styled, 20);
        // Same number of wrapped rows; visible-char budgets match.
        assert_eq!(p.len(), s.len(), "ANSI-styled wrap must match plain wrap");
    }

    /// Bold (`**x**`) emits an ANSI `\x1b[1m` open inside the
    /// accumulator and `\x1b[22m` close on TagEnd::Strong.
    #[test]
    fn strong_emits_bold_ansi() {
        let rendered = markdown_to_styled("the **fox** is quick", 80, crate::ui::theme::agent());
        let blob: String = rendered
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(blob.contains("\x1b[1m"), "expected bold open");
        assert!(blob.contains("\x1b[22m"), "expected bold close");
        assert!(blob.contains("fox"), "expected wrapped text");
    }

    /// Italic (`*x*`) maps to ANSI 3 / 23.
    #[test]
    fn emphasis_emits_italic_ansi() {
        let rendered = markdown_to_styled("the *fox*", 80, crate::ui::theme::agent());
        let blob: String = rendered
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(blob.contains("\x1b[3m"));
        assert!(blob.contains("\x1b[23m"));
    }

    /// Inline code preserves the backticks as visible markers AND
    /// embeds the tool-color SGR around them.
    #[test]
    fn inline_code_paints_with_tool_color() {
        let rendered = markdown_to_styled("call `fn_name`", 80, crate::ui::theme::agent());
        let blob: String = rendered
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(blob.contains("`fn_name`"));
        // dirge-08zx: after an inline code span, the foreground returns to the
        // stream's BASE color (here: agent), not terminal default (`\x1b[39m`),
        // so the rest of the paragraph stays correctly colored.
        assert!(
            blob.contains(&format!("`{}", ansi_fg(crate::ui::theme::agent()))),
            "inline code should restore the base color after the closing backtick: {blob:?}"
        );
        assert!(
            !blob.contains("\x1b[39m"),
            "must not reset to terminal default fg (that mis-colors the rest of the line)"
        );
    }

    /// Fenced code block with `rust` info string gets per-keyword
    /// coloring (verified by presence of an SGR sequence for `fn`).
    #[test]
    fn fenced_rust_block_gets_keyword_coloring() {
        let rendered =
            markdown_to_styled("```rust\nfn main() {}\n```", 80, crate::ui::theme::agent());
        let blob: String = rendered
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // The `fn` keyword should appear inside an SGR-wrapped span.
        // We don't pin a specific color (theme-dependent), but the
        // presence of any 38;5;<n> / 38;2;<r>;<g>;<b> / 3[0-7] SGR
        // immediately before "fn" indicates it was painted.
        assert!(blob.contains("fn"));
        assert!(blob.contains("\x1b["));
    }

    /// A long single line inside a fenced code block must WRAP to multiple rows
    /// (each within the width) rather than be emitted as one over-wide
    /// `LineEntry` — the chat painter draws one screen row per entry and CLIPS
    /// to width, so an unwrapped long code line gets cut off (the "I can only
    /// see one sentence in the code block" report). Prose already wraps via
    /// `word_wrap`; code rows previously did not.
    #[test]
    fn long_code_line_wraps_instead_of_clipping() {
        let line = "this is a very long single line of profile text inside a code \
                    block that must wrap across several rows instead of being clipped \
                    at the window edge";
        let input = format!("```\n{line}\n```");
        let width = 40;
        let rendered = markdown_to_styled(&input, width, crate::ui::theme::agent());

        // Code rows carry the 2-space gutter; strip ANSI to measure + inspect.
        let code_rows: Vec<String> = rendered
            .iter()
            .map(|e| crate::ui::ansi::strip_ansi(&e.text))
            .filter(|t| t.starts_with("  ") && !t.trim().is_empty())
            .collect();

        assert!(
            code_rows.len() >= 2,
            "long code line must wrap to >=2 rows, got {code_rows:?}"
        );
        for r in &code_rows {
            assert!(
                UnicodeWidthStr::width(r.as_str()) <= width,
                "wrapped code row exceeds width {width}: {r:?} (w={})",
                UnicodeWidthStr::width(r.as_str())
            );
        }
        // Every word survived across the wrapped rows (nothing clipped).
        let joined: String = code_rows
            .iter()
            .map(|r| r.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("this is a very long"),
            "head preserved: {joined:?}"
        );
        assert!(
            joined.contains("wrap across several rows"),
            "middle preserved: {joined:?}"
        );
        assert!(
            joined.contains("at the window edge"),
            "tail preserved: {joined:?}"
        );
    }

    /// Control characters injected by the LLM into response text
    /// must be stripped before markdown parsing — the function
    /// embeds its own SGR escapes and those are the only ones
    /// that should reach ratatui's buffer.
    #[test]
    fn strips_control_characters_before_parsing() {
        let rendered = markdown_to_styled(
            "hello\x1b]0;EVIL\x07 world\x07\x1b[2J\x0c",
            80,
            crate::ui::theme::agent(),
        );
        let blob: String = rendered
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join("");
        // Visible text preserved.
        assert!(blob.contains("hello"), "expected 'hello', got: {blob:?}");
        assert!(blob.contains("world"), "expected 'world', got: {blob:?}");
        // OSC title-set must be gone (ESC + ]...BEL).
        assert!(!blob.contains("EVIL"), "OSC payload must be stripped");
        // BEL must be gone.
        assert!(!blob.contains('\x07'), "BEL must be stripped: {blob:?}");
        // CSI clear-screen must be gone.
        assert!(!blob.contains("[2J"), "CSI 2J must be stripped: {blob:?}");
        // Form feed must be gone.
        assert!(!blob.contains('\x0c'), "FF must be stripped: {blob:?}");
        // SGR escapes generated BY the markdown renderer itself
        // (for markdown styling) are fine — those use \x1b[...m
        // which is the only format we embed.
    }

    /// Tab characters in code blocks survive sanitization so
    /// indented fenced blocks render correctly.
    #[test]
    fn preserves_tabs_for_code_block_indentation() {
        let rendered = markdown_to_styled(
            "```rust\n\tfn main() {}\n```",
            80,
            crate::ui::theme::agent(),
        );
        let blob: String = rendered
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            blob.contains('\t'),
            "tab must survive for code indent: {blob:?}"
        );
        // Syntax highlighting wraps `fn` in ANSI SGR escapes, so
        // "fn main" may not be contiguous in the rendered output.
        let plain = ansi::strip_ansi(&blob);
        assert!(
            plain.contains("fn main"),
            "code text must survive: {plain:?}"
        );
    }
}
