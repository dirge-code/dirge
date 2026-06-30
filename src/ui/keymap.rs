//! Configurable key bindings for the global "command" keys (VSCode-style).
//!
//! The TUI's agent-control keys (toggle reasoning, scroll, chat
//! navigation, kill-subagent) resolve through a [`Keymap`] that maps a
//! key chord → [`KeyAction`]. Built-in defaults reproduce the historical
//! bindings; the user's `keybindings` config (an array of
//! `{ key, command }`) overrides them per chord, exactly like a VSCode
//! `keybindings.json`.
//!
//! The input-editor's text-editing keys (Ctrl+A/E/W, kill-ring, word
//! motion, history) resolve through the sibling [`InputKeymap`] over
//! [`InputAction`] — same mechanism, a separate context (it dispatches
//! inside the text box rather than at the chat level). Kept fixed in both:
//! the universal cancel/interrupt gesture (Ctrl+C / Ctrl+D / Esc) — the
//! panic button must always be available — and intrinsic editing (typing a
//! character, Backspace, Delete, Enter to submit, Tab completion).

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::KeybindingConfig;

/// A single key chord: a key plus its modifiers.
pub type Chord = (KeyCode, KeyModifiers);
/// A chord SEQUENCE — emacs-style multi-key bindings like `C-x C-s`
/// (dirge-xv9l makes the keymaps sequence-native; the multi-key matching
/// runtime lands in phase 4, dirge-fl57). Length 1 for an ordinary
/// single-key binding, which is all the built-in defaults use.
pub type ChordSeq = Vec<Chord>;

/// A rebindable command namespace (dirge-5kkx.3): the global [`KeyAction`]
/// keys and the input-editor [`InputAction`] keys both implement this, so
/// one generic [`Bindings`] keymap and the name-lookup helpers are written
/// once. The `ALL` table — `(action, config-command-name, default chords)`
/// — is the single source of truth each namespace provides.
pub trait Command: Copy + 'static {
    #[allow(clippy::type_complexity)]
    const ALL: &'static [(Self, &'static str, &'static [Chord])];

    /// Resolve a config command name (case-insensitive, `-`/`_` agnostic)
    /// to an action. `None` for unknown commands.
    fn from_command(name: &str) -> Option<Self> {
        let norm = name.trim().to_ascii_lowercase().replace('-', "_");
        Self::ALL
            .iter()
            .find(|(_, cmd, _)| *cmd == norm)
            .map(|(a, _, _)| *a)
    }

    /// Comma-separated list of every valid command name (help / warnings).
    fn command_list() -> String {
        Self::ALL
            .iter()
            .map(|(_, c, _)| *c)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A rebindable global command. Each maps to a stable `command` string
/// used in the config and to a set of default chords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    ToggleReasoning,
    /// Expand on demand: the buffered thinking (while the agent is
    /// thinking), else reprint the last collapsed tool result in full.
    Expand,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    NextChat,
    PrevChat,
    CloseChat,
    KillSubagent,
    /// Drop all queued mid-execution interjections without cancelling the
    /// running agent (dirge-e59d). Distinct from Ctrl+C, which cancels the
    /// run AND clears the queue.
    DropQueue,
    /// Cycle the active prompt layer to the next available prompt. Silent —
    /// updates the status-bar badge without writing to the chat log.
    CyclePrompt,
}

impl Command for KeyAction {
    const ALL: &'static [(KeyAction, &'static str, &'static [Chord])] = &[
        (
            KeyAction::ToggleReasoning,
            "toggle_reasoning",
            &[(KeyCode::Char('r'), KeyModifiers::CONTROL)],
        ),
        (
            KeyAction::Expand,
            "expand",
            &[(KeyCode::Char('o'), KeyModifiers::CONTROL)],
        ),
        (
            KeyAction::ScrollPageUp,
            "scroll_page_up",
            &[(KeyCode::PageUp, KeyModifiers::NONE)],
        ),
        (
            KeyAction::ScrollPageDown,
            "scroll_page_down",
            &[(KeyCode::PageDown, KeyModifiers::NONE)],
        ),
        // Ctrl+Home/End scroll the CHAT to its extremes. Bare Home/End are left
        // for the input editor (cursor to line start / end) — binding them to
        // scroll shadowed those editor handlers entirely.
        (
            KeyAction::ScrollToTop,
            "scroll_to_top",
            &[(KeyCode::Home, KeyModifiers::CONTROL)],
        ),
        (
            KeyAction::ScrollToBottom,
            "scroll_to_bottom",
            &[(KeyCode::End, KeyModifiers::CONTROL)],
        ),
        (
            KeyAction::NextChat,
            "next_chat",
            &[(KeyCode::Char('n'), KeyModifiers::CONTROL)],
        ),
        (
            KeyAction::PrevChat,
            "prev_chat",
            &[(KeyCode::Char('p'), KeyModifiers::CONTROL)],
        ),
        (
            KeyAction::CloseChat,
            "close_chat",
            &[(KeyCode::Char('x'), KeyModifiers::CONTROL)],
        ),
        (
            KeyAction::KillSubagent,
            "kill_subagent",
            &[(KeyCode::Char('k'), KeyModifiers::CONTROL)],
        ),
        (
            // Alt+X, not Ctrl+X: Ctrl+X is `close_chat`. Matches the
            // on-screen hint printed when a message is queued.
            KeyAction::DropQueue,
            "drop_queue",
            &[(KeyCode::Char('x'), KeyModifiers::ALT)],
        ),
        (
            // Shift+Tab arrives from the terminal as the BackTab sequence;
            // see `normalize_chord`. Tab itself stays intrinsic to the
            // input editor (completion), so this is a distinct key.
            KeyAction::CyclePrompt,
            "cycle_prompt",
            &[(KeyCode::Tab, KeyModifiers::SHIFT)],
        ),
    ];
}

