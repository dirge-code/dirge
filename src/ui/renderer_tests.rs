use super::*;

/// dirge-8855: `PANEL_AUTO_MIN_COLS` must be the real boundary at which a
/// side panel first has room (`content_indent() >= 15`), not the stale
/// 100 that the indent gate silently overrode. The gutter indent for a
/// given terminal width is `((cols-2) - min(cols-2, 120)) / 2`.
#[test]
fn panel_auto_min_cols_matches_gutter_threshold() {
    let indent_at = |cols: u16| -> usize {
        let band = (cols.saturating_sub(2)) as usize;
        let target = band.min(120);
        band.saturating_sub(target) / 2
    };
    assert!(
        indent_at(PANEL_AUTO_MIN_COLS) >= 15,
        "at the threshold ({PANEL_AUTO_MIN_COLS}) a panel must fit (indent {} >= 15)",
        indent_at(PANEL_AUTO_MIN_COLS),
    );
    assert!(
        indent_at(PANEL_AUTO_MIN_COLS - 1) < 15,
        "one column below the threshold a panel must NOT fit (indent {} < 15)",
        indent_at(PANEL_AUTO_MIN_COLS - 1),
    );
}

#[test]
fn parse_display_spec_full_layout() {
    let v = parse_display_spec("left|main|right").unwrap();
    assert!(v.left && v.right);
}

#[test]
fn parse_display_spec_main_only_hides_both_sides() {
    let v = parse_display_spec("main").unwrap();
    assert!(!v.left && !v.right);
}

#[test]
fn parse_display_spec_main_and_right() {
    let v = parse_display_spec("main|right").unwrap();
    assert!(!v.left && v.right);
}

#[test]
fn parse_display_spec_left_only() {
    // `main` omitted; it's always shown regardless, so only `left`
    // toggles on here.
    let v = parse_display_spec("left").unwrap();
    assert!(v.left && !v.right);
}

#[test]
fn parse_display_spec_accepts_whitespace_commas_and_case() {
    let v = parse_display_spec("RIGHT, Left").unwrap();
    assert!(v.left && v.right);
    let v = parse_display_spec("main right").unwrap();
    assert!(!v.left && v.right);
}

#[test]
fn parse_display_spec_rejects_empty_and_unknown() {
    assert!(parse_display_spec("").is_err());
    assert!(parse_display_spec("   ").is_err());
    let err = parse_display_spec("middle").unwrap_err();
    assert!(
        err.contains("middle"),
        "error should name the bad token: {err}"
    );
}

/// wrap_editor: empty buffer → one empty row, cursor at (0, 0).
#[test]
fn wrap_editor_empty() {
    let (rows, r, c) = wrap_editor("", 0, 80);
    assert_eq!(rows, vec![String::new()]);
    assert_eq!((r, c), (0, 0));
}

/// wrap_editor: short single-line text doesn't wrap.
#[test]
fn wrap_editor_no_wrap_short() {
    let (rows, r, c) = wrap_editor("hello", 5, 80);
    assert_eq!(rows, vec!["hello".to_string()]);
    assert_eq!((r, c), (0, 5));
}

/// wrap_editor: hard newlines split into logical rows.
#[test]
fn wrap_editor_newlines_split() {
    let (rows, r, c) = wrap_editor("a\nb\ncc", 5, 80);
    assert_eq!(
        rows,
        vec!["a".to_string(), "b".to_string(), "cc".to_string()]
    );
    // Cursor at byte 5 = "cc" position 1.
    assert_eq!((r, c), (2, 1));
}

/// wrap_editor: long line soft-wraps to wrap_w cells. Cursor
/// lands on the wrapped row.
#[test]
fn wrap_editor_soft_wrap() {
    let s = "abcdefghij"; // 10 chars
    let (rows, r, c) = wrap_editor(s, 10, 4);
    // Wrap to 4 cells: ["abcd", "efgh", "ij"] (cursor at end).
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], "abcd");
    assert_eq!(rows[1], "efgh");
    assert_eq!(rows[2], "ij");
    assert_eq!((r, c), (2, 2));
}

/// dirge-0dqe / dirge-5w9v: a long single (un-newlined) buffer with
/// the cursor at the end soft-wraps to many rows, each ≤ wrap_w cells,
/// and the cursor lands on the LAST row — the property both the
/// questionnaire wrap and the compose-box scroll rely on to keep the
/// typed tail visible.
#[test]
fn wrap_editor_long_buffer_tail_on_last_row() {
    use unicode_width::UnicodeWidthStr;
    let s = "word ".repeat(60); // ~300 cells, no hard newlines
    let wrap_w = 20;
    let (rows, cursor_row, _) = wrap_editor(&s, s.len(), wrap_w);
    assert!(rows.len() > 1, "long buffer must wrap to multiple rows");
    for row in &rows {
        assert!(
            UnicodeWidthStr::width(row.as_str()) <= wrap_w,
            "each row must fit wrap_w; got {:?} ({} cells)",
            row,
            UnicodeWidthStr::width(row.as_str())
        );
    }
    assert_eq!(
        cursor_row as usize,
        rows.len() - 1,
        "cursor at end of buffer must land on the last (visible) row"
    );
}

/// dirge-5w9v: editor_scroll_offset keeps the cursor row inside the
/// window once wrapped content exceeds the capped box height.
#[test]
fn editor_scroll_offset_keeps_cursor_visible() {
    // Everything fits → no scroll.
    assert_eq!(editor_scroll_offset(5, 4, 8), 0);
    assert_eq!(editor_scroll_offset(8, 7, 8), 0);
    // Content exceeds window; cursor near the end → scroll so the
    // cursor lands on the last visible row.
    assert_eq!(editor_scroll_offset(20, 19, 8), 12); // 19 - (8-1)
    assert_eq!(editor_scroll_offset(20, 10, 8), 3); // 10 - 7
    // Cursor still within the first window → no scroll.
    assert_eq!(editor_scroll_offset(20, 5, 8), 0);
    // Never scroll past the end.
    assert_eq!(editor_scroll_offset(10, 9, 8), 2); // max_offset = 10-8
    // Degenerate windows.
    assert_eq!(editor_scroll_offset(10, 9, 0), 0);
    assert_eq!(editor_scroll_offset(0, 0, 8), 0);
}

