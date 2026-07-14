use compact_str::CompactString;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::keymap::{InputAction, InputKeymap};
use crate::ui::picker::FilePicker;
#[cfg(feature = "slash-completion")]
use crate::ui::slash::CompletionResult;
#[cfg(feature = "slash-completion")]
use crate::ui::slash::try_complete;

const KILL_RING_MAX: usize = 10;

// `cursor` is a byte-offset into `buffer` (UTF-8). The helpers below move the
// cursor by one character boundary so we never land in the middle of a
// multibyte sequence — that would panic on the next insert/remove in
// `CompactString`/`String`.
enum KillDir {
    Prepend,
    Append,
}

#[derive(Default)]
struct YankState {
    index: usize,
    cursor: usize,
    len: usize,
}

/// Max undo snapshots retained. Old entries drop off the front once
/// exceeded — a long editing session can't grow this unboundedly.
const UNDO_MAX: usize = 200;

/// Classifies the edit that produced a buffer change so consecutive
/// runs of typing collapse into one undo step while discrete edits
/// (paste, delete, kill) each get their own (dirge-7yea).
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    /// Inserting a non-whitespace character — coalesces with a prior
    /// `Insert` so a typed word is undone in one step.
    Insert,
    /// Inserting a whitespace character — joins the current word's undo
    /// step but closes it, so the next word starts fresh. Undo then
    /// removes a whole word (plus its trailing space) at a time.
    InsertBoundary,
    /// Anything else that mutated the buffer (paste, backspace, kill,
    /// yank, newline, …) — always its own undo step.
    Other,
}

/// A restorable snapshot of the editable state, captured BEFORE an
/// edit. Cursor is restored to where it sat before the edit.
struct UndoSnapshot {
    buffer: CompactString,
    cursor: usize,
    pastes: Vec<Option<PasteSlot>>,
}

/// Reinterpret an AltGr-composed character as literal text input (gh-659).
///
/// Windows reports AltGr as left-Ctrl + right-Alt, so a character that
/// needs AltGr on the active layout — `@ # [ ] { }` on the Italian,
/// German, Spanish, … layouts — reaches the app as a `Char` event with
/// BOTH `CONTROL` and `ALT` set, even though crossterm has already filled
/// in the composed `char`. The editor's insertion arms gate on
/// `!ctrl && !alt`, so without this those characters — and pastes
/// containing them, which the Windows console backend delivers as
/// synthesized keystrokes — are silently dropped. When both modifiers
/// accompany a printable char, strip them so the keystroke is treated as
/// text. Single-modifier chords (Ctrl+key / Alt+key keybindings) and
/// non-printable chars are left untouched, so real keybindings still fire.
/// Any Shift is preserved (some layouts compose via AltGr+Shift).
fn normalize_altgr(mut key: KeyEvent) -> KeyEvent {
    if let KeyCode::Char(c) = key.code
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT)
        && !c.is_control()
    {
        key.modifiers -= KeyModifiers::CONTROL | KeyModifiers::ALT;
    }
    key
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let len = s.len();
    let mut i = (idx + 1).min(len);
    while i < len && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn is_whitespace(ch: char) -> bool {
    ch.is_whitespace()
}

fn prev_word_boundary(s: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut idx = prev_char_boundary(s, cursor);
    // skip trailing whitespace
    while idx > 0 {
        let ch = s[..idx].chars().next_back().unwrap_or(' ');
        if !is_whitespace(ch) {
            break;
        }
        idx = prev_char_boundary(s, idx);
    }
    // determine character class at position
    if idx == 0 {
        return 0;
    }
    let ch = s[..idx].chars().next_back().unwrap_or(' ');
    let is_word = is_word_char(ch);
    // skip backward through same class
    while idx > 0 {
        let ch = s[..idx].chars().next_back().unwrap_or(' ');
        let current_is_word = is_word_char(ch);
        if current_is_word != is_word || is_whitespace(ch) {
            break;
        }
        let prev = prev_char_boundary(s, idx);
        if prev == idx {
            break;
        }
        idx = prev;
    }
    idx
}

fn next_word_boundary(s: &str, cursor: usize) -> usize {
    let len = s.len();
    if cursor >= len {
        return len;
    }
    let ch = s[cursor..].chars().next().unwrap_or(' ');
    let is_word = is_word_char(ch);
    let is_ws = is_whitespace(ch);
    let mut idx = cursor;
    // skip current class (word, punct, or whitespace)
    while idx < len {
        let ch = s[idx..].chars().next().unwrap_or(' ');
        let current_is_word = is_word_char(ch);
        let current_is_ws = is_whitespace(ch);
        if is_ws {
            if !current_is_ws {
                break;
            }
        } else if current_is_word != is_word {
            break;
        }
        idx = next_char_boundary(s, idx);
    }
    // skip whitespace and punctuation between words
    while idx < len {
        let ch = s[idx..].chars().next().unwrap_or(' ');
        if is_word_char(ch) {
            break;
        }
        idx = next_char_boundary(s, idx);
    }
    idx
}

fn cursor_line_start(s: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let haystack = &s[..cursor];
    match haystack.rfind('\n') {
        Some(pos) => pos + 1,
        None => 0,
    }
}

fn cursor_line_end(s: &str, cursor: usize) -> usize {
    let haystack = &s[cursor..];
    match haystack.find('\n') {
        Some(pos) => cursor + pos,
        None => s.len(),
    }
}

fn prev_line_start(s: &str, cursor: usize) -> Option<usize> {
    let line_start = cursor_line_start(s, cursor);
    if line_start == 0 {
        return None;
    }
    Some(cursor_line_start(s, line_start.saturating_sub(1)))
}

fn next_line_start(s: &str, cursor: usize) -> Option<usize> {
    let after = &s[cursor..];
    after.find('\n').map(|p| cursor + p + 1)
}

/// Byte offset to move the cursor to for a vertical (Up/Down) keystroke
/// across SOFT-wrapped display rows (dirge-5w9v). Reuses the renderer's
/// `wrap_editor` (the single soft-wrap source — no duplicated logic) to
/// find the cursor's display `(row, col)`, then locates the byte on the
/// adjacent row nearest that column. Returns `None` at the top/bottom
/// display row (caller then falls through to history). Every returned
/// offset is a real char boundary (the scan steps by `next_char_boundary`),
/// so a subsequent slice can't panic. O(rows·len) but only on a keypress
/// over a small compose buffer.
///
/// Only valid when the display buffer equals the raw buffer (no paste
/// placeholders); callers guard on that.
fn wrap_vertical_target(s: &str, cursor: usize, wrap_w: usize, up: bool) -> Option<usize> {
    use crate::ui::renderer::wrap_editor;
    let (rows, cur_row, cur_col) = wrap_editor(s, cursor, wrap_w);
    let target_row: u16 = if up {
        cur_row.checked_sub(1)?
    } else {
        let next = cur_row + 1;
        if next as usize >= rows.len() {
            return None;
        }
        next
    };
    // Scan char boundaries; on the target row, return the first offset
    // whose display column reaches `cur_col`, else the row's last offset.
    let mut best: Option<usize> = None;
    let mut b = 0usize;
    loop {
        let (_, r, c) = wrap_editor(s, b, wrap_w);
        if r == target_row {
            if c >= cur_col {
                return Some(b);
            }
            best = Some(b);
        } else if r > target_row {
            break;
        }
        if b >= s.len() {
            break;
        }
        b = next_char_boundary(s, b);
    }
    best
}

/// Map a CHAR column on the line `[start..end]` of `s` to its
/// byte offset within `s`. Clamps to `end` when the column exceeds
/// the line's char count. Used by history Up/Down (H-batch1-2) so
/// vertical motion across multi-byte chars never lands a cursor
/// mid-codepoint — slicing the buffer there would panic.
fn byte_at_char_col(s: &str, start: usize, end: usize, char_col: usize) -> usize {
    let line = &s[start..end];
    match line.char_indices().nth(char_col) {
        Some((i, _)) => start + i,
        None => end, // column past EOL — clamp
    }
}

/// Threshold for collapsing pastes: anything with >= this many newlines becomes a
/// `[N lines pasted]` placeholder. Single-line and short pastes go in raw so a
/// quick paste-of-a-command isn't surprising.
const PASTE_COLLAPSE_LINES: usize = 4;

/// Sentinel character bracketing a paste placeholder in the buffer. The buffer
/// stores `\x01<index>\x01`, where `<index>` is the decimal index into
/// `pastes`. Because `\x01` is filtered out of bracketed-paste content (see
/// `handle_paste`) and ignored as a typeable key, it can't appear in normal
/// input — so its presence reliably marks a placeholder block.
const PASTE_MARK: char = '\x01';

/// One paste slot referenced by a `\x01<idx>\x01` marker. Most pastes
/// are multi-line text; an image paste (Ctrl+V with an image on the
/// clipboard) stores the raw bytes here until submit, when they're
/// written to the session asset dir and attached to the prompt.
#[derive(Clone, Debug, PartialEq)]
pub enum PasteSlot {
    Text(CompactString),
    Image(PendingImage),
}

/// An image awaiting submit. The bytes live in the slot until the turn
/// is sent; on submit they're persisted via `Session::write_asset` and
/// referenced by an `ImageRef` (asset id) on the prompt.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingImage {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

pub struct InputEditor {
    pub buffer: CompactString,
    pub cursor: usize,
    history: Vec<CompactString>,
    history_pos: Option<usize>,
    /// In-progress text stashed when the user starts navigating history.
    /// Restored when they navigate past the newest entry (Down at the
    /// most-recent history slot) so the draft isn't lost. `None` when
    /// the live buffer IS the draft (not navigating history).
    history_draft: Option<(CompactString, usize)>,
    pub picker: Option<FilePicker>,
    kill_ring: Vec<CompactString>,
    last_action_was_kill: bool,
    yank_state: Option<YankState>,
    /// Pasted text bodies indexed by the digits appearing between `\x01` marks
    /// in the buffer. `None` entries are tombstones for expanded pastes (so
    /// existing indices remain valid).
    pastes: Vec<Option<PasteSlot>>,
    /// Image slots drained out of `pastes` at submit time (Enter), for
    /// the caller to persist + attach to the prompt. Cleared by the
    /// caller after it consumes them. `handle_key` clears `pastes`
    /// before returning, so this is the only channel for the submit
    /// images to escape.
    pub pending_images: Vec<PendingImage>,
    /// Current slash-command completion state, for rendering a preview.
    #[cfg(feature = "slash-completion")]
    pub completion: Option<CompletionResult>,
    /// Display width the buffer is soft-wrapped to in the box, pushed in
    /// from the renderer before each key dispatch. `0` = unknown (e.g.
    /// before the first render) → Up/Down fall back to hard-newline
    /// motion. Used to make vertical motion wrap-aware (dirge-5w9v).
    wrap_w: usize,
    /// Whether Ctrl+R reverse-i-search mode is active.
    search_mode: bool,
    /// Accumulated search query during reverse-i-search.
    search_query: CompactString,
    /// Index into `history` of the currently displayed match.
    search_match_idx: Option<usize>,
    /// Buffer + cursor stashed when entering search mode. Restored on cancel.
    search_draft: Option<(CompactString, usize)>,
    /// Resolves a key chord to a rebindable [`InputAction`]. Built-in
    /// defaults reproduce the historical text-editing keys; config/plugin
    /// overrides layer on in later phases (dirge-8fkp).
    keymap: InputKeymap,
    /// Undo history: snapshots captured before each edit, newest last
    /// (dirge-7yea). Ctrl+Z pops and restores.
    undo_stack: Vec<UndoSnapshot>,
    /// Kind of the most recent recorded edit, for coalescing typing
    /// runs. `None` after a non-edit key, an undo, or a submit so the
    /// next edit always starts a fresh undo group.
    last_edit_kind: Option<EditKind>,
}