/// Resolves key chords to a [`Command`] namespace `A`: built-in defaults
/// plus the user's per-chord overrides. Keyed on a [`ChordSeq`] so
/// multi-key bindings can be stored. One generic backs both the global
/// [`Keymap`] (over [`KeyAction`]) and the input-editor [`InputKeymap`]
/// (over [`InputAction`]); they differ only in which `resolve` they call
/// and that sequence matching ([`classify_seq`]) is global-only.
#[derive(Debug, Clone)]
pub struct Bindings<A: Command> {
    map: HashMap<ChordSeq, A>,
}

impl<A: Command> Default for Bindings<A> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}

/// Canonicalize a raw key chord for lookup. Shift+Tab is delivered by the
/// terminal as the BackTab sequence (crossterm `KeyCode::BackTab`), with or
/// without SHIFT depending on the terminal, while config bindings spell it
/// `"shift-tab"` / `"backtab"` → `(Tab, SHIFT)`. Fold the keystroke onto that
/// chord so a binding matches regardless of how it is reported.
fn normalize_chord(code: KeyCode, modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
    if code == KeyCode::BackTab {
        (KeyCode::Tab, KeyModifiers::SHIFT)
    } else {
        (code, modifiers)
    }
}

impl<A: Command> Bindings<A> {
    /// The built-in keymap (no config applied).
    pub fn defaults() -> Self {
        let mut map = HashMap::new();
        for (action, _, chords) in A::ALL {
            for chord in *chords {
                map.insert(vec![*chord], *action);
            }
        }
        Self { map }
    }

    /// The action bound to `key` (as a single-key chord), matching modifiers
    /// exactly. Used by the global keymap.
    pub fn resolve(&self, key: &KeyEvent) -> Option<A> {
        let chord = normalize_chord(key.code, key.modifiers);
        self.map.get(&[chord][..]).copied()
    }

    /// Like [`resolve`], but on a miss retries with Shift dropped. The input
    /// editor uses this: the hardcoded arms it replaced used
    /// `modifiers.contains(..)` guards / a bare nav arm, so e.g. `Shift+Left`
    /// moved the cursor and `Ctrl+Shift+A` jumped to line start. No editing
    /// chord binds Shift distinctly, so dropping it on a miss restores that
    /// tolerance without shadowing an explicit binding (the exact pass wins).
    pub fn resolve_lenient(&self, key: &KeyEvent) -> Option<A> {
        if let Some(action) = self.resolve(key) {
            return Some(action);
        }
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            let without_shift = key.modifiers - KeyModifiers::SHIFT;
            return self.map.get(&[(key.code, without_shift)][..]).copied();
        }
        None
    }

    /// True when `seq` is a *proper* prefix of some longer bound sequence —
    /// i.e. more keys could still complete a binding. Drives the pending-
    /// prefix hold in the chord-sequence runtime (#234).
    fn is_strict_prefix(&self, seq: &[Chord]) -> bool {
        self.map
            .keys()
            .any(|k| k.len() > seq.len() && k.starts_with(seq))
    }

    /// Bind a single chord to an action, replacing any existing binding.
    /// Test-only; production overrides flow through [`Keymaps::from_config`].
    #[cfg(test)]
    pub fn insert(&mut self, chord: Chord, action: A) {
        self.map.insert(vec![chord], action);
    }
}

impl Bindings<KeyAction> {
    /// Classify an accumulated chord sequence for the #234 runtime
    /// (global commands only). Single-key bindings are deliberately NOT
    /// reported as `Exact` here — they flow through [`Keymap::resolve`] with
    /// its full dispatch precedence; only genuine multi-key sequences fire
    /// from the matcher.
    pub fn classify_seq(&self, seq: &[Chord]) -> SeqClass {
        if seq.len() >= 2
            && let Some(action) = self.map.get(seq).copied()
        {
            return SeqClass::Exact(action);
        }
        if self.is_strict_prefix(seq) {
            SeqClass::Prefix
        } else {
            SeqClass::NoMatch
        }
    }
}

/// The global "command" keymap (toggle reasoning, scroll, chat nav, …).
pub type Keymap = Bindings<KeyAction>;
/// The input-editor keymap (cursor/word motion, kill-ring, history, …).
pub type InputKeymap = Bindings<InputAction>;

/// Outcome of matching an accumulated chord sequence against the keymap
/// (#234). See [`Keymap::classify_seq`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqClass {
    /// The sequence exactly matches a multi-key binding — fire this action.
    Exact(KeyAction),
    /// The sequence is a proper prefix of a longer binding — hold and wait
    /// for more keys.
    Prefix,
    /// The sequence matches nothing extendable — not part of a sequence.
    NoMatch,
}

/// Render one chord back to the config chord grammar (e.g. `ctrl-x`),
/// the inverse of [`parse_chord`]. Used for the pending-prefix footer
/// echo and conflict warnings.
fn chord_label(chord: &Chord) -> String {
    let (code, mods) = chord;
    let mut s = String::new();
    if mods.contains(KeyModifiers::CONTROL) {
        s.push_str("ctrl-");
    }
    if mods.contains(KeyModifiers::ALT) {
        s.push_str("alt-");
    }
    if mods.contains(KeyModifiers::SHIFT) {
        s.push_str("shift-");
    }
    let key = match code {
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::F(n) => format!("f{n}"),
        other => format!("{other:?}").to_ascii_lowercase(),
    };
    s.push_str(&key);
    s
}

/// Render a chord sequence (e.g. `ctrl-x ctrl-s`).
pub fn chord_seq_label(seq: &[Chord]) -> String {
    seq.iter().map(chord_label).collect::<Vec<_>>().join(" ")
}