/// dirge-ov2 Phase A: chat switching saves the prior chat's
/// buffer and selection, then loads the target chat's snapshot.
/// Round-trip preserves content.
#[test]
fn chat_snapshot_save_load_roundtrip() {
    let mut r = Renderer::new().expect("renderer");
    // Default chat is "main" at index 0.
    assert_eq!(r.active_chat(), 0);
    assert_eq!(r.chat_count(), 1);
    assert_eq!(r.chat_names(), vec!["main".to_string()]);

    // Seed main chat with some content.
    r.buffer.push(LineEntry {
        text: CompactString::new("main-line-1"),
        color: Color::White,
    });
    r.scroll_offset = 5;

    // Spawn a subagent chat and switch to it.
    let sub_idx = r.add_chat("subagent-1");
    assert_eq!(sub_idx, 1);
    assert_eq!(r.chat_count(), 2);
    r.switch_chat(sub_idx);
    assert_eq!(r.active_chat(), 1);

    // Subagent chat starts empty.
    assert!(r.buffer.is_empty());
    assert_eq!(r.scroll_offset, 0);

    // Add content to the subagent chat.
    r.buffer.push(LineEntry {
        text: CompactString::new("sub-line-1"),
        color: Color::Cyan,
    });
    r.scroll_offset = 2;

    // Switch back to main — its content must be restored.
    r.switch_chat(0);
    assert_eq!(r.buffer.len(), 1);
    assert_eq!(r.buffer[0].text.as_str(), "main-line-1");
    assert_eq!(r.scroll_offset, 5);

    // Switch back to subagent — its content also restored.
    r.switch_chat(1);
    assert_eq!(r.buffer.len(), 1);
    assert_eq!(r.buffer[0].text.as_str(), "sub-line-1");
    assert_eq!(r.scroll_offset, 2);

    // Switch to same chat is a no-op.
    r.switch_chat(1);
    assert_eq!(r.buffer.len(), 1);

    // Out-of-range index is a no-op (defensive — caller bug).
    r.switch_chat(99);
    assert_eq!(r.active_chat(), 1);
}

/// next_chat wraps around from last → first.
#[test]
fn next_chat_cycles_forward_with_wrap() {
    let mut r = Renderer::new().expect("renderer");
    r.add_chat("one");
    r.add_chat("two");
    assert_eq!(r.chat_count(), 3); // main + one + two
    assert_eq!(r.active_chat(), 0);
    r.next_chat();
    assert_eq!(r.active_chat(), 1);
    r.next_chat();
    assert_eq!(r.active_chat(), 2);
    r.next_chat(); // wrap
    assert_eq!(r.active_chat(), 0);
}

/// prev_chat wraps around from first → last.
#[test]
fn prev_chat_cycles_backward_with_wrap() {
    let mut r = Renderer::new().expect("renderer");
    r.add_chat("one");
    r.add_chat("two");
    assert_eq!(r.chat_count(), 3);
    // prev from 0 wraps to 2
    r.prev_chat();
    assert_eq!(r.active_chat(), 2);
    r.prev_chat();
    assert_eq!(r.active_chat(), 1);
    r.prev_chat();
    assert_eq!(r.active_chat(), 0);
}

/// next/prev are no-ops with only one chat.
#[test]
fn next_prev_noop_with_single_chat() {
    let mut r = Renderer::new().expect("renderer");
    assert_eq!(r.chat_count(), 1);
    r.next_chat();
    assert_eq!(r.active_chat(), 0);
    r.prev_chat();
    assert_eq!(r.active_chat(), 0);
}

/// remove_chat removes a chat and adjusts active_chat.
#[test]
fn remove_chat_adjusts_active() {
    let mut r = Renderer::new().expect("renderer");
    r.add_chat("one");
    r.add_chat("two");
    r.add_chat("three");
    // chats: [main, one, two, three], active=0
    r.switch_chat(2); // active = "two"
    assert_eq!(r.active_chat(), 2);
    // Remove chat 1 ("one") — active stays 2 but now points
    // to what WAS chat 2 (now shifted to index 1).
    r.remove_chat(1);
    assert_eq!(r.chat_count(), 3);
    assert_eq!(r.active_chat(), 1); // shifted down
    // Remove active chat — moves to next (or last if at end).
    r.switch_chat(2); // active = last chat ("three")
    r.remove_chat(2);
    assert_eq!(r.active_chat(), 0); // wraps to 0

    // Cannot remove the last remaining chat.
    let mut r2 = Renderer::new().expect("renderer");
    r2.remove_chat(0);
    assert_eq!(r2.chat_count(), 1);
    assert_eq!(r2.active_chat(), 0);
}

/// Create a renderer with a synthetic buffer of `n` short lines so we
/// can drive scroll/append behavior without touching a real terminal.
/// If `n` is less than `visible + min_scroll_margin`, pads to that size
/// so scroll_line_up actually has room to scroll regardless of terminal
/// height. Pass `min_scroll_margin: 15` for typical tests that need 10
/// scroll-up presses.
fn fresh_with_lines_scrollable(n: usize, min_scroll_margin: usize) -> Renderer {
    let mut r = Renderer::new().expect("renderer");
    let visible = r.visible_lines();
    let need = (visible + min_scroll_margin).max(n);
    for i in 0..need {
        r.buffer.push(LineEntry {
            text: CompactString::new(format!("line {i}")),
            color: Color::White,
        });
    }
    r.lines = r.buffer.len() as u16;
    r
}