/// Find the marker block `\x01<digits>\x01` containing or starting at
/// `cursor`. Returns `(start_of_opening_mark, byte_after_closing_mark, index)`.
fn marker_containing(s: &str, cursor: usize) -> Option<(usize, usize, usize)> {
    let bytes = s.as_bytes();
    // Walk back from cursor to find an opening PASTE_MARK.
    let mut i = cursor.min(bytes.len());
    while i > 0 && bytes[i - 1] != PASTE_MARK as u8 {
        i -= 1;
    }
    if i == 0 {
        return None;
    }
    // i is just after a PASTE_MARK; the opening mark is at i-1.
    let open = i - 1;
    let rest = &bytes[i..];
    let close_rel = rest.iter().position(|&b| b == PASTE_MARK as u8)?;
    let close = i + close_rel;
    if cursor > close {
        return None;
    }
    let digits = std::str::from_utf8(&bytes[i..close]).ok()?;
    let idx = digits.parse::<usize>().ok()?;
    Some((open, close + 1, idx))
}

/// If `pos` falls strictly inside a marker block `(start, end)`, return
/// `start` (so cursor motion moves *before* the block). Otherwise return
/// `pos` unchanged.
fn skip_left_over_marker(s: &str, pos: usize) -> usize {
    for (start, end, _) in marker_blocks(s) {
        if pos > start && pos < end {
            return start;
        }
    }
    pos
}

/// If `pos` falls strictly inside a marker block `(start, end)`, return
/// `end` (so cursor motion moves *after* the block). Otherwise return
/// `pos` unchanged.
fn skip_right_over_marker(s: &str, pos: usize) -> usize {
    for (start, end, _) in marker_blocks(s) {
        if pos > start && pos < end {
            return end;
        }
    }
    pos
}

/// Move one cursor step left, treating any marker block as a single unit.
fn prev_pos(s: &str, cursor: usize) -> usize {
    skip_left_over_marker(s, prev_char_boundary(s, cursor))
}

/// Move one cursor step right, treating any marker block as a single unit.
fn next_pos(s: &str, cursor: usize) -> usize {
    skip_right_over_marker(s, next_char_boundary(s, cursor))
}

/// Word-skip left, but never land mid-marker. `prev_word_boundary` is
/// marker-blind (it sees `\x01` as punctuation and would happily split the
/// marker open), so we post-process with `skip_left_over_marker` to round any
/// in-marker landing back to the marker's left edge.
fn prev_word_pos(s: &str, cursor: usize) -> usize {
    skip_left_over_marker(s, prev_word_boundary(s, cursor))
}

/// Word-skip right, with the symmetric marker-safety post-process.
fn next_word_pos(s: &str, cursor: usize) -> usize {
    skip_right_over_marker(s, next_word_boundary(s, cursor))
}

/// What range a backspace at `cursor` should remove. If the character to the
/// left is the closing mark of a placeholder, return the whole block;
/// otherwise return a single char.
fn backspace_range(s: &str, cursor: usize) -> Option<(usize, usize)> {
    if cursor == 0 {
        return None;
    }
    if let Some((start, end, _)) = marker_containing(s, cursor.saturating_sub(1))
        && cursor == end
    {
        return Some((start, end));
    }
    Some((prev_char_boundary(s, cursor), cursor))
}

/// What range a delete at `cursor` should remove. If the cursor sits at the
/// opening of a placeholder, return the whole block; otherwise a single char.
fn delete_range(s: &str, cursor: usize) -> Option<(usize, usize)> {
    if cursor >= s.len() {
        return None;
    }
    if let Some((start, end, _)) = marker_containing(s, cursor + 1)
        && cursor == start
    {
        return Some((start, end));
    }
    Some((cursor, next_char_boundary(s, cursor)))
}