/// Both keymaps built from one `keybindings` config (dirge-xv9l). The
/// global "command" keys and the input-editor keys share a single config
/// array; each `{ key, command }` routes to the right context by looking
/// its command name up in [`KeyAction`] then [`InputAction`]. The two
/// command namespaces are disjoint, so routing is unambiguous.
#[derive(Debug, Clone, Default)]
pub struct Keymaps {
    pub global: Keymap,
    pub input: InputKeymap,
}

impl Keymaps {
    /// Layer the user's `keybindings` over the defaults, routing each entry
    /// to the global or input keymap by command name. `none` / `unbind`
    /// clears the chord from BOTH contexts (a chord can be a default in
    /// each — e.g. Ctrl+N is `next_chat` globally and `history_next` in the
    /// editor — so disabling it means disabling it everywhere). Returns the
    /// keymaps plus warnings (bad chord / unknown command) to surface.
    pub fn from_config(bindings: Option<&[KeybindingConfig]>) -> (Self, Vec<String>) {
        let mut global = Keymap::defaults();
        let mut input = InputKeymap::defaults();
        let mut warnings = Vec::new();
        for b in bindings.unwrap_or(&[]) {
            let Some(seq) = parse_chord_sequence(&b.key) else {
                warnings.push(format!("keybindings: unrecognized key {:?}", b.key));
                continue;
            };
            let cmd = b.command.trim().to_ascii_lowercase().replace('-', "_");
            if matches!(cmd.as_str(), "none" | "noop" | "unbind" | "") {
                global.map.remove(&seq);
                input.map.remove(&seq);
                continue;
            }
            if let Some(action) = KeyAction::from_command(&cmd) {
                // dirge-z2p6: a user (or plugin) binding for a chord is
                // authoritative — clear any default in EITHER context first so
                // a leftover default in the other context can't shadow it.
                // The global keymap resolves before the input editor, so an
                // un-cleared global default would otherwise swallow a chord the
                // user rebound to an input command (the ctrl-r → reverse_search
                // case). Rebinding replaces; no explicit `unbind` needed.
                global.map.remove(&seq);
                input.map.remove(&seq);
                global.map.insert(seq, action);
            } else if let Some(action) = InputAction::from_command(&cmd) {
                global.map.remove(&seq);
                input.map.remove(&seq);
                input.map.insert(seq, action);
            } else {
                warnings.push(format!(
                    "keybindings: unknown command {:?} for key {:?} (valid: {}, {})",
                    b.command,
                    b.key,
                    KeyAction::command_list(),
                    InputAction::command_list()
                ));
            }
        }

        // dirge-fl57 conflict resolution: a chord that is BOTH a terminal
        // (single-key) binding AND the prefix of a longer sequence can't be
        // both — the sequence must wait for more keys. The sequence wins;
        // drop the single-key binding and warn.
        let prefixed: Vec<ChordSeq> = global
            .map
            .keys()
            .filter(|k| k.len() == 1 && global.is_strict_prefix(k))
            .cloned()
            .collect();
        for k in prefixed {
            global.map.remove(&k);
            warnings.push(format!(
                "keybindings: {} starts a chord sequence; its single-key binding is disabled",
                chord_seq_label(&k)
            ));
        }

        // Chord sequences only fire for global commands (the runtime matches
        // against the global keymap). An input-editor command bound to a
        // multi-key sequence would never trigger — drop it with a warning
        // rather than leave a dead binding.
        let input_seqs: Vec<ChordSeq> = input.map.keys().filter(|k| k.len() > 1).cloned().collect();
        for k in input_seqs {
            input.map.remove(&k);
            warnings.push(format!(
                "keybindings: chord sequences ({}) are only supported for global commands, \
                 not input-editor commands",
                chord_seq_label(&k)
            ));
        }

        (Keymaps { global, input }, warnings)
    }
}

/// A rebindable input-editor command (dirge-8fkp). The text box used to
/// dispatch these from a hardcoded `match` in [`crate::ui::input`]; they
/// now resolve through an [`InputKeymap`] the same way the global command
/// keys resolve through [`Keymap`], so they can be remapped from config
/// (phase 2) and by plugins (phase 3). Only NAMED editing commands are
/// here — intrinsic editing (typing a character, Backspace, Delete, Enter
/// to submit, Tab completion) and the Ctrl+C/D/Esc panic gesture stay
/// fixed and are handled directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAction {
    /// Cursor to the start of the current (visual) line.
    CursorLineStart,
    /// Cursor to the end of the current (visual) line.
    CursorLineEnd,
    /// Cursor one character left.
    CursorLeft,
    /// Cursor one character right (accepts a slash ghost-completion at EOL).
    CursorRight,
    /// Cursor one word left.
    WordLeft,
    /// Cursor one word right.
    WordRight,
    /// Delete the character before the cursor (Ctrl+H synonym for
    /// Backspace; the Backspace key itself stays intrinsic).
    DeleteCharBack,
    /// Delete the character at the cursor (forward delete, Ctrl+D).
    DeleteCharForward,
    /// Kill (cut to the kill-ring) from the cursor to end of line.
    KillToLineEnd,
    /// Kill from the start of the line to the cursor.
    KillToLineStart,
    /// Kill the word before the cursor (adds to the kill-ring).
    KillWordBack,
    /// Delete the word before the cursor (no kill-ring — distinct from
    /// [`InputAction::KillWordBack`]).
    DeleteWordBack,
    /// Delete the word after the cursor (no kill-ring).
    DeleteWordForward,
    /// Yank (paste) the top of the kill-ring.
    Yank,
    /// Yank-pop: cycle the kill-ring at the last yank.
    YankPop,
    /// Recall the previous history entry.
    HistoryPrev,
    /// Recall the next history entry.
    HistoryNext,
    /// Enter reverse-i-search over history.
    ReverseSearch,
    /// Up: wrap-aware line motion, then history at the top row.
    LineUp,
    /// Down: wrap-aware line motion, then history at the bottom row.
    LineDown,
    /// Undo the last edit to the input buffer (dirge-7yea).
    Undo,
}