/// Create a renderer with a synthetic buffer of `n` short lines so we
/// can drive scroll/append behavior without touching a real terminal.
fn fresh_with_lines(n: usize) -> Renderer {
    fresh_with_lines_scrollable(n, /* min_scroll_margin */ 15)
}

/// Absolute index of the first visible line in the current viewport,
/// matching the formula used by `render_viewport`.
fn view_start(r: &Renderer) -> usize {
    let visible = r.visible_lines();
    let total = r.buffer.len();
    let start = if r.scroll_offset == 0 {
        total.saturating_sub(visible)
    } else {
        total.saturating_sub(r.scroll_offset + visible)
    };
    start.min(total.saturating_sub(visible))
}

// Regression: previously, when the user scrolled up while output was
// streaming, scroll_offset stayed fixed but the buffer grew — so the
// viewport drifted forward into newer content. The fix bumps
// scroll_offset by one per appended line so the view stays anchored to
// the same absolute lines.
#[test]
fn regression_scrolled_up_view_stays_anchored_through_appends() {
    let mut r = fresh_with_lines(50);
    // Scroll up 10 lines. View start changes; record it.
    for _ in 0..10 {
        r.scroll_line_up();
    }
    let pinned_start = view_start(&r);

    // Stream in 8 new lines while the user is scrolled up.
    for i in 0..8 {
        r.push_buffer_line(LineEntry {
            text: CompactString::new(format!("new {i}")),
            color: Color::White,
        });
    }

    // The first visible line index hasn't moved.
    assert_eq!(view_start(&r), pinned_start);
}

// Regression: replace_from (used by the streaming-token markdown path)
// also has to honor the scroll anchor. If the agent's current response
// grows (or shrinks) while the user is scrolled up viewing earlier
// content, the earlier content must stay in view.
#[test]
fn regression_replace_from_keeps_view_anchored_when_scrolled_up() {
    // Build a buffer with enough lines that scrolling into the
    // middle actually works regardless of terminal height.
    let mut r = fresh_with_lines_scrollable(50, /* margin */ 15);
    for _ in 0..10 {
        r.scroll_line_up();
    }
    let pinned_start = view_start(&r);

    // Replace the tail of the buffer (last 10 lines) with twice
    // as many — simulates a streaming markdown re-render that
    // grew the current response. The user is scrolled above the
    // replaced region, so the view must stay anchored.
    let total = r.buffer.len();
    let repl_start = total.saturating_sub(10);
    let new_lines: Vec<LineEntry> = (0..20)
        .map(|i| LineEntry {
            text: CompactString::new(format!("repl {i}")),
            color: Color::White,
        })
        .collect();
    r.replace_from(repl_start, new_lines);

    assert_eq!(
        view_start(&r),
        pinned_start,
        "view drifted after replace-with-more"
    );

    // Now replace with FEWER lines (response got shorter via
    // re-render). The view should not drift upward past where
    // the user originally was.
    let total = r.buffer.len();
    let repl_start = total.saturating_sub(8);
    let shorter: Vec<LineEntry> = (0..3)
        .map(|i| LineEntry {
            text: CompactString::new(format!("sh {i}")),
            color: Color::White,
        })
        .collect();
    r.replace_from(repl_start, shorter);
    let after = view_start(&r);
    assert!(
        after <= pinned_start,
        "view drifted upward: after={after} pinned_start={pinned_start}",
    );
}

// When the user is AT the bottom (scroll_offset == 0), new content must
// be visible — the view follows the bottom. The anchor behavior must not
// accidentally pin the bottom-anchored view.
#[test]
fn at_bottom_view_follows_new_content() {
    let mut r = fresh_with_lines(50);
    assert_eq!(r.scroll_offset, 0);

    for i in 0..5 {
        r.push_buffer_line(LineEntry {
            text: CompactString::new(format!("new {i}")),
            color: Color::White,
        });
    }
    assert_eq!(r.scroll_offset, 0, "bottom-anchored view must stay at 0");

    let visible = r.visible_lines();
    let total = r.buffer.len();
    assert_eq!(view_start(&r), total.saturating_sub(visible));
}

// Selection indices are absolute and must NOT shift when content
// streams in. Prior to the anchor fix the selection rectangle visually
// drifted because scroll_offset stayed put while the viewport advanced;
// now the indices are still preserved and the viewport stays anchored,
// so the selection rectangle stays where the user dragged it.
#[test]
fn selection_indices_stay_absolute_under_streaming_appends() {
    let mut r = fresh_with_lines(50);
    for _ in 0..10 {
        r.scroll_line_up();
    }
    r.selection_active = true;
    r.selection_start = Some((15, 0));
    r.selection_end = Some((20, 5));

    for i in 0..7 {
        r.push_buffer_line(LineEntry {
            text: CompactString::new(format!("new {i}")),
            color: Color::White,
        });
    }

    // Selection indices are absolute and remain untouched.
    assert_eq!(r.selection_start, Some((15, 0)));
    assert_eq!(r.selection_end, Some((20, 5)));
}

// Boundary: a tiny buffer where appending pushes scroll_offset past
// max_offset. The clamp inside push_buffer_line keeps it in range.
#[test]
fn push_clamps_scroll_offset_to_max_when_buffer_grows() {
    let mut r = fresh_with_lines(2);
    let visible = r.visible_lines();
    // Force a non-zero offset (clamp may already prevent it on tiny
    // buffers; assert behavior either way).
    r.scroll_offset = 100;
    for _ in 0..3 {
        r.push_buffer_line(LineEntry {
            text: CompactString::new("more"),
            color: Color::White,
        });
    }
    let max_offset = r.buffer.len().saturating_sub(visible);
    assert!(
        r.scroll_offset <= max_offset,
        "scroll_offset {} must be ≤ max {}",
        r.scroll_offset,
        max_offset
    );
}