/// Scan `s` and return each marker block as `(start, end, index)` in order.
fn marker_blocks(s: &str) -> Vec<(usize, usize, usize)> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == PASTE_MARK as u8 {
            let start = i;
            let body_start = i + 1;
            if let Some(rel) = bytes[body_start..]
                .iter()
                .position(|&b| b == PASTE_MARK as u8)
            {
                let close = body_start + rel;
                if let Ok(digits) = std::str::from_utf8(&bytes[body_start..close])
                    && let Ok(idx) = digits.parse::<usize>()
                {
                    out.push((start, close + 1, idx));
                    i = close + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// Compute the placeholder display string for a paste body.
fn placeholder_display(text: &str) -> String {
    let lines = text.matches('\n').count() + 1;
    format!("[{} lines pasted]", lines)
}

impl InputEditor {
    pub fn new() -> Self {
        InputEditor {
            buffer: CompactString::new(""),
            cursor: 0,
            history: Vec::new(),
            history_pos: None,
            history_draft: None,
            picker: None,
            kill_ring: Vec::new(),
            last_action_was_kill: false,
            yank_state: None,
            pastes: Vec::new(),
            pending_images: Vec::new(),
            #[cfg(feature = "slash-completion")]
            completion: None,
            wrap_w: 0,
            search_mode: false,
            search_query: CompactString::new(""),
            search_match_idx: None,
            search_draft: None,
            keymap: InputKeymap::defaults(),
            undo_stack: Vec::new(),
            last_edit_kind: None,
        }
    }

    /// Push the current display wrap width (from the renderer) so Up/Down
    /// can move by SOFT-wrapped display rows, not just hard newlines.
    pub fn set_wrap_width(&mut self, wrap_w: usize) {
        self.wrap_w = wrap_w;
    }

    /// Install the input keymap built from config (dirge-xv9l). Replaces
    /// the built-in defaults the editor starts with so user `keybindings`
    /// targeting input-editor commands take effect.
    pub fn set_keymap(&mut self, keymap: InputKeymap) {
        self.keymap = keymap;
    }

    /// Insert pasted text. If it spans `PASTE_COLLAPSE_LINES` or more lines,
    /// store it and insert a `[N lines pasted]` placeholder; otherwise insert
    /// raw. If the same content was already pasted and is still represented
    /// by a placeholder, expand that placeholder inline instead (so a second
    /// paste of the same content reveals the body).
    /// Replace the entire buffer with `text` and move the cursor to
    /// the end. Used by `/fork` to restore the original user prompt
    /// into the editor for re-editing.
    pub fn set_text(&mut self, text: &str) {
        self.buffer = CompactString::new(text);
        self.cursor = self.buffer.len();
        self.pastes.clear();
        // Reset kill ring state so a subsequent yank doesn't paste
        // text from before the set_text (which would be jarring —
        // the editor was just rewritten by /fork). History position
        // also resets so Up/Down navigation starts from the new
        // baseline instead of mid-history. Drop the history draft
        // too — the user just got a /fork-restored prompt; that IS
        // the new draft.
        self.kill_ring.clear();
        self.yank_state = None;
        self.history_pos = None;
        self.history_draft = None;
        // /fork rewrites the editor wholesale — the prior buffer's
        // undo history no longer applies to this new draft.
        self.clear_undo();
    }

    /// Snapshot the editable state before an edit. `kind == Insert`
    /// coalesces with a preceding `Insert` so a typed word collapses
    /// into a single undo step; every other kind starts a new step.
    fn record_undo(
        &mut self,
        buffer: CompactString,
        cursor: usize,
        pastes: Vec<Option<PasteSlot>>,
        kind: EditKind,
    ) {
        let coalesce = matches!(kind, EditKind::Insert | EditKind::InsertBoundary)
            && self.last_edit_kind == Some(EditKind::Insert)
            && !self.undo_stack.is_empty();
        if !coalesce {
            self.undo_stack.push(UndoSnapshot {
                buffer,
                cursor,
                pastes,
            });
            if self.undo_stack.len() > UNDO_MAX {
                self.undo_stack.remove(0);
            }
        }
        // A whitespace insert (or any non-Insert edit) closes the
        // current run so the next character starts a fresh undo group.
        self.last_edit_kind = match kind {
            EditKind::Insert => Some(EditKind::Insert),
            EditKind::InsertBoundary | EditKind::Other => None,
        };
    }

    /// Restore the most recent pre-edit snapshot. No-op when nothing
    /// has been edited since the last submit / fork.
    fn undo(&mut self) {
        if let Some(snap) = self.undo_stack.pop() {
            self.buffer = snap.buffer;
            self.cursor = snap.cursor.min(self.buffer.len());
            self.pastes = snap.pastes;
            self.last_edit_kind = None;
            self.history_pos = None;
            self.reset_kill_accumulation();
        }
    }

    fn clear_undo(&mut self) {
        self.undo_stack.clear();
        self.last_edit_kind = None;
    }

    pub fn handle_paste(&mut self, text: &str) {
        let pre_buffer = self.buffer.clone();
        let pre_cursor = self.cursor;
        let pre_pastes = self.pastes.clone();
        self.handle_paste_inner(text);
        if self.buffer != pre_buffer || self.pastes != pre_pastes {
            self.record_undo(pre_buffer, pre_cursor, pre_pastes, EditKind::Other);
        }
    }

    /// Insert a pasted image as a `[image]` placeholder slot. The bytes
    /// stay in the slot until [`submission`]; on send they're persisted
    /// to the session asset dir and attached to the prompt. Ignored
    /// while the file picker is active (same guard as text paste).
    pub fn handle_paste_image(&mut self, image: PendingImage) {
        if self.picker.as_ref().is_some_and(|p| p.active) {
            return;
        }
        let pre_buffer = self.buffer.clone();
        let pre_cursor = self.cursor;
        let pre_pastes = self.pastes.clone();
        let idx = self.pastes.len();
        self.pastes.push(Some(PasteSlot::Image(image)));
        let marker = format!("{}{}{}", PASTE_MARK, idx, PASTE_MARK);
        self.insert_str(&marker);
        if self.buffer != pre_buffer || self.pastes != pre_pastes {
            self.record_undo(pre_buffer, pre_cursor, pre_pastes, EditKind::Other);
        }
    }

    fn handle_paste_inner(&mut self, text: &str) {
        // The file picker (`@query`) maintains its own filter state. A paste
        // landing here would write marker bytes into the buffer that the
        // picker doesn't know about, leaving a stale/corrupt query. Easiest
        // to just ignore pastes while the picker is active — the user can
        // close the picker (Esc) and re-paste.
        if self.picker.as_ref().is_some_and(|p| p.active) {
            return;
        }
        // Normalize line endings to `\n`. macOS-era clipboards and some
        // terminal paste streams deliver `\r` or `\r\n`. Without this the
        // line count comes out as 1, the collapse threshold isn't reached,
        // and the raw text gets inserted — with embedded `\r` chars that
        // the terminal then renders as carriage-returns, garbling the line.
        let normalized: String = text.replace("\r\n", "\n").replace('\r', "\n");
        // Strip PASTE_MARK so it can never appear in paste content and confuse
        // the marker parser.
        let cleaned: String = normalized.chars().filter(|&c| c != PASTE_MARK).collect();
        if cleaned.is_empty() {
            return;
        }
        // UI-6: reject pastes over ~1MB. Multi-MB pastes accumulate in
        // the buffer, allocate for line wrapping, and bloat re-renders.
        // The terminal is for text, not binary blobs — truncate with a
        // visible warning so the user knows.
        const MAX_PASTE_BYTES: usize = 1_048_576; // 1 MB
        if cleaned.len() > MAX_PASTE_BYTES {
            // dirge-n9c7: cut at the last char boundary at or below the BYTE
            // cap. The old `chars().take(MAX_PASTE_BYTES)` counted characters,
            // so a non-ASCII paste kept up to ~4x the limit and the notice
            // (charging `.len()`, bytes) mislabeled it as chars.
            let cut = crate::text::char_boundary_at_or_before(&cleaned, MAX_PASTE_BYTES);
            self.insert_str(&cleaned[..cut]);
            // Append a truncation notice so it's clear the paste was cut.
            self.insert_str(&format!(
                "\n\n[paste truncated: {} bytes → 1 MB limit]",
                cleaned.len()
            ));
            return;
        }
        let line_count = cleaned.matches('\n').count() + 1;
        if line_count < PASTE_COLLAPSE_LINES {
            self.insert_str(&cleaned);
            return;
        }
        // Auto-expand on repeat: if this body matches an existing placeholder
        // in the buffer, expand it inline rather than inserting another
        // placeholder.
        if let Some((start, end, idx)) =
            marker_blocks(&self.buffer).into_iter().find(|(_, _, idx)| {
                self.pastes
                    .get(*idx)
                    .and_then(|opt| opt.as_ref())
                    // Only text slots auto-expand; image slots have no
                    // inline text form.
                    .and_then(|slot| match slot {
                        PasteSlot::Text(s) => Some(s.as_str() == cleaned.as_str()),
                        PasteSlot::Image(_) => None,
                    })
                    .unwrap_or(false)
            })
        {
            if let Some(PasteSlot::Text(body)) = self.pastes[idx].take() {
                self.buffer.replace_range(start..end, body.as_str());
                // Place cursor at end of expanded text.
                self.cursor = start + body.len();
            }
            self.history_pos = None;
            self.reset_kill_accumulation();
            return;
        }
        let idx = self.pastes.len();
        self.pastes
            .push(Some(PasteSlot::Text(CompactString::from(cleaned))));
        let marker = format!("{}{}{}", PASTE_MARK, idx, PASTE_MARK);
        self.insert_str(&marker);
    }

    fn insert_str(&mut self, s: &str) {
        self.buffer.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.history_pos = None;
        self.reset_kill_accumulation();
    }

    /// Remove a byte range from the buffer and place the cursor at `start`.
    /// If the range fully contains a placeholder marker block, the
    /// corresponding `pastes` slot is tombstoned so its body can be GC'd
    /// (idempotent — repeat removes are fine).
    fn remove_range(&mut self, start: usize, end: usize) {
        // Detect any marker block fully contained in the removed range and
        // free its stored body.
        for (mstart, mend, idx) in marker_blocks(&self.buffer) {
            if mstart >= start
                && mend <= end
                && let Some(slot) = self.pastes.get_mut(idx)
            {
                *slot = None;
            }
        }
        self.buffer.replace_range(start..end, "");
        self.cursor = start;
    }

    /// Return the buffer with all placeholder markers expanded to their
    /// original paste bodies. Used at submit time so the agent receives the
    /// real text.
    pub fn expanded(&self) -> CompactString {
        Self::expand_with_pastes(&self.buffer, &self.pastes).into()
    }

    /// Render the buffer for the input box display: each
    /// `\x01<idx>\x01` marker block is replaced with the
    /// `[N lines pasted]` placeholder so the user sees a compact
    /// representation rather than a bare digit between invisible
    /// SOH bytes. Returns `(display_text, cursor_byte_in_display)`
    /// so the renderer can place the cursor correctly.
    ///
    /// Cursor mapping: marker blocks are atomic for cursor motion
    /// (see `prev_pos` / `next_pos`), so the cursor only ever sits
    /// at the open boundary, the close boundary, or outside any
    /// marker. For each marker block whose close is at or before
    /// the cursor, shift the displayed cursor by
    /// `placeholder.len() - marker_len`.
    pub fn display(&self) -> (String, usize) {
        let buf = self.buffer.as_str();
        let blocks = marker_blocks(buf);
        if blocks.is_empty() {
            return (buf.to_string(), self.cursor.min(buf.len()));
        }
        let cursor = self.cursor.min(buf.len());
        let mut out = String::with_capacity(buf.len());
        let mut cursor_display = cursor;
        let mut last_end = 0usize;
        for (start, end, idx) in &blocks {
            out.push_str(&buf[last_end..*start]);
            let placeholder = self
                .pastes
                .get(*idx)
                .and_then(|o| o.as_ref())
                .map(|slot| match slot {
                    PasteSlot::Text(s) => placeholder_display(s.as_str()),
                    PasteSlot::Image(_) => "[image]".to_string(),
                })
                .unwrap_or_default();
            let marker_len = end - start;
            if cursor >= *end {
                // Cursor is past the marker — shift by delta.
                cursor_display = cursor_display
                    .saturating_sub(marker_len)
                    .saturating_add(placeholder.len());
            } else if cursor > *start {
                // Defensive: cursor inside the marker (shouldn't
                // happen via normal motion). Clamp to end of
                // placeholder in the display.
                cursor_display = out.len() + placeholder.len();
            }
            out.push_str(&placeholder);
            last_end = *end;
        }
        out.push_str(&buf[last_end..]);
        (out, cursor_display)
    }

    /// Expand markers in `s` using `pastes` for bodies. Free-function form
    /// so it can also be used to flatten markers in kill-ring entries
    /// before we clear `pastes`.
    fn expand_with_pastes(s: &str, pastes: &[Option<PasteSlot>]) -> String {
        let blocks = marker_blocks(s);
        if blocks.is_empty() {
            return s.to_string();
        }
        let mut out = String::with_capacity(s.len());
        let mut cur = 0;
        for (start, end, idx) in blocks {
            out.push_str(&s[cur..start]);
            // Image slots have no inline text form — they're attached
            // as separate parts at submit, so they vanish from the
            // flattened text (the marker is simply dropped here).
            if let Some(Some(PasteSlot::Text(body))) = pastes.get(idx) {
                out.push_str(body);
            }
            cur = end;
        }
        out.push_str(&s[cur..]);
        out
    }

    /// Return (display_text, display_cursor_col) for a logical line of the
    /// buffer with placeholders rendered as `[N lines pasted]`. Used by the
    /// renderer so the input bar shows a compact representation.
    #[allow(dead_code)]
    pub fn render_line(&self, line: &str, cursor_in_line: usize) -> (String, usize) {
        let blocks = marker_blocks(line);
        if blocks.is_empty() {
            return (line.to_string(), cursor_in_line);
        }
        let mut out = String::with_capacity(line.len());
        let mut display_cursor = cursor_in_line;
        let mut cur = 0;
        for (start, end, idx) in blocks {
            // Carry plain text before the block.
            if cur < start {
                out.push_str(&line[cur..start]);
            }
            let placeholder = self
                .pastes
                .get(idx)
                .and_then(|o| o.as_ref())
                .map(|slot| match slot {
                    PasteSlot::Text(s) => placeholder_display(s.as_str()),
                    PasteSlot::Image(_) => "[image]".to_string(),
                })
                .unwrap_or_else(|| "[expanded]".to_string());
            // Adjust the displayed cursor position if it lies after this block.
            if cursor_in_line >= end {
                let block_len = end - start;
                display_cursor = display_cursor - block_len + placeholder.len();
            } else if cursor_in_line > start && cursor_in_line < end {
                // Cursor logically inside a marker — pin it to the placeholder
                // boundary so it never appears mid-marker.
                display_cursor = out.len() + placeholder.len();
            }
            out.push_str(&placeholder);
            cur = end;
        }
        if cur < line.len() {
            out.push_str(&line[cur..]);
        }
        (out, display_cursor)
    }

    pub fn start_picker(&mut self) {
        let picker = self.picker.get_or_insert_with(FilePicker::new);
        picker.activate();
    }

    fn reset_kill_accumulation(&mut self) {
        self.last_action_was_kill = false;
        self.yank_state = None;
        #[cfg(feature = "slash-completion")]
        {
            self.completion = None;
        }
    }

    fn push_kill(&mut self, text: CompactString, direction: KillDir) {
        if text.is_empty() {
            return;
        }
        // dirge-wncc: a real kill mutates the buffer, invalidating the byte
        // offsets a prior Yank recorded in `yank_state`. Clear it so a
        // following YankPop can't operate on a stale range (emacs
        // semantics: yank-pop only follows a yank / yank-pop). Unlike the
        // other buffer mutations, the kill actions route through here and
        // never call `reset_kill_accumulation`.
        self.yank_state = None;
        if self.last_action_was_kill && !self.kill_ring.is_empty() {
            let entry = &mut self.kill_ring[0];
            match direction {
                KillDir::Prepend => {
                    let mut new = text;
                    new.push_str(entry);
                    *entry = new;
                }
                KillDir::Append => {
                    entry.push_str(&text);
                }
            }
        } else {
            self.kill_ring.insert(0, text);
            if self.kill_ring.len() > KILL_RING_MAX {
                self.kill_ring.pop();
            }
        }
        self.last_action_was_kill = true;
    }

    pub fn handle_picker_key(&mut self, key: KeyEvent) -> bool {
        // Same AltGr fold as `handle_key` (gh-659): keep the file-picker
        // query typable with `@`-layout special chars.
        let key = normalize_altgr(key);
        let picker = match self.picker.as_mut() {
            Some(p) if p.active => p,
            _ => return false,
        };

        match key.code {
            KeyCode::Char(c)
                if c == '\x08' || (c == 'h' && key.modifiers.contains(KeyModifiers::CONTROL)) =>
            {
                if picker.cursor > 0 {
                    picker.backspace();
                    self.cursor = prev_char_boundary(&self.buffer, self.cursor);
                    self.buffer.remove(self.cursor);
                } else {
                    // `rfind` returns a *byte* offset and `self.cursor`
                    // is a byte offset (see line below where we add
                    // `c.len_utf8()`). The previous version mixed byte
                    // offsets with `chars().take(N)` which counts chars
                    // — corrupted any buffer containing multi-byte
                    // text before the `@`. Use byte-level slicing
                    // throughout.
                    if let Some(at) = self.buffer.rfind('@') {
                        let before = &self.buffer[..at];
                        let after = self.buffer.get(at + 1..).unwrap_or("");
                        let new_buf = format!("{}{}", before, after);
                        self.cursor = at;
                        self.buffer = new_buf.into();
                    }
                    picker.deactivate();
                }
                true
            }
            KeyCode::Char(c) => {
                picker.char_input(c);
                self.buffer.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                true
            }
            KeyCode::Backspace => {
                if picker.cursor > 0 {
                    picker.backspace();
                    self.cursor = prev_char_boundary(&self.buffer, self.cursor);
                    self.buffer.remove(self.cursor);
                    true
                } else {
                    // Same byte-vs-char fix as the Esc branch above.
                    if let Some(at) = self.buffer.rfind('@') {
                        let before = &self.buffer[..at];
                        let after = self.buffer.get(at + 1..).unwrap_or("");
                        let new_buf = format!("{}{}", before, after);
                        self.cursor = at;
                        self.buffer = new_buf.into();
                    }
                    picker.deactivate();
                    true
                }
            }
            KeyCode::Tab => {
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::SHIFT)
                {
                    picker.select_prev();
                } else {
                    picker.select_next();
                }
                true
            }
            KeyCode::Up => {
                picker.select_prev();
                true
            }
            KeyCode::Down => {
                picker.select_next();
                true
            }
            KeyCode::Enter => {
                if let Some(path) = picker.selected_path() {
                    let path_str = path.to_string_lossy().to_string();
                    // Byte-level slicing — `rfind`, `picker.query.len()`,
                    // and `self.cursor` are all byte offsets. Previous
                    // version mixed byte indices with `chars()` iters
                    // and corrupted the buffer on multi-byte input.
                    if let Some(at) = self.buffer.rfind('@') {
                        let before = &self.buffer[..at];
                        let after_byte = at + 1 + picker.query.len();
                        let after = self.buffer.get(after_byte..).unwrap_or("");
                        let new_cursor = before.len() + path_str.len();
                        let new_buf = format!("{}{}{}", before, path_str, after);
                        self.cursor = new_cursor;
                        self.buffer = new_buf.into();
                    }
                }
                picker.deactivate();
                true
            }
            KeyCode::Esc => {
                // Use BYTE-level slicing here, matching the Enter
                // path above. `rfind('@')` returns a byte offset;
                // the previous implementation used `chars().take(at)`
                // and `chars().skip(at + ...)` which mixed byte
                // offsets with char counts and corrupted the buffer
                // for any input containing multi-byte UTF-8 chars
                // before the `@` (accented letters, emoji, CJK, …).
                if let Some(at) = self.buffer.rfind('@') {
                    let before = &self.buffer[..at];
                    let after_byte = at + 1 + picker.query.len();
                    let after = self.buffer.get(after_byte..).unwrap_or("");
                    let new_buf = format!("{}{}", before, after);
                    self.cursor = at;
                    self.buffer = new_buf.into();
                }
                picker.deactivate();
                true
            }
            _ => false,
        }
    }

    /// Check if this key resolves to the external editor action.
    /// Used by the event loop to pre-emptively suspend the TUI before
    /// calling `handle_key()`, so the input reader thread is stopped
    /// before the editor spawns.
    #[cfg(unix)]
    pub fn is_external_editor_key(&self, key: &KeyEvent) -> bool {
        self.keymap.resolve_lenient(key) == Some(InputAction::ExternalEditor)
    }

    /// Spawn $EDITOR with the current buffer in a temporary file.
    /// On successful exit, replace the buffer with the file contents.
    /// Errors are reported via the notification channel.
    ///
    /// NOTE: Caller MUST suspend the TUI (stop input reader, reset terminal)
    /// before calling this, and resume after. See
    /// `suspend_tui_for_subprocess` / `resume_tui_after_subprocess` in
    /// `terminal.rs`.
    #[cfg(unix)]
    pub(crate) fn open_in_external_editor(&mut self) -> Option<CompactString> {
        use std::io::Write;
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

        // Seed with the EXPANDED text (paste bodies inline, image markers
        // dropped) rather than the raw buffer: the raw buffer carries
        // invisible `\x01<idx>\x01` sentinel bytes that render as garbage in
        // the editor and are trivially corrupted, and its text-paste bodies
        // are collapsed to `[N lines pasted]` so they can't be edited at all
        // (dirge-vpma.5).
        let seed = self.editor_seed();

        let path = std::env::temp_dir().join(format!("dirge-input-{}.md", std::process::id()));
        // Clear any leftover from an interrupted prior edit, then create the
        // file with O_EXCL (`create_new`): if an attacker pre-planted a symlink
        // at this predictable path, the open fails safely instead of writing
        // through it.
        let _ = std::fs::remove_file(&path);
        let write_result = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|mut f| f.write_all(seed.as_bytes()));
        if let Err(e) = write_result {
            crate::ui::notifications::notify_send(crate::ui::notifications::Notification::Error(
                format!("External editor: failed to write temp file: {e}"),
            ));
            return None;
        }

        // dirge redirects fd 1/2 to the log file for the TUI session; point
        // them at /dev/tty for the child editor's lifetime so it draws on the
        // real terminal, then restore. fd 0 (stdin) is already the terminal.
        let saved: Option<(i32, i32, i32)> = unsafe {
            let tty = libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR);
            if tty < 0 {
                None
            } else {
                let so = libc::dup(1);
                let se = libc::dup(2);
                libc::dup2(tty, 1);
                libc::dup2(tty, 2);
                Some((tty, so, se))
            }
        };

        // git-style invocation: pass the temp path as a positional arg ($1 via
        // "$@") instead of interpolating it into the command string, so a path
        // with spaces/metacharacters can't break the command. `$EDITOR` still
        // word-splits (e.g. `EDITOR="code --wait"`).
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$@\""))
            .arg(&editor) // $0
            .arg(&path) // $1 → "$@"
            .status();

        // Restore fd 1/2 to the log file and close the saved/tty fds (no leak).
        if let Some((tty, so, se)) = saved {
            unsafe {
                libc::dup2(so, 1);
                libc::dup2(se, 2);
                libc::close(so);
                libc::close(se);
                libc::close(tty);
            }
        }

        let content = match status {
            Ok(s) if s.success() => std::fs::read_to_string(&path).unwrap_or_else(|_| seed.clone()),
            Ok(s) => {
                crate::ui::notifications::notify_send(
                    crate::ui::notifications::Notification::Error(format!(
                        "External editor exited with: {}",
                        s.code()
                            .map(|c| format!("code {c}"))
                            .unwrap_or_else(|| "signal".into())
                    )),
                );
                seed.clone()
            }
            Err(e) => {
                crate::ui::notifications::notify_send(
                    crate::ui::notifications::Notification::Error(format!(
                        "External editor: failed to spawn {editor}: {e}"
                    )),
                );
                seed.clone()
            }
        };

        let _ = std::fs::remove_file(&path);

        // Compare against the seed, not the raw buffer — the seed is what the
        // editor was handed, so this correctly detects "no change" even though
        // the raw buffer differs (markers expanded). On the error paths above
        // `content == seed`, so nothing is applied.
        if content != seed {
            self.apply_editor_result(&content);
        }

        None
    }

    /// Text handed to the external editor: the buffer with paste markers
    /// expanded to their bodies and image markers dropped. Same flattening
    /// as [`expanded`](Self::expanded) — named as a seam so the round-trip is
    /// testable and symmetric with [`apply_editor_result`](Self::apply_editor_result).
    #[cfg(unix)]
    fn editor_seed(&self) -> String {
        self.expanded().to_string()
    }

    /// Fold an external-editor result back into the buffer. The editor works
    /// on plain text (see [`editor_seed`](Self::editor_seed)), so text pastes
    /// return inline and need no slots — but pending IMAGES have no textual
    /// form and were dropped from the seed. Preserve them by re-attaching each
    /// as a fresh image placeholder appended after the edited text, so they
    /// still submit with the turn. Before dirge-vpma.5 this was a bare
    /// `set_text`, which cleared every paste slot and silently destroyed
    /// pasted images (and any un-expanded text bodies).
    #[cfg(unix)]
    fn apply_editor_result(&mut self, content: &str) {
        let images: Vec<PendingImage> = self
            .pastes
            .iter()
            .filter_map(|slot| match slot {
                Some(PasteSlot::Image(img)) => Some(img.clone()),
                _ => None,
            })
            .collect();
        // set_text clears pastes / kill ring / history draft / undo — right
        // here, since the editor rewrote the draft wholesale.
        self.set_text(content);
        for image in images {
            let idx = self.pastes.len();
            self.pastes.push(Some(PasteSlot::Image(image)));
            let marker = format!("{}{}{}", PASTE_MARK, idx, PASTE_MARK);
            self.insert_str(&marker);
        }
    }

    /// Apply a resolved rebindable editing command (dirge-8fkp). The
    /// bodies are the historical hardcoded `handle_key` arms moved behind
    /// the [`InputAction`] enum unchanged, so the default keymap reproduces
    /// the old behavior exactly.
    fn apply_input_action(&mut self, action: InputAction) -> Option<CompactString> {
        match action {
            InputAction::CursorLineStart => {
                self.cursor = cursor_line_start(&self.buffer, self.cursor);
                self.reset_kill_accumulation();
                None
            }
            InputAction::CursorLineEnd => {
                self.cursor = cursor_line_end(&self.buffer, self.cursor);
                self.reset_kill_accumulation();
                None
            }
            InputAction::CursorLeft => {
                if self.cursor > 0 {
                    self.cursor = prev_pos(&self.buffer, self.cursor);
                }
                self.reset_kill_accumulation();
                None
            }
            InputAction::CursorRight => {
                // At end-of-line with a slash-command ghost completion
                // showing, Right accepts it (fills in the suffix) instead
                // of just moving the cursor.
                #[cfg(feature = "slash-completion")]
                {
                    if self.cursor == self.buffer.len()
                        && let Some(suffix) = crate::ui::slash::ghost_suffix(self.buffer.as_str())
                    {
                        self.buffer.push_str(&suffix);
                        self.cursor = self.buffer.len();
                        self.reset_kill_accumulation();
                        return None;
                    }
                }
                if self.cursor < self.buffer.len() {
                    self.cursor = next_pos(&self.buffer, self.cursor);
                }
                self.reset_kill_accumulation();
                None
            }
            InputAction::WordLeft => {
                if self.cursor > 0 {
                    self.cursor = prev_word_pos(&self.buffer, self.cursor);
                }
                self.reset_kill_accumulation();
                None
            }
            InputAction::WordRight => {
                if self.cursor < self.buffer.len() {
                    self.cursor = next_word_pos(&self.buffer, self.cursor);
                } else {
                    self.cursor = self.buffer.len();
                }
                self.reset_kill_accumulation();
                None
            }
            InputAction::DeleteCharBack => {
                if let Some((start, end)) = backspace_range(&self.buffer, self.cursor) {
                    self.remove_range(start, end);
                }
                self.reset_kill_accumulation();
                None
            }
            InputAction::DeleteCharForward => {
                if let Some((start, end)) = delete_range(&self.buffer, self.cursor) {
                    self.remove_range(start, end);
                }
                self.reset_kill_accumulation();
                None
            }
            InputAction::KillToLineEnd => {
                let line_end = cursor_line_end(&self.buffer, self.cursor);
                if self.cursor < line_end {
                    let killed: CompactString = self.buffer[self.cursor..line_end].into();
                    self.buffer.replace_range(self.cursor..line_end, "");
                    self.push_kill(killed, KillDir::Append);
                }
                None
            }
            InputAction::KillToLineStart => {
                let line_start = cursor_line_start(&self.buffer, self.cursor);
                if self.cursor > line_start {
                    let killed: CompactString = self.buffer[line_start..self.cursor].into();
                    let after = &self.buffer[self.cursor..];
                    self.buffer = [&self.buffer[..line_start], after].concat().into();
                    self.cursor = line_start;
                    self.push_kill(killed, KillDir::Prepend);
                }
                None
            }
            InputAction::KillWordBack => {
                if self.cursor > 0 {
                    let start = prev_word_pos(&self.buffer, self.cursor);
                    let killed: CompactString = self.buffer[start..self.cursor].into();
                    self.buffer.replace_range(start..self.cursor, "");
                    self.cursor = start;
                    self.push_kill(killed, KillDir::Prepend);
                }
                None
            }
            InputAction::DeleteWordBack => {
                if self.cursor > 0 {
                    let start = prev_word_pos(&self.buffer, self.cursor);
                    self.buffer.replace_range(start..self.cursor, "");
                    self.cursor = start;
                }
                self.reset_kill_accumulation();
                None
            }
            InputAction::DeleteWordForward => {
                if self.cursor < self.buffer.len() {
                    let end = next_word_pos(&self.buffer, self.cursor);
                    self.buffer.replace_range(self.cursor..end, "");
                }
                self.reset_kill_accumulation();
                None
            }
            InputAction::Yank => {
                if let Some(text) = self.kill_ring.first() {
                    let text = text.clone();
                    let len = text.len();
                    self.buffer.insert_str(self.cursor, &text);
                    self.yank_state = Some(YankState {
                        index: 0,
                        cursor: self.cursor,
                        len,
                    });
                    self.cursor += len;
                }
                self.last_action_was_kill = false;
                None
            }
            InputAction::YankPop => {
                if let Some(ref state) = self.yank_state {
                    let range_end = state.cursor + state.len;
                    // dirge-wncc: verify the previously-yanked text is still
                    // intact at the recorded range before replacing it.
                    // `str::get` returns None if the range isn't on char
                    // boundaries (an intervening kill/insert shifted offsets),
                    // so this can't panic; the content check also prevents
                    // clobbering unrelated text that happens to sit at the
                    // same byte range. Was a bare `range_end <= len` byte check.
                    let intact = self
                        .buffer
                        .get(state.cursor..range_end)
                        .zip(self.kill_ring.get(state.index))
                        .is_some_and(|(slice, yanked)| slice == yanked.as_str());
                    if self.kill_ring.len() > 1 && intact {
                        let next = (state.index + 1) % self.kill_ring.len();
                        if let Some(text) = self.kill_ring.get(next) {
                            let text = text.clone();
                            self.buffer.replace_range(state.cursor..range_end, "");
                            self.buffer.insert_str(state.cursor, &text);
                            self.cursor = state.cursor + text.len();
                            self.yank_state = Some(YankState {
                                index: next,
                                cursor: state.cursor,
                                len: text.len(),
                            });
                        }
                    }
                }
                None
            }
            InputAction::HistoryPrev => {
                self.history_up();
                self.reset_kill_accumulation();
                None
            }
            InputAction::HistoryNext => {
                self.history_down();
                self.reset_kill_accumulation();
                None
            }
            InputAction::ReverseSearch => {
                self.enter_search();
                None
            }
            InputAction::LineUp => {
                self.reset_kill_accumulation();
                // If already navigating history, continue.
                if self.history_pos.is_some() {
                    self.history_up();
                    return None;
                }
                // Wrap-aware vertical motion: move by displayed (soft-
                // wrapped) rows when the width is known and the buffer
                // has no paste placeholders (display == raw). At the top
                // display row, fall through to history (dirge-5w9v).
                if self.wrap_w > 0 && marker_blocks(&self.buffer).is_empty() {
                    if let Some(pos) =
                        wrap_vertical_target(&self.buffer, self.cursor, self.wrap_w, true)
                    {
                        self.cursor = pos;
                    } else {
                        self.history_up();
                    }
                    return None;
                }
                // Fallback (markers present / width unknown): hard-newline
                // motion, then history.
                if let Some(pos) = prev_line_start(&self.buffer, self.cursor) {
                    let line_start = cursor_line_start(&self.buffer, self.cursor);
                    // H-batch1-2 (audit fix): map by CHAR column, not
                    // byte column. Adding `cursor - line_start` (a
                    // byte distance) to `pos` could land mid-codepoint
                    // when either line has multi-byte chars — the
                    // next `replace_range`/slice would panic with
                    // "byte index N is not a char boundary."
                    let char_col = self.buffer[line_start..self.cursor].chars().count();
                    let target_line_end = self.buffer[pos..]
                        .find('\n')
                        .map(|p| pos + p)
                        .unwrap_or(self.buffer.len());
                    self.cursor = byte_at_char_col(&self.buffer, pos, target_line_end, char_col);
                    return None;
                }
                // At top of buffer → fall through to history.
                self.history_up();
                None
            }
            InputAction::LineDown => {
                self.reset_kill_accumulation();
                // If already navigating history, continue.
                if self.history_pos.is_some() {
                    self.history_down();
                    return None;
                }
                // Wrap-aware vertical motion (see InputAction::LineUp). At the
                // bottom display row, fall through to history (dirge-5w9v).
                if self.wrap_w > 0 && marker_blocks(&self.buffer).is_empty() {
                    if let Some(pos) =
                        wrap_vertical_target(&self.buffer, self.cursor, self.wrap_w, false)
                    {
                        self.cursor = pos;
                    } else {
                        self.history_down();
                    }
                    return None;
                }
                // Fallback (markers present / width unknown): hard-newline
                // motion, then history.
                if let Some(pos) = next_line_start(&self.buffer, self.cursor) {
                    let line_start = cursor_line_start(&self.buffer, self.cursor);
                    // H-batch1-2 (audit fix) — see InputAction::LineUp.
                    let char_col = self.buffer[line_start..self.cursor].chars().count();
                    let target_line_end = self.buffer[pos..]
                        .find('\n')
                        .map(|p| pos + p)
                        .unwrap_or(self.buffer.len());
                    self.cursor = byte_at_char_col(&self.buffer, pos, target_line_end, char_col);
                    return None;
                }
                // At bottom of buffer → fall through to history.
                self.history_down();
                None
            }
            // Normally intercepted by `handle_key` before dispatch (so it
            // doesn't record itself as an edit); handled here too for
            // exhaustiveness and any direct-dispatch path.
            InputAction::Undo => {
                self.undo();
                None
            }
            InputAction::ExternalEditor => {
                #[cfg(unix)]
                {
                    self.open_in_external_editor()
                }
                #[cfg(not(unix))]
                {
                    None
                }
            }
        }
    }

    /// Public entry: dispatches the key, then maintains undo history
    /// around the edit (dirge-7yea). Undo (Ctrl+Z) is handled here so
    /// it can't record itself; reverse-i-search is excluded entirely
    /// (its buffer mirrors a transient history match, not an edit).
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<CompactString> {
        // Fold AltGr-composed chars (Ctrl+Alt+printable on Windows) back to
        // plain text before any dispatch, so `@ # [ ] { }` on non-US
        // layouts type and paste instead of being dropped (gh-659).
        let key = normalize_altgr(key);
        if self.search_mode {
            return self.handle_key_inner(key);
        }
        let action = self.keymap.resolve_lenient(&key);
        if matches!(action, Some(InputAction::Undo)) {
            self.undo();
            return None;
        }
        // History recall, wrap-aware line motion at a buffer edge, and
        // entering reverse-i-search all REPLACE the buffer without being
        // a user edit. Keep them off the undo stack so Ctrl+Z reverts
        // real edits, not navigation, and end the current typing run.
        if matches!(
            action,
            Some(
                InputAction::HistoryPrev
                    | InputAction::HistoryNext
                    | InputAction::LineUp
                    | InputAction::LineDown
                    | InputAction::ReverseSearch
            )
        ) {
            let submitted = self.handle_key_inner(key);
            self.last_edit_kind = None;
            return submitted;
        }
        // A plain non-whitespace character coalesces into the current
        // typing run; everything else (whitespace, paste, kill, motion)
        // is its own undo step.
        let kind = match key.code {
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if c.is_whitespace() {
                    EditKind::InsertBoundary
                } else {
                    EditKind::Insert
                }
            }
            _ => EditKind::Other,
        };
        let pre_buffer = self.buffer.clone();
        let pre_cursor = self.cursor;
        let pre_pastes = self.pastes.clone();
        let submitted = self.handle_key_inner(key);
        if self.search_mode {
            // The key just entered reverse-i-search; the buffer now
            // mirrors a history match, so leave the undo stack alone.
            return submitted;
        }
        if submitted.is_some() {
            // A submission resets the editor — prior edits aren't undoable.
            self.clear_undo();
        } else if self.buffer != pre_buffer || self.pastes != pre_pastes {
            self.record_undo(pre_buffer, pre_cursor, pre_pastes, kind);
        } else {
            // A non-editing key ends the current typing run so the next
            // character starts a fresh undo group.
            self.last_edit_kind = None;
        }
        submitted
    }

    fn handle_key_inner(&mut self, key: KeyEvent) -> Option<CompactString> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // Ctrl+J is a portable newline trigger — many terminals never send
        // Shift+Enter as a distinct keystroke, but Ctrl+J always arrives.
        // Handled here before the Enter arm so it works even when the
        // terminal collapses Ctrl+J onto KeyCode::Enter. NOT in reverse-i-search
        // — there `self.buffer` holds the displayed match, so inserting a raw
        // `\n` would corrupt it and desync the search; let search-mode handle it.
        if ctrl && matches!(key.code, KeyCode::Char('j')) && !self.search_mode {
            if !self.picker.as_ref().is_some_and(|p| p.active) {
                self.buffer.insert(self.cursor, '\n');
                self.cursor += 1;
                self.history_pos = None;
                self.reset_kill_accumulation();
            }
            return None;
        }

        // ── search-mode dispatch ───────────────────────────
        if self.search_mode {
            return self.handle_search_key(key);
        }

        // Rebindable editing commands (cursor/word motion, kill-ring, yank,
        // history) resolve through the input keymap (dirge-8fkp). Intrinsic
        // keys — char insertion, Enter, Backspace, Delete, Tab — are NOT
        // rebindable and fall through to the match below.
        if let Some(action) = self.keymap.resolve_lenient(&key) {
            return self.apply_input_action(action);
        }

        match key.code {
            KeyCode::Enter => {
                if self.picker.as_ref().is_some_and(|p| p.active) {
                    return None;
                }
                // Meta+Enter or Shift+Enter inserts newline
                if has_shift || alt {
                    self.buffer.insert(self.cursor, '\n');
                    self.cursor += 1;
                    self.history_pos = None;
                    return None;
                }
                // Plain Enter → submit. Expand any paste placeholders so the
                // agent receives the original text. Store the expanded form in
                // history too — history navigation can't rely on paste-index
                // continuity across turns.
                let submitted = self.expanded();
                if !submitted.is_empty() {
                    // Dedup against the most recent entry (bash/Emacs
                    // convention — pressing Enter on the same prompt
                    // twice shouldn't fill history with duplicates).
                    // Also cap history at 500 entries so a long-lived
                    // session doesn't grow it unboundedly.
                    const HISTORY_MAX: usize = 500;
                    let is_dupe = self
                        .history
                        .last()
                        .map(|prev| prev.as_str() == submitted.as_str())
                        .unwrap_or(false);
                    if !is_dupe {
                        self.history.push(submitted.clone());
                        if self.history.len() > HISTORY_MAX {
                            // Drop the oldest entries in batches so we
                            // aren't doing a shift on every submit
                            // once we hit the cap.
                            let drain_to = self.history.len() - HISTORY_MAX;
                            self.history.drain(..drain_to);
                        }
                    }
                }
                self.history_pos = None;
                self.history_draft = None;
                self.buffer.clear();
                self.cursor = 0;
                // Flatten markers in kill-ring entries to their raw bodies
                // before dropping pastes — otherwise a later Ctrl+Y would
                // yank back marker bytes referencing indices we just
                // cleared, and `expanded()` would silently omit them.
                for entry in self.kill_ring.iter_mut() {
                    if entry.contains(PASTE_MARK) {
                        let expanded = Self::expand_with_pastes(entry, &self.pastes);
                        *entry = expanded.into();
                    }
                }
                // Drain image slots out before clearing, so the caller
                // can persist + attach them to the prompt at send time.
                self.pending_images = self
                    .pastes
                    .iter()
                    .filter_map(|o| match o {
                        Some(PasteSlot::Image(i)) => Some(i.clone()),
                        _ => None,
                    })
                    .collect();
                self.pastes.clear();
                self.reset_kill_accumulation();
                if submitted.is_empty() && self.pending_images.is_empty() {
                    None
                } else {
                    Some(submitted)
                }
            }

            // Plain char: only if not ctrl/alt-modified
            KeyCode::Char(c) if !ctrl && !alt => {
                if c == '@' {
                    let at_word_start = self.cursor == 0
                        || self.buffer.as_bytes().get(self.cursor - 1) == Some(&b' ');
                    if at_word_start {
                        self.start_picker();
                    }
                }
                self.buffer.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.history_pos = None;
                self.reset_kill_accumulation();
                None
            }

            KeyCode::Backspace => {
                if let Some((start, end)) = backspace_range(&self.buffer, self.cursor) {
                    self.remove_range(start, end);
                }
                self.reset_kill_accumulation();
                None
            }

            KeyCode::Delete => {
                if let Some((start, end)) = delete_range(&self.buffer, self.cursor) {
                    self.remove_range(start, end);
                }
                self.reset_kill_accumulation();
                None
            }

            KeyCode::Tab => {
                #[cfg(feature = "slash-completion")]
                {
                    // Don't grab Tab while the `@`-file-picker is
                    // active — that path has its own keystroke
                    // handling. The buffer wouldn't start with `/`
                    // in practice, but guard explicitly so future
                    // changes can't accidentally race the picker —
                    // mirrors the Enter / Ctrl+J guards above.
                    let picker_active = self.picker.as_ref().is_some_and(|p| p.active);
                    if !picker_active
                        && self.buffer.starts_with('/')
                        && let Some(cr) = try_complete(&self.buffer, self.cursor)
                    {
                        self.buffer = cr.new_buffer.clone().into();
                        self.cursor = cr.new_cursor;
                        self.reset_kill_accumulation();
                        self.completion = Some(cr);
                        return None;
                    }
                }
                self.buffer.insert_str(self.cursor, "  ");
                self.cursor += 2;
                self.reset_kill_accumulation();
                None
            }

            _ => None,
        }
    }

    fn history_up(&mut self) {
        let hist_len = self.history.len();
        if hist_len == 0 {
            return;
        }
        // Stash whatever the user was typing when they first start
        // navigating history. Restored by `history_down` when they
        // pass back beyond the most-recent entry.
        if self.history_pos.is_none() {
            self.history_draft = Some((self.buffer.clone(), self.cursor));
        }
        let pos = match self.history_pos {
            Some(p) if p > 0 => p - 1,
            Some(_) => 0,
            None => hist_len - 1,
        };
        self.history_pos = Some(pos);
        self.buffer = self.history[pos].clone();
        self.cursor = self.buffer.len();
    }

    fn history_down(&mut self) {
        match self.history_pos {
            Some(pos) if pos + 1 < self.history.len() => {
                let new_pos = pos + 1;
                self.history_pos = Some(new_pos);
                self.buffer = self.history[new_pos].clone();
                self.cursor = self.buffer.len();
            }
            Some(_) => {
                // Past the most-recent history entry → return to
                // whatever the user was typing before they started
                // browsing history (empty if there was no draft).
                self.history_pos = None;
                if let Some((draft, cursor)) = self.history_draft.take() {
                    self.buffer = draft;
                    self.cursor = cursor.min(self.buffer.len());
                } else {
                    self.buffer.clear();
                    self.cursor = 0;
                }
            }
            None => {}
        }
    }

    // ── Ctrl+F reverse-i-search ─────────────────────────────

    pub fn is_in_search(&self) -> bool {
        self.search_mode
    }

    #[allow(dead_code)]
    pub fn search_query(&self) -> &str {
        self.search_query.as_str()
    }

    #[allow(dead_code)]
    pub fn search_match_text(&self) -> &str {
        match self.search_match_idx {
            Some(idx) => self.history.get(idx).map(|s| s.as_str()).unwrap_or(""),
            None => "",
        }
    }

    pub fn search_display(&self) -> (String, usize) {
        // Route through display() so paste markers are collapsed
        // to placeholders (same as the normal editor buffer).
        let (matched, matched_cursor) = self.display();
        let prefix = format!("(reverse-i-search)`{}': ", self.search_query);
        let full = format!("{}{}", prefix, matched);
        (full, prefix.len() + matched_cursor)
    }

    fn enter_search(&mut self) {
        if self.history.is_empty() {
            return;
        }
        self.search_draft = Some((self.buffer.clone(), self.cursor));
        self.search_mode = true;
        self.search_query.clear();
        self.search_match_idx = Some(self.history.len() - 1);
        self.buffer = self.history[self.history.len() - 1].clone();
        self.cursor = self.buffer.len();
    }

    fn search_find(&self, query: &str) -> Option<usize> {
        if query.is_empty() {
            if self.history.is_empty() {
                return None;
            }
            return Some(self.history.len() - 1);
        }
        let lower = query.to_lowercase();
        for (i, entry) in self.history.iter().enumerate().rev() {
            let entry_lower = entry.to_lowercase();
            if entry_lower.contains(lower.as_str()) {
                return Some(i);
            }
        }
        None
    }

    /// Narrow from the current match position backward so typing
    /// after cycling doesn't teleport to the newest match.
    fn search_refine(&self, query: &str) -> Option<usize> {
        if query.is_empty() {
            if self.history.is_empty() {
                return None;
            }
            return Some(self.history.len() - 1);
        }
        let start = self.search_match_idx.unwrap_or(self.history.len() - 1);
        let lower = query.to_lowercase();
        for (i, entry) in self.history[..=start].iter().enumerate().rev() {
            if entry.to_lowercase().contains(lower.as_str()) {
                return Some(i);
            }
        }
        None
    }

    fn search_cycle_next(&mut self) {
        let query = self.search_query.clone();
        let start = self.search_match_idx.unwrap_or(self.history.len());
        let lower = query.to_lowercase();
        let next = if start == 0 {
            self.history
                .iter()
                .enumerate()
                .rev()
                .find(|(_, entry)| {
                    if query.is_empty() {
                        true
                    } else {
                        entry.to_lowercase().contains(lower.as_str())
                    }
                })
                .map(|(i, _)| i)
        } else {
            let range = &self.history[..start];
            range
                .iter()
                .enumerate()
                .rev()
                .find(|(_, entry)| {
                    if query.is_empty() {
                        true
                    } else {
                        entry.to_lowercase().contains(lower.as_str())
                    }
                })
                .map(|(i, _)| i)
                .or_else(|| {
                    self.history
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, entry)| {
                            if query.is_empty() {
                                true
                            } else {
                                entry.to_lowercase().contains(lower.as_str())
                            }
                        })
                        .map(|(i, _)| i)
                })
        };
        if let Some(idx) = next {
            self.search_match_idx = Some(idx);
            self.buffer = self.history[idx].clone();
            self.cursor = self.buffer.len();
        }
    }

    fn exit_search_accept(&mut self) {
        if self.search_match_idx.is_none()
            && let Some((draft, cursor)) = self.search_draft.take()
        {
            self.buffer = draft;
            self.cursor = cursor.min(self.buffer.len());
        }
        self.search_mode = false;
        self.search_query.clear();
        self.search_match_idx = None;
        self.search_draft = None;
    }

    fn exit_search_cancel(&mut self) {
        self.search_mode = false;
        self.search_query.clear();
        self.search_match_idx = None;
        if let Some((draft, cursor)) = self.search_draft.take() {
            self.buffer = draft;
            self.cursor = cursor.min(self.buffer.len());
        } else {
            self.buffer.clear();
            self.cursor = 0;
        }
    }

    pub fn cancel_search(&mut self) {
        self.exit_search_cancel();
    }

    pub fn load_history_entry(&mut self, content: &str) {
        if content.is_empty() {
            return;
        }
        let is_dupe = self
            .history
            .last()
            .map(|prev| prev.as_str() == content)
            .unwrap_or(false);
        if !is_dupe {
            self.history.push(CompactString::new(content));
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Option<CompactString> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Char('f') if ctrl => {
                self.search_cycle_next();
                None
            }

            KeyCode::Char('c') if ctrl => {
                self.exit_search_cancel();
                None
            }
            KeyCode::Esc => {
                self.exit_search_cancel();
                None
            }

            KeyCode::Enter => {
                self.exit_search_accept();
                None
            }

            KeyCode::Backspace => {
                if !self.search_query.is_empty() {
                    self.search_query.pop();
                }
                // Widening (shorter query) — find newest match.
                if let Some(idx) = self.search_find(&self.search_query) {
                    self.search_match_idx = Some(idx);
                    self.buffer = self.history[idx].clone();
                    self.cursor = self.buffer.len();
                } else {
                    self.search_match_idx = None;
                    self.buffer.clear();
                    self.cursor = 0;
                }
                None
            }

            KeyCode::Char(c) if !ctrl => {
                self.search_query.push(c);
                if let Some(idx) = self.search_refine(&self.search_query) {
                    self.search_match_idx = Some(idx);
                    self.buffer = self.history[idx].clone();
                    self.cursor = self.buffer.len();
                } else {
                    self.search_match_idx = None;
                    self.buffer.clear();
                    self.cursor = 0;
                }
                None
            }

            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // dirge-8fkp: input keys now dispatch through the InputKeymap. These
    // pin that the default chords still drive the historical editing
    // actions through handle_key, and that remapping the keymap reroutes a
    // chord (the whole point of the refactor).

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn ctrl_a_e_move_to_line_bounds_via_keymap() {
        let mut e = InputEditor::new();
        e.insert_str("hello world");
        e.handle_key(ev(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(e.cursor, 0, "Ctrl+A → line start");
        e.handle_key(ev(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(e.cursor, e.buffer.len(), "Ctrl+E → line end");
        // Home/End are the same actions on different chords.
        e.handle_key(ev(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(e.cursor, 0);
        e.handle_key(ev(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(e.cursor, e.buffer.len());
    }

    #[test]
    fn ctrl_k_w_kill_through_keymap() {
        let mut e = InputEditor::new();
        e.insert_str("foo bar baz");
        e.handle_key(ev(KeyCode::Char('a'), KeyModifiers::CONTROL)); // to start
        e.handle_key(ev(KeyCode::Char('k'), KeyModifiers::CONTROL)); // kill to end
        assert_eq!(e.buffer.as_str(), "");
        e.handle_key(ev(KeyCode::Char('y'), KeyModifiers::CONTROL)); // yank it back
        assert_eq!(e.buffer.as_str(), "foo bar baz");
        // Ctrl+W kills the word before the cursor (cursor at end).
        e.handle_key(ev(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(e.buffer.as_str(), "foo bar ");
    }

    /// dirge-vpma.16: Ctrl+K kills to end of the current LINE, not the
    /// end of the whole buffer; the yanked text is available via Ctrl+Y.
    #[test]
    fn kill_to_line_end_stops_at_newline() {
        let mut e = InputEditor::new();
        e.insert_str("line1\nline2\nline3");
        e.cursor = 3; // middle of line 1 ("lin|e1")
        e.handle_key(ev(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(e.buffer.as_str(), "lin\nline2\nline3");
        e.handle_key(ev(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(e.buffer.as_str(), "line1\nline2\nline3");
    }

    /// dirge-wncc: a kill (Ctrl+W here) after a yank mutates the buffer
    /// but went through `push_kill`, which did NOT clear `yank_state`.
    /// A later YankPop then operated on stale byte offsets. push_kill now
    /// clears yank_state, so YankPop after a kill is inert (emacs
    /// semantics: yank-pop only follows a yank / yank-pop).
    #[test]
    fn kill_after_yank_clears_yank_state() {
        let mut e = InputEditor::new();
        e.insert_str("alpha");
        e.handle_key(ev(KeyCode::Char('a'), KeyModifiers::CONTROL)); // to start
        e.handle_key(ev(KeyCode::Char('k'), KeyModifiers::CONTROL)); // kill "alpha"
        e.handle_key(ev(KeyCode::Char('y'), KeyModifiers::CONTROL)); // yank it back
        assert!(e.yank_state.is_some(), "yank must set yank_state");
        e.handle_key(ev(KeyCode::Char('w'), KeyModifiers::CONTROL)); // kill word back
        assert!(
            e.yank_state.is_none(),
            "a kill after a yank must clear yank_state"
        );
        // A following YankPop is now a no-op (nothing to pop).
        e.handle_key(ev(KeyCode::Char('y'), KeyModifiers::ALT));
        assert_eq!(e.buffer.as_str(), "");
    }

    /// dirge-n9c7: the 1 MB paste cap is a BYTE cap, but truncation used to
    /// `chars().take(cap)` — so a multibyte paste kept up to ~4x the bytes
    /// (and the notice mislabeled the byte count as chars). Cut at the last
    /// char boundary at or below the byte cap instead.
    #[test]
    fn oversized_multibyte_paste_truncated_to_byte_cap() {
        const CAP: usize = 1_048_576; // must match MAX_PASTE_BYTES
        let mut e = InputEditor::new();
        // 400k × 3-byte '中' = 1.2 MB of bytes but only 400k chars, so the old
        // char-based take(CAP) kept the whole thing.
        let big = "中".repeat(400_000);
        assert!(big.len() > CAP);
        e.handle_paste(&big);
        let buf = e.buffer.as_str();
        let notice_at = buf.find("\n\n[paste truncated").expect("truncation notice");
        let content = &buf[..notice_at];
        assert!(
            content.len() <= CAP,
            "kept {} bytes, over the {CAP}-byte cap",
            content.len()
        );
        // Filled up to the cap (floored to a char boundary, so within 3 bytes).
        assert!(
            content.len() > CAP - 3,
            "under-filled: {} bytes",
            content.len()
        );
        assert!(
            buf.contains("bytes → 1 MB limit"),
            "notice should read bytes"
        );
    }

    /// dirge-wncc: even if a stale yank_state survives, YankPop must never
    /// panic or corrupt text — its guard verifies the recorded range is
    /// still on char boundaries AND still holds the yanked text.
    #[test]
    fn yankpop_with_stale_offsets_is_safe() {
        // Multibyte: range_end lands INSIDE 'é' (bytes 1..3). A raw
        // replace_range(0..2) would panic "not a char boundary".
        let mut e = InputEditor::new();
        e.kill_ring = vec!["hi".into(), "second".into()];
        e.buffer = "héllo".into();
        e.cursor = 0;
        e.yank_state = Some(YankState {
            index: 0,
            cursor: 0,
            len: 2,
        });
        e.handle_key(ev(KeyCode::Char('y'), KeyModifiers::ALT)); // must not panic
        assert_eq!(
            e.buffer.as_str(),
            "héllo",
            "stale YankPop must not mutate the buffer"
        );

        // ASCII: boundaries valid but the recorded text no longer matches
        // what's there → must not silently clobber unrelated bytes.
        let mut e = InputEditor::new();
        e.kill_ring = vec!["hello".into(), "second".into()];
        e.buffer = "XXXXX".into();
        e.cursor = 0;
        e.yank_state = Some(YankState {
            index: 0,
            cursor: 0,
            len: 3,
        });
        e.handle_key(ev(KeyCode::Char('y'), KeyModifiers::ALT));
        assert_eq!(
            e.buffer.as_str(),
            "XXXXX",
            "stale YankPop must not corrupt unrelated text"
        );
    }

    #[test]
    fn plain_char_still_inserts_after_refactor() {
        let mut e = InputEditor::new();
        // A bare 'a' is intrinsic, not the Ctrl+A action — it must insert.
        e.handle_key(ev(KeyCode::Char('a'), KeyModifiers::NONE));
        e.handle_key(ev(KeyCode::Char('b'), KeyModifiers::NONE));
        assert_eq!(e.buffer.as_str(), "ab");
        assert_eq!(e.cursor, 2);
    }

    #[test]
    fn remapping_the_keymap_reroutes_a_chord() {
        let mut e = InputEditor::new();
        e.insert_str("hello world");
        // Rebind Ctrl+A to move to line END instead of start.
        e.keymap.insert(
            (KeyCode::Char('a'), KeyModifiers::CONTROL),
            InputAction::CursorLineEnd,
        );
        e.cursor = 3;
        e.handle_key(ev(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(e.cursor, e.buffer.len(), "Ctrl+A now goes to line end");
    }

    // dirge-5w9v: vertical motion moves by SOFT-wrapped display rows.
    // "abcdefghij" at wrap_w=4 wraps to ["abcd","efgh","ij"].
    #[test]
    fn wrap_vertical_target_moves_by_display_rows() {
        let s = "abcdefghij";
        let w = 4;
        // From end (row 2, col 2), Up lands on row 1 at the same column
        // ('g' = byte 6).
        assert_eq!(wrap_vertical_target(s, s.len(), w, true), Some(6));
        // From there (row 1, col 2), Down returns to row 2, column
        // clamped to its length (end = byte 10).
        assert_eq!(wrap_vertical_target(s, 6, w, false), Some(10));
        // Top display row → None (caller falls through to history).
        assert_eq!(wrap_vertical_target(s, 1, w, true), None);
        // Bottom display row → None.
        assert_eq!(wrap_vertical_target(s, 9, w, false), None);
    }

    #[test]
    fn test_prev_word_boundary_basic() {
        assert_eq!(prev_word_boundary("hello world", 11), 6);
        assert_eq!(prev_word_boundary("hello world", 6), 0);
        assert_eq!(prev_word_boundary("hello world", 5), 0);
    }

    #[test]
    fn test_prev_word_from_middle() {
        assert_eq!(prev_word_boundary("hello world", 9), 6); // middle of "world"
    }

    #[test]
    fn test_prev_word_at_start() {
        assert_eq!(prev_word_boundary("hello", 0), 0);
    }

    #[test]
    fn test_prev_word_punctuation() {
        assert_eq!(prev_word_boundary("foo.bar", 7), 4); // start of "bar"
        assert_eq!(prev_word_boundary("foo.bar", 4), 0); // start of "foo"
    }

    #[test]
    fn test_next_word_boundary_basic() {
        // "hello world foo" = 15 bytes
        assert_eq!(next_word_boundary("hello world foo", 0), 6);
        assert_eq!(next_word_boundary("hello world foo", 6), 12);
        assert_eq!(next_word_boundary("hello world foo", 12), 15);
    }

    #[test]
    fn test_next_word_at_end() {
        assert_eq!(next_word_boundary("hello", 5), 5);
    }

    #[test]
    fn test_next_word_punctuation() {
        // With updated logic: from start, skip "foo" + "." → land at "bar_baz" (byte 4)
        assert_eq!(next_word_boundary("foo.bar_baz", 0), 4);
        assert_eq!(next_word_boundary("foo.bar_baz", 3), 4); // from '.', skip it → byte 4
        assert_eq!(next_word_boundary("foo.bar_baz", 4), 11); // skip "bar_baz" → end
    }

    #[test]
    fn test_prev_word_multibyte() {
        // "hå bør": h(0) å(1,2→3) sp(3) b(4) ø(5,6→7) r(7→8)
        assert_eq!(prev_word_boundary("hå bør", 7), 4); // from after 'ø' → start of "bør" at 4
        assert_eq!(prev_word_boundary("hå bør", 4), 0); // from start of "bør" → start of "hå" at 0
    }

    #[test]
    fn test_next_word_multibyte() {
        // "hå bør": 8 bytes. h(0) å(1-2=3) sp(3) b(4) ø(5-6=7) r(7→8)
        assert_eq!(next_word_boundary("hå bør", 0), 4); // skip "hå ", land at "b" (byte 4)
        assert_eq!(next_word_boundary("hå bør", 4), 8); // skip "bør", land at end
    }

    #[test]
    fn test_cursor_line_start() {
        assert_eq!(cursor_line_start("hello\nworld", 10), 6);
        assert_eq!(cursor_line_start("hello\nworld", 3), 0);
        assert_eq!(cursor_line_start("single", 6), 0);
    }

    #[test]
    fn test_prev_line_start() {
        assert_eq!(prev_line_start("hello\nworld", 10), Some(0));
        assert_eq!(prev_line_start("hello\nworld", 3), None);
    }

    #[test]
    fn test_next_line_start() {
        assert_eq!(next_line_start("hello\nworld", 0), Some(6));
        assert_eq!(next_line_start("hello\nworld", 10), None);
    }

    // --- H-batch1-2: byte_at_char_col panic-defence ----------------

    #[test]
    fn byte_at_char_col_ascii_round_trip() {
        // "hello\nworld" — moving from col 3 on "world" (target line)
        let s = "hello\nworld";
        assert_eq!(byte_at_char_col(s, 6, 11, 3), 9);
    }

    #[test]
    fn byte_at_char_col_handles_multibyte_target() {
        // Source line has 4 ASCII chars; target line has 2 emoji
        // chars. Moving "col 3" to a 2-char target → clamp to end.
        let s = "abcd\n🦀🚀";
        // col 0 on "🦀🚀" lands at start byte (just after \n = 5)
        assert_eq!(byte_at_char_col(s, 5, s.len(), 0), 5);
        // col 1 = start of 🚀 (after 4-byte 🦀)
        assert_eq!(byte_at_char_col(s, 5, s.len(), 1), 9);
        // col 2 = end-of-line
        assert_eq!(byte_at_char_col(s, 5, s.len(), 2), s.len());
        // col 3 (past EOL) clamps to end
        assert_eq!(byte_at_char_col(s, 5, s.len(), 3), s.len());
    }

    /// History navigation must preserve the user's in-progress draft.
    /// Up stashes it; Down past the newest entry restores it.
    #[test]
    fn history_up_stashes_in_progress_draft() {
        let mut e = InputEditor::new();
        e.history.push("first".into());
        e.history.push("second".into());
        e.insert_str("working on this");
        let saved_cursor = e.cursor;

        e.history_up();
        assert_eq!(e.buffer.as_str(), "second");
        // The in-progress text is stashed, not lost.
        assert_eq!(
            e.history_draft.as_ref().map(|(s, c)| (s.as_str(), *c)),
            Some(("working on this", saved_cursor))
        );

        e.history_up();
        assert_eq!(e.buffer.as_str(), "first");

        e.history_down();
        assert_eq!(e.buffer.as_str(), "second");

        // Down past the most-recent entry restores the draft.
        e.history_down();
        assert_eq!(e.buffer.as_str(), "working on this");
        assert_eq!(e.cursor, saved_cursor);
        assert!(e.history_draft.is_none());
    }

    #[test]
    fn history_up_with_empty_draft_still_returns_empty_on_restore() {
        let mut e = InputEditor::new();
        e.history.push("only".into());

        e.history_up();
        assert_eq!(e.buffer.as_str(), "only");

        e.history_down();
        assert_eq!(e.buffer.as_str(), "");
        assert!(e.history_draft.is_none());
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn right_arrow_accepts_slash_ghost_completion() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut e = InputEditor::new();
        e.insert_str("/disp");
        let out = e.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(out.is_none());
        assert_eq!(e.buffer.as_str(), "/display");
        assert_eq!(e.cursor, e.buffer.len());
    }

    #[test]
    fn right_arrow_without_ghost_moves_cursor() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut e = InputEditor::new();
        e.insert_str("hello");
        e.cursor = 0; // not at end, and not a slash command
        e.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(e.cursor, 1);
        assert_eq!(e.buffer.as_str(), "hello");
    }

    #[test]
    fn history_down_with_no_navigation_is_noop() {
        let mut e = InputEditor::new();
        e.insert_str("draft");
        let before = e.buffer.clone();
        e.history_down();
        assert_eq!(e.buffer, before);
        assert!(e.history_draft.is_none());
    }

    #[test]
    fn set_text_clears_history_draft() {
        let mut e = InputEditor::new();
        e.history.push("h".into());
        e.insert_str("draft");
        e.history_up();
        assert!(e.history_draft.is_some());
        e.set_text("fork-restored");
        assert!(e.history_draft.is_none());
    }

    #[test]
    fn byte_at_char_col_handles_multibyte_source_for_history_up() {
        // Regression: source line had emoji; col was previously
        // counted in BYTES (4 per emoji). Moving up to a shorter
        // ASCII line landed mid-codepoint or past EOL of the
        // target — the next replace_range panicked.
        let s = "🦀🦀🦀\nabc";
        // The source line is "🦀🦀🦀" — 3 chars, 12 bytes. From
        // byte-cursor 12 (end of source), char_col should be 3.
        // Moving "up" to ... well there's no line above the source,
        // but as the target-side helper: col 3 on "abc" (3-char
        // line) lands at end (byte 3 relative to target start = 13).
        // Target line is &s[13..16] = "abc".
        assert_eq!(byte_at_char_col(s, 13, s.len(), 3), s.len());
    }

    /// User-reported bug: a multi-line paste inserted a marker like
    /// `\x01<idx>\x01` into the buffer; the renderer printed the raw
    /// bytes, so the user only saw the bare digit between two
    /// invisible SOH characters instead of the intended
    /// `[N lines pasted]` placeholder. `display()` is the fix —
    /// these tests pin the projection AND the cursor mapping.
    #[test]
    fn display_no_marker_passes_through() {
        let mut e = InputEditor::new();
        e.insert_str("hello world");
        e.cursor = 5;
        let (s, c) = e.display();
        assert_eq!(s, "hello world");
        assert_eq!(c, 5);
    }

    #[test]
    fn display_marker_renders_as_placeholder() {
        let mut e = InputEditor::new();
        // 5 lines triggers the collapse threshold.
        e.handle_paste("a\nb\nc\nd\ne");
        let (s, _) = e.display();
        assert_eq!(s, "[5 lines pasted]");
    }

    #[test]
    fn display_cursor_maps_to_end_of_placeholder() {
        let mut e = InputEditor::new();
        e.handle_paste("a\nb\nc\nd\ne");
        // handle_paste leaves the cursor at the end of the marker
        // (after the closing \x01). Verify display maps it to the
        // end of the placeholder string.
        let (s, c) = e.display();
        assert_eq!(s, "[5 lines pasted]");
        assert_eq!(c, s.len());
    }

    #[test]
    fn display_cursor_before_marker_unchanged() {
        let mut e = InputEditor::new();
        e.insert_str("pre ");
        e.handle_paste("a\nb\nc\nd\ne");
        // Cursor is currently at end (after the marker). Reset to
        // before "pre" and confirm the display cursor matches.
        e.cursor = 0;
        let (s, c) = e.display();
        assert!(
            s.starts_with("pre [") && s.contains("lines pasted]"),
            "got {s:?}",
        );
        assert_eq!(c, 0);
    }

    #[test]
    fn display_text_after_marker_offset_correctly() {
        let mut e = InputEditor::new();
        e.handle_paste("a\nb\nc\nd\ne");
        e.insert_str(" suffix");
        // Cursor sits at end of " suffix" — verify display maps it
        // to the end of `[N lines pasted] suffix`.
        let (s, c) = e.display();
        assert_eq!(s, "[5 lines pasted] suffix");
        assert_eq!(c, s.len());
    }

    // ── gh-659: AltGr-composed characters on Windows ──────────────────
    // Windows reports AltGr as Ctrl+Alt, so `@ # [ ] { }` on the Italian
    // (and German/Spanish/…) layouts reach the editor as `Char` events
    // with BOTH modifiers set. They must be inserted as text, not dropped
    // as (unbound) keybindings — and `@` must still open the file picker.

    #[test]
    fn altgr_at_sign_inserts_and_opens_picker() {
        let mut e = InputEditor::new();
        e.handle_key(ev(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(e.buffer.as_str(), "@");
        assert_eq!(e.cursor, 1);
        assert!(
            e.picker.as_ref().is_some_and(|p| p.active),
            "AltGr @ must open the @-file picker",
        );
    }

    #[test]
    fn altgr_special_chars_are_inserted() {
        for c in ['#', '[', ']', '{', '}'] {
            let mut e = InputEditor::new();
            e.handle_key(ev(
                KeyCode::Char(c),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ));
            assert_eq!(
                e.buffer.as_str(),
                c.to_string(),
                "AltGr {c:?} must be inserted as text",
            );
        }
    }

    #[test]
    fn altgr_char_with_shift_is_inserted() {
        // Some layouts compose via AltGr+Shift; the extra Shift must not
        // suppress the insert.
        let mut e = InputEditor::new();
        e.handle_key(ev(
            KeyCode::Char('{'),
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
        ));
        assert_eq!(e.buffer.as_str(), "{");
    }

    #[test]
    fn altgr_char_typed_into_active_picker_query() {
        // Once the picker is open, an AltGr char must extend the query
        // rather than be dropped.
        let mut e = InputEditor::new();
        e.handle_key(ev(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert!(e.picker.as_ref().is_some_and(|p| p.active));
        let consumed = e.handle_picker_key(ev(
            KeyCode::Char('#'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert!(consumed, "picker must consume the AltGr char");
        assert_eq!(e.buffer.as_str(), "@#");
    }

    #[test]
    fn ctrl_only_binding_still_fires_not_inserted() {
        // A single-modifier chord is a keybinding, NOT text: Ctrl+A moves
        // to line start and inserts nothing.
        let mut e = InputEditor::new();
        e.insert_str("hello");
        e.handle_key(ev(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(e.buffer.as_str(), "hello", "Ctrl+A must not insert 'a'");
        assert_eq!(e.cursor, 0, "Ctrl+A moves to line start");
    }

    #[test]
    fn alt_only_char_is_not_inserted_as_text() {
        // Alt+<char> is reserved for keybindings; an unbound Alt combo
        // must never land in the buffer as literal text.
        let mut e = InputEditor::new();
        e.handle_key(ev(KeyCode::Char('z'), KeyModifiers::ALT));
        assert_eq!(e.buffer.as_str(), "", "Alt+z must not insert 'z'");
    }

    // dirge-vpma.5: Ctrl+G (external editor) used to seed the temp file with
    // the raw buffer — invisible `\x01` marker bytes and collapsed paste
    // bodies — then fold the result back via a bare `set_text`, which cleared
    // every paste slot and silently destroyed pasted images.
    #[cfg(unix)]
    #[test]
    fn editor_seed_expands_text_and_drops_images() {
        let mut e = InputEditor::new();
        e.insert_str("before ");
        e.handle_paste("line1\nline2\nline3\nline4\nline5"); // collapses to a placeholder
        e.handle_paste_image(PendingImage {
            bytes: vec![1, 2, 3],
            media_type: "image/png".into(),
        });
        e.insert_str(" after");

        // Raw buffer carries the sentinel markers.
        assert!(e.buffer.as_str().contains(PASTE_MARK));

        let seed = e.editor_seed();
        // Text body is inline and editable; no sentinel bytes; image is gone
        // from the text (it has no textual form).
        assert!(seed.contains("line1\nline2\nline3\nline4\nline5"));
        assert!(
            !seed.contains(PASTE_MARK),
            "seed must not carry marker bytes"
        );
        assert!(seed.starts_with("before "));
        assert!(seed.ends_with(" after"));
    }

    #[cfg(unix)]
    #[test]
    fn apply_editor_result_preserves_pending_images() {
        let mut e = InputEditor::new();
        e.insert_str("hi ");
        e.handle_paste_image(PendingImage {
            bytes: vec![9, 8, 7],
            media_type: "image/png".into(),
        });

        // Simulate the editor returning rewritten text (no image markers —
        // the seed dropped them).
        e.apply_editor_result("totally new prompt");

        assert!(e.buffer.as_str().starts_with("totally new prompt"));
        // The image survived and re-submits with the turn.
        let submitted = e.handle_key(ev(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(submitted.as_deref(), Some("totally new prompt"));
        assert_eq!(
            e.pending_images.len(),
            1,
            "pasted image must survive Ctrl+G"
        );
        assert_eq!(e.pending_images[0].bytes, vec![9, 8, 7]);
    }
}