impl Command for InputAction {
    const ALL: &'static [(InputAction, &'static str, &'static [Chord])] = &[
        (
            InputAction::CursorLineStart,
            "cursor_line_start",
            &[
                (KeyCode::Char('a'), KeyModifiers::CONTROL),
                (KeyCode::Home, KeyModifiers::NONE),
            ],
        ),
        (
            InputAction::CursorLineEnd,
            "cursor_line_end",
            &[
                (KeyCode::Char('e'), KeyModifiers::CONTROL),
                (KeyCode::End, KeyModifiers::NONE),
            ],
        ),
        (
            InputAction::CursorLeft,
            "cursor_left",
            &[
                (KeyCode::Char('b'), KeyModifiers::CONTROL),
                (KeyCode::Left, KeyModifiers::NONE),
            ],
        ),
        (
            InputAction::CursorRight,
            "cursor_right",
            &[(KeyCode::Right, KeyModifiers::NONE)],
        ),
        (
            InputAction::WordLeft,
            "word_left",
            &[
                (KeyCode::Char('b'), KeyModifiers::ALT),
                (KeyCode::Left, KeyModifiers::ALT),
            ],
        ),
        (
            InputAction::WordRight,
            "word_right",
            &[
                (KeyCode::Char('f'), KeyModifiers::ALT),
                (KeyCode::Right, KeyModifiers::ALT),
            ],
        ),
        (
            InputAction::DeleteCharBack,
            "delete_char_back",
            &[(KeyCode::Char('h'), KeyModifiers::CONTROL)],
        ),
        (
            InputAction::DeleteCharForward,
            "delete_char_forward",
            &[(KeyCode::Char('d'), KeyModifiers::CONTROL)],
        ),
        (
            InputAction::KillToLineEnd,
            "kill_to_line_end",
            &[(KeyCode::Char('k'), KeyModifiers::CONTROL)],
        ),
        (
            InputAction::KillToLineStart,
            "kill_to_line_start",
            &[(KeyCode::Char('u'), KeyModifiers::CONTROL)],
        ),
        (
            InputAction::KillWordBack,
            "kill_word_back",
            &[(KeyCode::Char('w'), KeyModifiers::CONTROL)],
        ),
        (
            InputAction::DeleteWordBack,
            "delete_word_back",
            &[(KeyCode::Backspace, KeyModifiers::ALT)],
        ),
        (
            InputAction::DeleteWordForward,
            "delete_word_forward",
            &[(KeyCode::Char('d'), KeyModifiers::ALT)],
        ),
        (
            InputAction::Yank,
            "yank",
            &[(KeyCode::Char('y'), KeyModifiers::CONTROL)],
        ),
        (
            InputAction::YankPop,
            "yank_pop",
            &[(KeyCode::Char('y'), KeyModifiers::ALT)],
        ),
        (
            InputAction::HistoryPrev,
            "history_prev",
            &[(KeyCode::Char('p'), KeyModifiers::CONTROL)],
        ),
        (
            InputAction::HistoryNext,
            "history_next",
            &[(KeyCode::Char('n'), KeyModifiers::CONTROL)],
        ),
        (
            InputAction::ReverseSearch,
            "reverse_search",
            &[(KeyCode::Char('f'), KeyModifiers::CONTROL)],
        ),
        (
            InputAction::LineUp,
            "line_up",
            &[(KeyCode::Up, KeyModifiers::NONE)],
        ),
        (
            InputAction::LineDown,
            "line_down",
            &[(KeyCode::Down, KeyModifiers::NONE)],
        ),
        (
            InputAction::Undo,
            "undo",
            &[(KeyCode::Char('z'), KeyModifiers::CONTROL)],
        ),
    ];
}

/// Parse a chord SEQUENCE: one or more chords separated by whitespace,
/// e.g. `ctrl-x ctrl-s` (emacs-style, dirge-fl57) or a bare `ctrl-r`.
/// Each chord is parsed by [`parse_chord`]; the whole spec fails if any
/// chord does or the spec is empty. (The space *key* is written as the
/// word `space`, so whitespace-splitting is unambiguous.)
pub fn parse_chord_sequence(spec: &str) -> Option<ChordSeq> {
    let chords: Option<ChordSeq> = spec.split_whitespace().map(parse_chord).collect();
    let chords = chords?;
    if chords.is_empty() {
        None
    } else {
        Some(chords)
    }
}