// Streaming via commit_partial (the path used by `write` for streamed
// tokens) also goes through push_buffer_line. Verify the partial commit
// bumps the offset when scrolled up.
#[test]
fn commit_partial_routes_through_anchor_aware_push() {
    let mut r = fresh_with_lines(50);
    for _ in 0..10 {
        r.scroll_line_up();
    }
    let pinned_start = view_start(&r);

    r.partial = CompactString::new("a streamed token chunk");
    r.partial_color = Color::White;
    r.commit_partial();

    assert_eq!(view_start(&r), pinned_start);
}

// --- granular selection ----------------------------------------------

fn fresh_with_text(lines: &[&str]) -> Renderer {
    let mut r = Renderer::new().unwrap();
    for s in lines {
        r.buffer.push(LineEntry {
            text: CompactString::new(s),
            color: Color::White,
        });
    }
    r
}

/// Same-row selection extracts the substring between start.1 and
/// end.1 (char-indexed, exclusive end).
#[test]
fn selected_text_single_row_substring() {
    let mut r = fresh_with_text(&["hello world"]);
    r.selection_active = true;
    r.selection_start = Some((0, 6));
    r.selection_end = Some((0, 11));
    assert_eq!(r.selected_text(), Some("world".to_string()));
}

/// Reverse drag (end before start) still yields the same substring —
/// `selected_text` normalizes to row-major order.
#[test]
fn selected_text_reverse_drag_normalizes() {
    let mut r = fresh_with_text(&["hello world"]);
    r.selection_active = true;
    r.selection_start = Some((0, 11));
    r.selection_end = Some((0, 6));
    assert_eq!(r.selected_text(), Some("world".to_string()));
}

/// Multi-row selection takes the tail of the start row, the full
/// middle rows, and the head of the end row.
#[test]
fn selected_text_multi_row_spans_lines() {
    let mut r = fresh_with_text(&["first line", "middle", "last line"]);
    r.selection_active = true;
    r.selection_start = Some((0, 6)); // "line"
    r.selection_end = Some((2, 4)); // "last"
    assert_eq!(r.selected_text(), Some("line\nmiddle\nlast".to_string()));
}

/// Same-row empty selection (start == end) returns None — nothing
/// selected yet, just a click.
#[test]
fn selected_text_empty_selection_returns_none() {
    let mut r = fresh_with_text(&["hello"]);
    r.selection_active = true;
    r.selection_start = Some((0, 3));
    r.selection_end = Some((0, 3));
    assert!(r.selected_text().is_none());
}

/// Multi-byte UTF-8: char indices ignore byte width. `é` and `🦀`
/// each count as 1 char, not their byte widths.
#[test]
fn selected_text_handles_unicode() {
    let mut r = fresh_with_text(&["café 🦀 rust"]);
    r.selection_active = true;
    r.selection_start = Some((0, 0));
    r.selection_end = Some((0, 6)); // "café 🦀"
    assert_eq!(r.selected_text(), Some("café 🦀".to_string()));
}

/// Markdown rendering bakes SGR escapes into LineEntry::text;
/// the selection path must strip them before handing the
/// string to the clipboard. Columns reflect user-perceived
/// character offsets in the visible glyphs, not the
/// escape-laden source.
#[test]
fn selected_text_strips_ansi_escapes() {
    // Visible text is "hello red world" (15 chars). The buffer
    // line carries `\x1b[31m` around "red".
    let mut r = fresh_with_text(&[]);
    r.buffer.clear();
    r.buffer.push(LineEntry {
        text: CompactString::from("hello \x1b[31mred\x1b[0m world"),
        color: Color::Reset,
    });
    r.selection_active = true;
    // Select the full visible content (cols 0..15).
    r.selection_start = Some((0, 0));
    r.selection_end = Some((0, 15));
    assert_eq!(r.selected_text(), Some("hello red world".to_string()));

    // Substring selection lands on clean chars too —
    // "red world" is cols 6..15 of the stripped text.
    r.selection_end = Some((0, 15));
    r.selection_start = Some((0, 6));
    assert_eq!(r.selected_text(), Some("red world".to_string()));
}

/// dirge-el8o: prose the renderer soft-wrapped across several display
/// rows must copy back as ONE line. `word_wrap` keeps the breaking
/// space on the prior row (chunks look like `["the quick ", "brown
/// fox ", "jumps"]`), so a continuation row — one whose predecessor
/// ends in whitespace — joins with no separator instead of a newline.
#[test]
fn selected_text_joins_soft_wrapped_rows() {
    let mut r = fresh_with_text(&["the quick ", "brown fox ", "jumps"]);
    r.selection_active = true;
    r.selection_start = Some((0, 0));
    r.selection_end = Some((2, 5));
    assert_eq!(
        r.selected_text(),
        Some("the quick brown fox jumps".to_string())
    );
}

/// A real line break — a row that does NOT end in whitespace — keeps
/// its newline. Paragraph structure (and the blank line between
/// paragraphs) survives the copy.
#[test]
fn selected_text_keeps_hard_newlines_and_blanks() {
    let mut r = fresh_with_text(&["para one ", "wraps here", "", "next para"]);
    r.selection_active = true;
    r.selection_start = Some((0, 0));
    r.selection_end = Some((3, 9));
    assert_eq!(
        r.selected_text(),
        Some("para one wraps here\n\nnext para".to_string())
    );
}

/// End-to-end: a paragraph wrapped by the REAL markdown path
/// (`markdown_to_styled` → `word_wrap`) copies back as the original
/// prose. Guards the join against changes to how wrapping splits.
#[test]
fn selected_text_joins_real_wrapped_markdown() {
    let prose = "the quick brown fox jumps over the lazy dog again and again";
    let mut styled = crate::ui::markdown::markdown_to_styled(prose, 20, Color::White);
    // Drop any trailing blank row the renderer may append after the
    // paragraph so the selection ends on real content.
    while styled
        .last()
        .is_some_and(|e| crate::ui::ansi::strip_ansi(&e.text).trim().is_empty())
    {
        styled.pop();
    }
    assert!(styled.len() > 1, "prose should wrap to multiple rows");
    let last = styled.len() - 1;
    let last_len = crate::ui::ansi::strip_ansi(&styled[last].text)
        .chars()
        .count();
    let mut r = Renderer::new().unwrap();
    r.buffer.clear();
    for e in styled {
        r.buffer.push(e);
    }
    r.selection_active = true;
    r.selection_start = Some((0, 0));
    r.selection_end = Some((last, last_len));
    assert_eq!(r.selected_text(), Some(prose.to_string()));
}

/// The join decision is made per-boundary: a partial first row still
/// counts as ending in whitespace, and the head of the final row is
/// appended without a leading newline when its predecessor wrapped.
#[test]
fn selected_text_join_respects_partial_rows() {
    let mut r = fresh_with_text(&["xxthe quick ", "brown foxyy"]);
    r.selection_active = true;
    r.selection_start = Some((0, 2)); // skip "xx"
    r.selection_end = Some((1, 9)); // up to "brown fox"
    assert_eq!(r.selected_text(), Some("the quick brown fox".to_string()));
}

/// `buffer_pos_at` clamps char_col to the line's length so dragging
/// past the right edge anchors at end-of-line rather than
/// silently extending past visible content.
#[test]
fn buffer_pos_at_clamps_past_eol() {
    let r = fresh_with_text(&["short"]);
    // Row 0 is the chat top frame in the ui-redesign; row 1 is
    // the first chat content row. `buffer_line_at_row` returns
    // Some(0) for row 1 (start = 0 after saturating, idx = 0).
    let pos = r.buffer_pos_at(1, 999);
    assert_eq!(pos, Some((0, 5)));
}

// --- B3-8: display-width-aware column mapping --------------

#[test]
fn display_col_to_char_index_ascii_round_trip() {
    // ASCII: 1 char = 1 display cell. char_index == display_col.
    assert_eq!(display_col_to_char_index("hello", 0), 0);
    assert_eq!(display_col_to_char_index("hello", 3), 3);
    assert_eq!(display_col_to_char_index("hello", 5), 5);
    // Past EOL clamps to char count.
    assert_eq!(display_col_to_char_index("hello", 99), 5);
}

#[test]
fn display_col_to_char_index_cjk_compresses() {
    // "日本" — 2 chars, 4 display cells.
    let s = "日本";
    assert_eq!(display_col_to_char_index(s, 0), 0);
    // Display col 1: middle of 日 — anchor to its start (char 0).
    assert_eq!(display_col_to_char_index(s, 1), 0);
    assert_eq!(display_col_to_char_index(s, 2), 1); // start of 本
    assert_eq!(display_col_to_char_index(s, 3), 1); // middle of 本
    assert_eq!(display_col_to_char_index(s, 4), 2); // EOL
    assert_eq!(display_col_to_char_index(s, 99), 2);
}

#[test]
fn display_col_to_char_index_emoji() {
    // "a🦀b" — 3 chars, widths 1 + 2 + 1 = 4 cells.
    let s = "a🦀b";
    assert_eq!(display_col_to_char_index(s, 0), 0); // start
    assert_eq!(display_col_to_char_index(s, 1), 1); // start of 🦀
    assert_eq!(display_col_to_char_index(s, 2), 1); // middle of 🦀
    assert_eq!(display_col_to_char_index(s, 3), 2); // start of b
    assert_eq!(display_col_to_char_index(s, 4), 3); // EOL
}

/// L-R3: buffer_pos_at clamps to VISIBLE char count (post ANSI
/// strip) not raw char count. Without this, a click far right
/// on a styled line would clamp past the visible-text length
/// and selected_text's slice would either return an empty
/// string or land in the middle of the escape bytes.
#[test]
fn buffer_pos_at_clamps_to_visible_chars_not_raw_bytes() {
    let mut r = fresh_with_text(&[]);
    r.buffer.clear();
    // Visible: "hello red world" — 15 chars. Raw: 25 chars
    // (including 10 chars of `\x1b[31m` + `\x1b[0m` escape).
    r.buffer.push(LineEntry {
        text: CompactString::from("hello \x1b[31mred\x1b[0m world"),
        color: Color::Reset,
    });
    // Click well past the visible end. content_indent() is 0
    // in the default test renderer, so col == char_col. Row 1
    // is the first chat content row (row 0 is the chat frame).
    let pos = r.buffer_pos_at(1, 999).expect("must resolve");
    assert_eq!(pos.1, 15, "clamp should hit visible length 15, not raw 25");
}

// --- wrap_input -------------------------------------------------------

fn lines(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

#[test]
fn wrap_empty_buffer_has_one_row() {
    let (rows, cr, cc) = wrap_input(&lines(&[""]), 0, 0, 10);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].logical_line, 0);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 0));
    assert_eq!((cr, cc), (0, 0));
}

#[test]
fn wrap_short_line_no_split() {
    let (rows, cr, cc) = wrap_input(&lines(&["hi"]), 0, 2, 10);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 2));
    assert_eq!((cr, cc), (0, 2));
}

#[test]
fn wrap_splits_long_line_into_multiple_visual_rows() {
    // "abcdefghi" with wrap_width=3 -> 3 rows of 3 chars each.
    let (rows, cr, cc) = wrap_input(&lines(&["abcdefghi"]), 0, 5, 3);
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 3));
    assert_eq!((rows[1].char_start, rows[1].char_end), (3, 6));
    assert_eq!((rows[2].char_start, rows[2].char_end), (6, 9));
    // cursor at col 5 -> row 1, col 2
    assert_eq!((cr, cc), (1, 2));
}

#[test]
fn wrap_cursor_at_exact_boundary_stays_on_filled_row() {
    // "abc" with wrap_width=3 — cursor at col 3 (end of line). Should
    // sit at the right edge of the only row, not on a phantom row 1.
    let (rows, cr, cc) = wrap_input(&lines(&["abc"]), 0, 3, 3);
    assert_eq!(rows.len(), 1);
    assert_eq!((cr, cc), (0, 3));
}