/// Parse a chord string like `ctrl-r`, `pageup`, `ctrl-shift-x`, `home`,
/// `f5` into a `(KeyCode, KeyModifiers)`. Case-insensitive, `-`/`+`
/// separated, modifiers before the key. Returns `None` on a malformed
/// spec. This is the single chord grammar for the whole app — the plugin
/// layer's `parse_key_spec` delegates here (dirge-5kkx.2). It lives in this
/// always-compiled module so it's available without the `plugin` feature.
pub fn parse_chord(spec: &str) -> Option<(KeyCode, KeyModifiers)> {
    let spec = spec.trim().to_ascii_lowercase();
    if spec.is_empty() {
        return None;
    }
    let parts: Vec<&str> = spec.split(['-', '+']).filter(|s| !s.is_empty()).collect();
    let (key_part, mod_parts) = parts.split_last()?;
    let mut modifiers = KeyModifiers::NONE;
    for m in mod_parts {
        match *m {
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "alt" | "meta" | "option" => modifiers |= KeyModifiers::ALT,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            _ => return None,
        }
    }
    let code = match *key_part {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        // "backtab" is how terminals deliver Shift+Tab; canonicalize it to
        // the same chord as "shift-tab" (Tab + SHIFT).
        "backtab" => {
            modifiers |= KeyModifiers::SHIFT;
            KeyCode::Tab
        }
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pagedn" => KeyCode::PageDown,
        f if f.starts_with('f') && f.len() >= 2 && f[1..].chars().all(|c| c.is_ascii_digit()) => {
            // Strict: reject a leading zero (`f01`) so it doesn't silently
            // parse to F(1) via lenient u8::from_str.
            let suffix = &f[1..];
            if suffix.len() > 1 && suffix.starts_with('0') {
                return None;
            }
            let n: u8 = suffix.parse().ok()?;
            if (1..=12).contains(&n) {
                KeyCode::F(n)
            } else {
                return None;
            }
        }
        s if s.chars().count() == 1 => KeyCode::Char(s.chars().next().unwrap()),
        _ => return None,
    };
    Some((code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(key: &str, command: &str) -> KeybindingConfig {
        KeybindingConfig {
            key: key.to_string(),
            command: command.to_string(),
        }
    }
    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }
    /// Global-keymap half of `Keymaps::from_config`, for the global-binding
    /// tests below.
    fn global_from(bindings: &[KeybindingConfig]) -> (Keymap, Vec<String>) {
        let (kms, warns) = Keymaps::from_config(Some(bindings));
        (kms.global, warns)
    }

    #[test]
    fn defaults_resolve() {
        let km = Keymap::defaults();
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(KeyAction::ToggleReasoning)
        );
        assert_eq!(
            km.resolve(&ev(KeyCode::PageUp, KeyModifiers::NONE)),
            Some(KeyAction::ScrollPageUp)
        );
        // A plain char / unbound chord resolves to nothing.
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('a'), KeyModifiers::NONE)),
            None
        );
    }

    /// dirge-e59d: Alt+X drops queued interjections (Ctrl+X stays
    /// `close_chat`). Pin the binding so the on-screen "Alt+X drops" hint
    /// can't drift from the actual chord.
    #[test]
    fn alt_x_drops_queue_ctrl_x_closes_chat() {
        let km = Keymap::defaults();
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('x'), KeyModifiers::ALT)),
            Some(KeyAction::DropQueue)
        );
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            Some(KeyAction::CloseChat)
        );
        assert_eq!(
            KeyAction::from_command("drop_queue"),
            Some(KeyAction::DropQueue)
        );
    }

    /// Bare Home/End are LEFT for the input editor (cursor to line start/end);
    /// chat scroll-to-extremes is on Ctrl+Home/End. Pin it so the editor
    /// handlers can't get silently shadowed again.
    #[test]
    fn home_end_scroll_is_ctrl_only() {
        let km = Keymap::defaults();
        assert_eq!(km.resolve(&ev(KeyCode::Home, KeyModifiers::NONE)), None);
        assert_eq!(km.resolve(&ev(KeyCode::End, KeyModifiers::NONE)), None);
        assert_eq!(
            km.resolve(&ev(KeyCode::Home, KeyModifiers::CONTROL)),
            Some(KeyAction::ScrollToTop)
        );
        assert_eq!(
            km.resolve(&ev(KeyCode::End, KeyModifiers::CONTROL)),
            Some(KeyAction::ScrollToBottom)
        );
    }

    #[test]
    fn parse_chord_forms() {
        assert_eq!(
            parse_chord("ctrl-r"),
            Some((KeyCode::Char('r'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_chord("Ctrl+T"),
            Some((KeyCode::Char('t'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_chord("pageup"),
            Some((KeyCode::PageUp, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_chord("ctrl-shift-x"),
            Some((
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ))
        );
        assert_eq!(parse_chord("f5"), Some((KeyCode::F(5), KeyModifiers::NONE)));
        assert_eq!(parse_chord("boguskey"), None);
        assert_eq!(parse_chord("ctrl-"), None);
        assert_eq!(parse_chord("f99"), None);
    }

    #[test]
    fn parse_chord_is_the_one_grammar() {
        // dirge-5kkx.2: parse_key_spec (plugin) now delegates here, so the
        // formerly-divergent cases resolve consistently.
        assert_eq!(
            parse_chord("f01"),
            None,
            "strict f-keys reject leading zero"
        );
        assert_eq!(
            parse_chord("bs"),
            Some((KeyCode::Backspace, KeyModifiers::NONE)),
            "the `bs` alias is accepted"
        );
        // `+` separator and `option` modifier both work (config-doc grammar).
        assert_eq!(
            parse_chord("ctrl+x"),
            Some((KeyCode::Char('x'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_chord("option-f"),
            Some((KeyCode::Char('f'), KeyModifiers::ALT))
        );
    }

    #[test]
    fn shift_tab_and_backtab_parse_to_tab_shift() {
        // Terminals deliver Shift+Tab as the BackTab sequence; both spellings
        // canonicalize to the same chord so a config binding matches either.
        assert_eq!(
            parse_chord("shift-tab"),
            Some((KeyCode::Tab, KeyModifiers::SHIFT))
        );
        assert_eq!(
            parse_chord("backtab"),
            Some((KeyCode::Tab, KeyModifiers::SHIFT))
        );
    }

    #[test]
    fn backtab_keyevent_resolves_a_shift_tab_binding() {
        // A real Shift+Tab keystroke arrives as KeyCode::BackTab (with or
        // without SHIFT, depending on the terminal); a "shift-tab" config
        // binding must match it.
        let (km, warns) = global_from(&[cfg("shift-tab", "toggle_reasoning")]);
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            km.resolve(&ev(KeyCode::BackTab, KeyModifiers::NONE)),
            Some(KeyAction::ToggleReasoning)
        );
        assert_eq!(
            km.resolve(&ev(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(KeyAction::ToggleReasoning)
        );
    }

    #[test]
    fn cycle_prompt_command_resolves() {
        assert_eq!(
            KeyAction::from_command("cycle_prompt"),
            Some(KeyAction::CyclePrompt)
        );
        assert_eq!(
            KeyAction::from_command("cycle-prompt"),
            Some(KeyAction::CyclePrompt)
        );
    }

    #[test]
    fn cycle_prompt_is_the_default_shift_tab_binding() {
        let km = Keymap::defaults();
        assert_eq!(
            km.resolve(&ev(KeyCode::BackTab, KeyModifiers::NONE)),
            Some(KeyAction::CyclePrompt)
        );
        assert_eq!(
            km.resolve(&ev(KeyCode::Tab, KeyModifiers::SHIFT)),
            Some(KeyAction::CyclePrompt)
        );
    }

    #[test]
    fn override_rebinds_and_keeps_other_defaults() {
        // Rebind toggle-reasoning to Ctrl+T.
        let (km, warns) = global_from(&[cfg("ctrl-t", "toggle_reasoning")]);
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Some(KeyAction::ToggleReasoning)
        );
        // The default Ctrl+R still toggles (adding a binding doesn't drop
        // the default), and an unrelated default is intact.
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(KeyAction::ToggleReasoning)
        );
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            Some(KeyAction::NextChat)
        );
    }

    #[test]
    fn override_on_an_occupied_chord_replaces_it() {
        // Binding Ctrl+R to next_chat takes Ctrl+R away from toggle.
        let (km, warns) = global_from(&[cfg("ctrl-r", "next_chat")]);
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(KeyAction::NextChat)
        );
    }

    #[test]
    fn unbind_removes_a_default() {
        let (km, _) = global_from(&[cfg("ctrl-r", "none")]);
        assert_eq!(
            km.resolve(&ev(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn rebind_across_contexts_overrides_without_explicit_unbind() {
        // dirge-z2p6: Ctrl+R defaults to a GLOBAL action (ToggleReasoning).
        // The user rebinds it to `reverse_search`, an INPUT command. Because
        // the global keymap resolves before the input editor, an un-cleared
        // global default would swallow the chord. The rebind must take effect
        // with NO explicit `unbind` first.
        let (kms, warns) = Keymaps::from_config(Some(&[cfg("ctrl-r", "reverse_search")]));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            kms.input
                .resolve(&ev(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(InputAction::ReverseSearch),
        );
        assert_eq!(
            kms.global
                .resolve(&ev(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            None,
            "the shadowing global default must be cleared by the rebind",
        );
    }

    #[test]
    fn rebind_input_default_to_global_clears_the_input_default() {
        // Symmetric: Ctrl+A defaults to an INPUT command (CursorLineStart).
        // Rebinding it to a global command makes the chord global-only — the
        // now-shadowed input default is cleared rather than left dead.
        let (kms, warns) = Keymaps::from_config(Some(&[cfg("ctrl-a", "next_chat")]));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            kms.global
                .resolve(&ev(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(KeyAction::NextChat),
        );
        assert_eq!(
            kms.input
                .resolve(&ev(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            None,
        );
    }

    #[test]
    fn invalid_chord_and_unknown_command_warn() {
        let (_, warns) = global_from(&[
            cfg("kaboom", "toggle_reasoning"),
            cfg("ctrl-y", "do_a_barrel_roll"),
        ]);
        assert_eq!(warns.len(), 2, "{warns:?}");
        assert!(warns[0].contains("unrecognized key"));
        assert!(warns[1].contains("unknown command"));
    }

    // --- InputKeymap (dirge-8fkp) ----------------------------------

    #[test]
    fn input_defaults_resolve_historical_chords() {
        let km = InputKeymap::defaults();
        // A representative sweep of the historical text-editing keys.
        let cases = [
            (
                (KeyCode::Char('a'), KeyModifiers::CONTROL),
                InputAction::CursorLineStart,
            ),
            (
                (KeyCode::Home, KeyModifiers::NONE),
                InputAction::CursorLineStart,
            ),
            (
                (KeyCode::Char('e'), KeyModifiers::CONTROL),
                InputAction::CursorLineEnd,
            ),
            (
                (KeyCode::End, KeyModifiers::NONE),
                InputAction::CursorLineEnd,
            ),
            (
                (KeyCode::Char('b'), KeyModifiers::CONTROL),
                InputAction::CursorLeft,
            ),
            ((KeyCode::Left, KeyModifiers::NONE), InputAction::CursorLeft),
            (
                (KeyCode::Right, KeyModifiers::NONE),
                InputAction::CursorRight,
            ),
            (
                (KeyCode::Char('b'), KeyModifiers::ALT),
                InputAction::WordLeft,
            ),
            ((KeyCode::Left, KeyModifiers::ALT), InputAction::WordLeft),
            (
                (KeyCode::Char('f'), KeyModifiers::ALT),
                InputAction::WordRight,
            ),
            ((KeyCode::Right, KeyModifiers::ALT), InputAction::WordRight),
            (
                (KeyCode::Char('d'), KeyModifiers::CONTROL),
                InputAction::DeleteCharForward,
            ),
            (
                (KeyCode::Char('k'), KeyModifiers::CONTROL),
                InputAction::KillToLineEnd,
            ),
            (
                (KeyCode::Char('u'), KeyModifiers::CONTROL),
                InputAction::KillToLineStart,
            ),
            (
                (KeyCode::Char('w'), KeyModifiers::CONTROL),
                InputAction::KillWordBack,
            ),
            (
                (KeyCode::Backspace, KeyModifiers::ALT),
                InputAction::DeleteWordBack,
            ),
            (
                (KeyCode::Char('d'), KeyModifiers::ALT),
                InputAction::DeleteWordForward,
            ),
            (
                (KeyCode::Char('y'), KeyModifiers::CONTROL),
                InputAction::Yank,
            ),
            (
                (KeyCode::Char('y'), KeyModifiers::ALT),
                InputAction::YankPop,
            ),
            (
                (KeyCode::Char('p'), KeyModifiers::CONTROL),
                InputAction::HistoryPrev,
            ),
            (
                (KeyCode::Char('n'), KeyModifiers::CONTROL),
                InputAction::HistoryNext,
            ),
            (
                (KeyCode::Char('f'), KeyModifiers::CONTROL),
                InputAction::ReverseSearch,
            ),
            ((KeyCode::Up, KeyModifiers::NONE), InputAction::LineUp),
            ((KeyCode::Down, KeyModifiers::NONE), InputAction::LineDown),
        ];
        for ((code, mods), want) in cases {
            assert_eq!(km.resolve(&ev(code, mods)), Some(want), "{code:?}+{mods:?}");
        }
    }

    #[test]
    fn input_resolve_treats_shift_as_insignificant_on_miss() {
        // Regression guard: the old hardcoded arms tolerated extra modifiers
        // (a bare `KeyCode::Left =>` matched Shift+Left). Exact matching alone
        // would drop those; resolve retries without Shift.
        let km = InputKeymap::defaults();
        // Shift+nav still moves.
        assert_eq!(
            km.resolve_lenient(&ev(KeyCode::Left, KeyModifiers::SHIFT)),
            Some(InputAction::CursorLeft)
        );
        assert_eq!(
            km.resolve_lenient(&ev(KeyCode::Home, KeyModifiers::SHIFT)),
            Some(InputAction::CursorLineStart)
        );
        // Ctrl+Shift+A still jumps to line start (Shift dropped → Ctrl+A).
        assert_eq!(
            km.resolve_lenient(&ev(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(InputAction::CursorLineStart)
        );
        // An explicit Shift binding still wins via the exact pass.
        let mut km2 = InputKeymap::defaults();
        km2.insert((KeyCode::Left, KeyModifiers::SHIFT), InputAction::WordLeft);
        assert_eq!(
            km2.resolve_lenient(&ev(KeyCode::Left, KeyModifiers::SHIFT)),
            Some(InputAction::WordLeft)
        );
    }

    #[test]
    fn input_intrinsic_keys_are_unbound() {
        // Plain chars, Backspace, Delete, Enter, Tab stay intrinsic — they
        // must NOT resolve to a rebindable action (so handle_key keeps them).
        let km = InputKeymap::defaults();
        for (code, mods) in [
            (KeyCode::Char('a'), KeyModifiers::NONE),
            (KeyCode::Backspace, KeyModifiers::NONE),
            (KeyCode::Delete, KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::NONE),
            (KeyCode::Tab, KeyModifiers::NONE),
        ] {
            assert_eq!(km.resolve(&ev(code, mods)), None, "{code:?}");
        }
    }

    #[test]
    fn every_input_command_name_round_trips() {
        // ALL is the single source of truth: each command name resolves back
        // to its action, and unknown names don't.
        for (action, name, _) in InputAction::ALL {
            assert_eq!(InputAction::from_command(name), Some(*action), "{name}");
        }
        assert_eq!(InputAction::from_command("not_a_command"), None);
        // Global and input command namespaces stay disjoint (phase 2 routes
        // a single `keybindings` array across both by name).
        for (_, name, _) in InputAction::ALL {
            assert_eq!(KeyAction::from_command(name), None, "collision on {name}");
        }
        for (_, name, _) in KeyAction::ALL {
            assert_eq!(InputAction::from_command(name), None, "collision on {name}");
        }
    }

    // --- unified config surface (dirge-xv9l) -----------------------

    #[test]
    fn config_rebinds_an_input_command() {
        // A single `keybindings` array can now target input-editor commands.
        let (kms, warns) = Keymaps::from_config(Some(&[cfg("ctrl-z", "kill_to_line_start")]));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            kms.input
                .resolve(&ev(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            Some(InputAction::KillToLineStart)
        );
        // The default Ctrl+U still maps too (adding doesn't drop the default).
        assert_eq!(
            kms.input
                .resolve(&ev(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            Some(InputAction::KillToLineStart)
        );
        // Global keymap untouched.
        assert_eq!(
            kms.global
                .resolve(&ev(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(KeyAction::ToggleReasoning)
        );
    }

    #[test]
    fn config_routes_global_and_input_in_one_array() {
        let (kms, warns) = Keymaps::from_config(Some(&[
            cfg("ctrl-t", "toggle_reasoning"), // global
            cfg("alt-a", "cursor_line_start"), // input
        ]));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            kms.global
                .resolve(&ev(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Some(KeyAction::ToggleReasoning)
        );
        assert_eq!(
            kms.input
                .resolve(&ev(KeyCode::Char('a'), KeyModifiers::ALT)),
            Some(InputAction::CursorLineStart)
        );
    }

    #[test]
    fn unbind_clears_both_contexts() {
        // Ctrl+N is a default in BOTH maps (next_chat / history_next); `none`
        // disables it everywhere.
        let (kms, _) = Keymaps::from_config(Some(&[cfg("ctrl-n", "none")]));
        assert_eq!(
            kms.global
                .resolve(&ev(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            kms.input
                .resolve(&ev(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn unknown_command_warns_with_both_namespaces() {
        let (_, warns) = Keymaps::from_config(Some(&[cfg("ctrl-y", "do_a_barrel_roll")]));
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(warns[0].contains("unknown command"));
        // The valid-command hint lists both global and input vocabularies.
        assert!(warns[0].contains("toggle_reasoning"));
        assert!(warns[0].contains("cursor_line_start"));
    }

    #[test]
    fn parse_chord_sequence_forms() {
        // Single chord → length 1.
        assert_eq!(
            parse_chord_sequence("ctrl-r"),
            Some(vec![(KeyCode::Char('r'), KeyModifiers::CONTROL)])
        );
        // Multi-key emacs sequence → length 2.
        assert_eq!(
            parse_chord_sequence("ctrl-x ctrl-s"),
            Some(vec![
                (KeyCode::Char('x'), KeyModifiers::CONTROL),
                (KeyCode::Char('s'), KeyModifiers::CONTROL),
            ])
        );
        // The space KEY is the word `space`, so it isn't a separator.
        assert_eq!(
            parse_chord_sequence("ctrl-space"),
            Some(vec![(KeyCode::Char(' '), KeyModifiers::CONTROL)])
        );
        // Empty / all-whitespace → None; a bad chord anywhere fails the whole.
        assert_eq!(parse_chord_sequence("   "), None);
        assert_eq!(parse_chord_sequence("ctrl-x boguskey"), None);
    }

    #[test]
    fn plugin_then_user_precedence_user_wins() {
        // dirge-rj3k / #476: the host concatenates plugin bind-key entries
        // BEFORE the user's config and feeds the lot to from_config, which
        // applies in order (last wins). So on a conflicting chord the user
        // overrides the plugin, while a plugin-only binding still takes.
        let plugin = [
            cfg("ctrl-t", "toggle_reasoning"), // plugin-only → takes effect
            cfg("ctrl-r", "next_chat"),        // conflicts with the user below
        ];
        let user = [cfg("ctrl-r", "scroll_to_top")];
        let merged: Vec<KeybindingConfig> = plugin.iter().chain(user.iter()).cloned().collect();
        let (kms, warns) = Keymaps::from_config(Some(&merged));
        assert!(warns.is_empty(), "{warns:?}");
        // Plugin-only binding present.
        assert_eq!(
            kms.global
                .resolve(&ev(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Some(KeyAction::ToggleReasoning)
        );
        // User wins the Ctrl+R conflict over the plugin.
        assert_eq!(
            kms.global
                .resolve(&ev(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(KeyAction::ScrollToTop)
        );
    }

    #[test]
    fn plugin_can_rebind_an_input_command_and_unbind() {
        // A plugin remapping the editor: bind Ctrl+T to kill-to-line-end and
        // disable the default Ctrl+K.
        let plugin = [cfg("ctrl-t", "kill_to_line_end"), cfg("ctrl-k", "none")];
        let (kms, warns) = Keymaps::from_config(Some(&plugin));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            kms.input
                .resolve(&ev(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Some(InputAction::KillToLineEnd)
        );
        // Ctrl+K cleared from the input map (and the global kill_subagent
        // default too, since `none` clears both contexts).
        assert_eq!(
            kms.input
                .resolve(&ev(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            None
        );
    }

    // --- chord-sequence runtime (#234, dirge-fl57) -----------------

    fn ctrl(c: char) -> Chord {
        (KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn sequence_binding_disables_its_terminal_prefix() {
        // Binding `ctrl-x ctrl-s` makes plain Ctrl+X a prefix key, so its
        // default terminal binding (close_chat) is dropped with a warning —
        // the sequence wins.
        let (kms, warns) = Keymaps::from_config(Some(&[cfg("ctrl-x ctrl-s", "toggle_reasoning")]));
        assert_eq!(
            kms.global.map.get(&vec![ctrl('x'), ctrl('s')]),
            Some(&KeyAction::ToggleReasoning)
        );
        // Plain Ctrl+X no longer resolves to close_chat (it's a prefix now).
        assert_eq!(
            kms.global
                .resolve(&ev(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            None
        );
        assert!(
            warns.iter().any(|w| w.contains("starts a chord sequence")),
            "{warns:?}"
        );
    }

    #[test]
    fn classify_seq_holds_prefix_then_fires_exact() {
        let (kms, _) = Keymaps::from_config(Some(&[cfg("ctrl-x ctrl-s", "scroll_to_top")]));
        let km = &kms.global;
        // First key of the sequence → hold.
        assert_eq!(km.classify_seq(&[ctrl('x')]), SeqClass::Prefix);
        // Completing it → fire.
        assert_eq!(
            km.classify_seq(&[ctrl('x'), ctrl('s')]),
            SeqClass::Exact(KeyAction::ScrollToTop)
        );
        // A wrong second key → no match (caller aborts the prefix).
        assert_eq!(km.classify_seq(&[ctrl('x'), ctrl('a')]), SeqClass::NoMatch);
        // An unrelated single key → no match (handled by normal resolve).
        assert_eq!(km.classify_seq(&[ctrl('r')]), SeqClass::NoMatch);
    }

    #[test]
    fn single_key_bindings_never_classify_as_exact() {
        // The matcher only fires multi-key sequences; a plain Ctrl+R binding
        // is left to the normal dispatch (so its precedence rules apply).
        let km = Keymap::defaults();
        assert_eq!(km.classify_seq(&[ctrl('r')]), SeqClass::NoMatch);
    }

    #[test]
    fn input_command_sequence_is_rejected_with_warning() {
        // Sequences only fire for global commands; an input-command sequence
        // is dropped with a warning rather than left as a dead binding.
        let (kms, warns) = Keymaps::from_config(Some(&[cfg("ctrl-x ctrl-s", "kill_to_line_end")]));
        assert!(
            kms.input.map.keys().all(|k| k.len() == 1),
            "no input sequences kept"
        );
        assert!(
            warns
                .iter()
                .any(|w| w.contains("only supported for global commands")),
            "{warns:?}"
        );
    }

    #[test]
    fn chord_labels_round_trip_the_grammar() {
        assert_eq!(chord_label(&ctrl('x')), "ctrl-x");
        assert_eq!(chord_label(&(KeyCode::Home, KeyModifiers::NONE)), "home");
        assert_eq!(
            chord_label(&(KeyCode::Char('f'), KeyModifiers::ALT | KeyModifiers::SHIFT)),
            "alt-shift-f"
        );
        assert_eq!(chord_seq_label(&[ctrl('x'), ctrl('s')]), "ctrl-x ctrl-s");
    }
}