#[test]
fn wrap_cursor_after_full_row_with_continuation() {
    // "abcdef" with wrap_width=3 — cursor at col 6 (end). Two rows, cursor
    // at end of row 1 (col 3), not at start of phantom row 2.
    let (rows, cr, cc) = wrap_input(&lines(&["abcdef"]), 0, 6, 3);
    assert_eq!(rows.len(), 2);
    assert_eq!((cr, cc), (1, 3));
}

#[test]
fn wrap_cursor_at_start_of_continuation_row() {
    // "abcdef" with wrap_width=3 — cursor at col 3 (just past first row).
    // Not the exact-boundary "at end of line" case: chars continue.
    let (rows, cr, cc) = wrap_input(&lines(&["abcdef"]), 0, 3, 3);
    assert_eq!(rows.len(), 2);
    assert_eq!((cr, cc), (1, 0));
}

#[test]
fn wrap_multiple_logical_lines() {
    // Two logical lines, second one has the cursor.
    let (rows, cr, cc) = wrap_input(&lines(&["abc", "defgh"]), 1, 4, 3);
    // Line 0: 1 row (3 chars); Line 1: 2 rows (3 + 2)
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].logical_line, 0);
    assert_eq!(rows[1].logical_line, 1);
    assert_eq!(rows[2].logical_line, 1);
    // Cursor at line 1, col 4 -> within line 1's row 1 (visual row 2 overall), col 1
    assert_eq!((cr, cc), (2, 1));
}

#[test]
fn wrap_empty_then_filled_line_cursor_on_empty() {
    // ["", "abc"] with cursor on line 0 at col 0.
    let (rows, cr, cc) = wrap_input(&lines(&["", "abc"]), 0, 0, 3);
    // Line 0: 1 (empty) row; Line 1: 1 row of "abc"
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 0));
    assert_eq!((rows[1].char_start, rows[1].char_end), (0, 3));
    assert_eq!((cr, cc), (0, 0));
}

#[test]
fn wrap_width_one_degenerate() {
    // wrap_width=1 in extremely narrow terminal — every char becomes its
    // own row. Should not panic and cursor should still map.
    let (rows, cr, cc) = wrap_input(&lines(&["abc"]), 0, 2, 1);
    assert_eq!(rows.len(), 3);
    assert_eq!((cr, cc), (2, 0));
}

#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn terminal_title_idle_and_done_show_simple_title() {
    use crate::ui::avatar::AvatarState;
    let t = super::format_terminal_title(AvatarState::Idle, None);
    assert_eq!(t, "● dirge");
    let t = super::format_terminal_title(AvatarState::Done, Some("bash"));
    assert_eq!(t, "● dirge");
}

#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn terminal_title_shows_tool_name_for_working_states() {
    use crate::ui::avatar::AvatarState;
    let t = super::format_terminal_title(AvatarState::Reading, Some("grep"));
    assert!(t.contains("grep"), "title should contain tool name: {t:?}");
    assert!(
        t.contains("◌"),
        "working states should use yellow dot marker: {t:?}"
    );
    let t = super::format_terminal_title(AvatarState::Writing, Some("edit"));
    assert!(t.contains("edit"), "title should contain tool name: {t:?}");
    let t = super::format_terminal_title(AvatarState::Bash, Some("bash"));
    assert!(t.contains("bash"), "title should contain tool name: {t:?}");
}

#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn terminal_title_error_and_alert_show_warning_marker() {
    use crate::ui::avatar::AvatarState;
    let t = super::format_terminal_title(AvatarState::Error, None);
    assert!(t.contains("ERROR"));
    assert!(
        t.contains("✗"),
        "error states should use red dot marker: {t:?}"
    );
    let t = super::format_terminal_title(AvatarState::Alert, None);
    assert!(t.contains("needs input"));
    assert!(
        t.contains("✗"),
        "alert states should use red dot marker: {t:?}"
    );
}

/// PR #144 follow-up: tool names containing BEL/ESC/newline must
/// be scrubbed before being concatenated into the OSC payload —
/// otherwise a hostile plugin or MCP server could inject further
/// escape sequences via `set_last_tool_name`.
#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn terminal_title_strips_control_bytes_from_tool_name() {
    use crate::ui::avatar::AvatarState;
    let t = super::format_terminal_title(AvatarState::Reading, Some("evil\x07\x1b[31m"));
    assert!(!t.contains('\x07'));
    assert!(!t.contains('\x1b'));
    // The clean residue should still surface so the user sees
    // *something* if the name was mostly text.
    assert!(t.contains("evil"));
}

#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn terminal_title_all_control_bytes_falls_back_to_working() {
    use crate::ui::avatar::AvatarState;
    let t = super::format_terminal_title(AvatarState::Bash, Some("\x07\x1b\n"));
    assert_eq!(t, "◌ dirge: working");
}

#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn osc_set_title_uses_st_terminator() {
    let bytes = super::osc_set_title("hello");
    // OSC introducer `\x1b]0;` + payload + ST terminator `\x1b\\`
    assert_eq!(bytes, b"\x1b]0;hello\x1b\\");
    assert!(
        !bytes.contains(&0x07),
        "BEL should not be used: {:?}",
        bytes
    );
}

#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn osc_reset_title_releases_to_shell() {
    let bytes = super::osc_reset_title();
    assert_eq!(bytes, b"\x1b]0;\x1b\\");
}

/// dirge-b11: helper — build a `PanelData` with `n` synthetic
/// modified-file entries. Keeps the unit tests for the scroll
/// offset state machine self-contained.
fn panel_with_modified(n: usize) -> PanelData {
    PanelData {
        modified: (0..n).map(|i| format!("f{i}.rs")).collect(),
        ..PanelData::default()
    }
}

/// dirge-b11: scrolling beyond the list's tail clamps to
/// `total - visible_rows`. Scrolling by a positive delta repeatedly
/// must not strand the offset past the last visible page.
#[test]
fn modified_offset_clamps_to_list_size() {
    let mut r = Renderer::new().unwrap();
    r.set_panel_data(panel_with_modified(20));
    // Visible window = 5 rows → max offset = 15. Scrolling by 100
    // must clamp, not overshoot.
    r.panel_modified_scroll(100, 5);
    assert_eq!(r.modified_offset, 15);
    // And scrolling further forward is a no-op (returns false).
    let changed = r.panel_modified_scroll(10, 5);
    assert!(!changed);
    assert_eq!(r.modified_offset, 15);
    // Scrolling backwards past 0 clamps to 0.
    r.panel_modified_scroll(-1000, 5);
    assert_eq!(r.modified_offset, 0);
}

/// dirge-b11: when the underlying MODIFIED list grows (a new file
/// modification just landed) the renderer must reset the scroll
/// offset to 0 so the user immediately sees the newest entry —
/// otherwise an in-progress investigation would scroll past
/// fresh activity without warning.
#[test]
fn modified_offset_resets_on_new_entry() {
    let mut r = Renderer::new().unwrap();
    // Seed with 20 entries and scroll into the middle of the list.
    r.set_panel_data(panel_with_modified(20));
    r.panel_modified_scroll(7, 5);
    assert_eq!(r.modified_offset, 7);
    // List grows N → N+1: offset must snap back to 0.
    r.set_panel_data(panel_with_modified(21));
    assert_eq!(r.modified_offset, 0);
}

/// dirge-b11: when the list shrinks (entries pruned at the 256-
/// cap or via cwd change), the offset stays put — the render-time
/// clamp handles the case where the offset would otherwise point
/// past the end. Growth is the only event that resets the view.
#[test]
fn modified_offset_persists_on_shrink() {
    let mut r = Renderer::new().unwrap();
    r.set_panel_data(panel_with_modified(20));
    r.panel_modified_scroll(7, 5);
    assert_eq!(r.modified_offset, 7);
    // Shrink — offset survives because the user might still want
    // to inspect what's left.
    r.set_panel_data(panel_with_modified(15));
    assert_eq!(r.modified_offset, 7);
}

/// dirge-b11: when the list fits inside the visible window, the
/// scroll operation is a no-op (and resets a stale offset to 0 as
/// a safety net). Mouse wheel ticks here must not leave the
/// footer reading `↑ N newer / ↓ M older` against an empty older
/// segment.
#[test]
fn modified_offset_no_op_when_list_fits() {
    let mut r = Renderer::new().unwrap();
    r.set_panel_data(panel_with_modified(3));
    // 3 entries fit in 5 visible rows → scroll is no-op and
    // returns false.
    let changed = r.panel_modified_scroll(5, 5);
    assert!(!changed);
    assert_eq!(r.modified_offset, 0);
}

/// dirge-b11: when the user has scrolled, the MODIFIED sub-panel's
/// footer reads `↑ N newer / ↓ M older` so they know there's
/// content in BOTH directions. When `offset == 0` (default view)
/// the footer keeps the original `+N older` shape so existing
/// screenshots / behavior stay intact.
#[test]
fn footer_shows_both_directions_when_scrolled() {
    use crate::ui::tui::layout::Layout;
    use crate::ui::tui::panels::RightPanel;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    // Build a panel with 20 modified entries — list will overflow
    // any realistic visible window.
    let data = panel_with_modified(20);
    let layout = Layout::new(160, 30, 1);
    // Render the panel TWICE: once at offset 0, once scrolled
    // mid-list. Verify the footer flips between the two shapes.
    let scan_footer = |offset: usize| -> String {
        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(
                    RightPanel::new(&data).modified_offset(offset),
                    layout.right_panel,
                );
            })
            .unwrap();
        backend = terminal.backend().clone();
        // The footer occupies the bottom-1 row of the right panel
        // (above the ╰─╯ border). Scan all panel rows for the
        // shape — robust against minor layout drift.
        let mut rows: Vec<String> = Vec::new();
        for y in layout.right_panel.y..(layout.right_panel.y + layout.right_panel.height) {
            let row: String = (layout.right_panel.x
                ..layout.right_panel.x + layout.right_panel.width)
                .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
                .collect();
            rows.push(row);
        }
        rows.join("\n")
    };
    let head = scan_footer(0);
    assert!(
        head.contains("older"),
        "default-view footer should still read '+N older'; got:\n{head}"
    );
    assert!(
        !head.contains("newer"),
        "default-view footer should NOT mention 'newer'; got:\n{head}"
    );
    let scrolled = scan_footer(5);
    // The narrow right panel may truncate the trailing "older"
    // word at the right border; assert against the "↑ N newer"
    // half plus the leading "↓" downward arrow that the dual-
    // direction footer adds. Both arrows together → both
    // directions surfaced.
    assert!(
        scrolled.contains("↑") && scrolled.contains("newer") && scrolled.contains("↓"),
        "scrolled footer should mention BOTH directions; got:\n{scrolled}"
    );
}

// ── terminal-mode self-healing (fix/mouse-capture-self-heal) ──────────

/// The re-assert payload re-enables exactly the modes dirge owns and that a
/// child program can clobber: SGR mouse capture (so wheel + click reach the
/// app) and bracketed paste. This is what heals "the whole UI scrolls off"
/// — a leaked `?1000l` is undone by the next due paint re-emitting `?1000h`.
#[test]
fn mode_reassert_payload_re_enables_mouse_and_paste() {
    let now = std::time::Instant::now();
    let bytes = super::mode_reassert_payload(None, now).expect("first paint always re-asserts");
    let s = std::str::from_utf8(bytes).unwrap();
    assert!(
        s.contains("\x1b[?1000h"),
        "must re-enable basic mouse tracking"
    );
    assert!(
        s.contains("\x1b[?1006h"),
        "must re-enable SGR mouse encoding"
    );
    assert!(s.contains("\x1b[?2004h"), "must re-enable bracketed paste");
    // Must NOT re-enter the alternate screen (would risk a per-second
    // clear/flicker) or toggle cursor visibility (managed per frame).
    assert!(!s.contains("\x1b[?1049h"), "must not re-enter alt screen");
    assert!(!s.contains("\x1b[?25"), "must not touch cursor visibility");
}

/// The throttle: re-assert on the first paint, then suppress until the
/// interval elapses, then re-assert again. A leak that lands between paints
/// is healed within one interval.
#[test]
fn mode_reassert_payload_is_throttled() {
    let t0 = std::time::Instant::now();
    assert!(
        super::mode_reassert_payload(None, t0).is_some(),
        "first paint (no prior assert) re-asserts"
    );
    assert!(
        super::mode_reassert_payload(Some(t0), t0).is_none(),
        "same instant → not due"
    );
    assert!(
        super::mode_reassert_payload(Some(t0), t0 + std::time::Duration::from_millis(100))
            .is_none(),
        "100ms later → still throttled"
    );
    assert!(
        super::mode_reassert_payload(Some(t0), t0 + super::MODE_REASSERT_INTERVAL).is_some(),
        "after the interval → re-asserts (self-heal)"
    );
}

/// Scrollback eviction (front drain past MAX_SCROLLBACK) bumps the
/// eviction generation — the counter the Ctrl+O collapse guard relies on
/// to know an absolute line anchor has been invalidated.
#[test]
fn eviction_generation_bumps_when_scrollback_overflows() {
    let mut r = Renderer::new().expect("renderer");
    assert_eq!(r.eviction_generation(), 0);
    // MAX_SCROLLBACK is 20_000; push enough to trigger at least one drain.
    for i in 0..20_050 {
        let _ = r.write_line(&format!("l{i}"), Color::White);
    }
    assert!(
        r.eviction_generation() >= 1,
        "front eviction must bump the generation"
    );
}

// ── dirge-qy3y: scrollback reflow on resize ─────────────────────────

/// Plain text re-wraps to a new width when the buffer is rebuilt from its
/// source blocks, and rebuilding back at the original width reproduces the
/// original wrap exactly.
#[test]
fn rebuild_reflows_plain_text_to_new_width() {
    let mut r = Renderer::new().expect("renderer");
    r.set_test_cols(40);
    let long = "the quick brown fox jumps over the lazy dog and then keeps on running very far";
    r.write_line(long, Color::White).unwrap();
    let wide_rows = r.buffer_len();

    r.set_test_cols(20);
    r.rebuild();
    let narrow_rows = r.buffer_len();
    assert!(
        narrow_rows > wide_rows,
        "narrower width must wrap into more rows: {wide_rows} -> {narrow_rows}",
    );

    r.set_test_cols(40);
    r.rebuild();
    assert_eq!(
        r.buffer_len(),
        wide_rows,
        "rebuild at the original width reproduces the original wrap",
    );
}

/// A committed markdown table reflows its column layout to a narrower width
/// on rebuild (the bug report: tables kept their first-render widths).
#[test]
fn rebuild_reflows_markdown_table() {
    use unicode_width::UnicodeWidthStr;
    let mut r = Renderer::new().expect("renderer");
    r.set_test_cols(70);
    let table = "| name | description |\n|---|---|\n| alpha | the first item here |\n| beta | a second item over there |";
    r.stream(table, Color::White, false);
    r.commit_stream();
    let wide_max = r
        .buffer_lines()
        .iter()
        .map(|l| UnicodeWidthStr::width(*l))
        .max()
        .unwrap_or(0);

    r.set_test_cols(34);
    r.rebuild();
    let narrow_max = r
        .buffer_lines()
        .iter()
        .map(|l| UnicodeWidthStr::width(*l))
        .max()
        .unwrap_or(0);

    assert!(
        narrow_max < wide_max,
        "table must reflow to a smaller max row width: {wide_max} -> {narrow_max}",
    );
    // content_width at 34 cols = min(34-2, 120) = 32.
    assert!(
        narrow_max <= 32,
        "reflowed table rows must fit the new content width (<=32), got {narrow_max}",
    );
}

/// The strong invariant the whole design rests on: at a fixed width, the
/// derived `buffer` equals a rebuild from `source` — so `source` is a
/// faithful mirror of `buffer` across plain, markdown, and interleaved
/// content.
#[test]
fn rebuild_is_idempotent_at_same_width() {
    let mut r = Renderer::new().expect("renderer");
    r.set_test_cols(50);
    r.write_line("hello world", Color::White).unwrap();
    r.stream(
        "# Heading\n\nsome **bold** prose that runs on",
        Color::White,
        true,
    );
    r.commit_stream();
    r.write_line("trailing status line", Color::White).unwrap();

    let before: Vec<String> = r.buffer_lines().iter().map(|s| s.to_string()).collect();
    r.rebuild();
    let after: Vec<String> = r.buffer_lines().iter().map(|s| s.to_string()).collect();
    assert_eq!(
        before, after,
        "rebuild at the same width must reproduce the buffer exactly",
    );
}

/// Pre-formatted chamber rows (recorded via `write_line_raw` as `Raw` blocks)
/// must NOT re-wrap on a narrowing rebuild — they're preserved verbatim so the
/// box borders don't fracture (a `Plain` block of the same text would wrap).
#[test]
fn raw_rows_do_not_rewrap_on_narrowing() {
    let mut r = Renderer::new().expect("renderer");
    r.set_test_cols(80);
    let row = format!("│ {} │", "x".repeat(60));
    r.write_line_raw(&row, Color::White).unwrap();
    let before = r.buffer_len();
    assert_eq!(before, 1, "one raw row");

    r.set_test_cols(30);
    r.rebuild();
    assert_eq!(
        r.buffer_len(),
        1,
        "raw row must stay a single row after narrowing (no border-fracturing re-wrap)",
    );
    assert_eq!(r.buffer_lines()[0], row, "raw row preserved verbatim");
}
