//! Shared ANSI / control-byte handling.
//!
//! Three things show up across the UI layer:
//!   1. Sanitizing text from untrusted producers (tool output, MCP
//!      server stderr, websearch results) before it reaches the
//!      chat buffer.
//!   2. Computing the visible width of strings that may embed SGR
//!      escapes (lives in `wrap::visible_width`).
//!   3. Building SGR colour sequences (lives in `markdown::ansi_fg`).
//!
//! Centralising (1) here means MCP / websearch / tool-output / chat
//! sanitization share one definition of "what's a control byte" —
//! previously each had its own filter, drifting in coverage (e.g.
//! one blocked C0 but not C1, another stripped `\r` but not C1
//! either).
//!
//! Threat model: a child process / search result / tool response
//! must not be able to steer the terminal (set color, move cursor,
//! disable mouse mode, switch alt screen, run OSC bell/notification,
//! emit DCS sequences). All known escape-introducer codepoints get
//! filtered:
//!   - C0 controls (U+0000..=U+001F) — including ESC (U+001B)
//!   - DEL (U+007F)
//!   - C1 controls (U+0080..=U+009F) — single-byte CSI / OSC / DCS
//!     in 8-bit terminals
//!
//! Newline and tab are SEPARATE knobs because some consumers want
//! to preserve them (chat markdown), others don't (chamber rows,
//! single-line banners).

/// What whitespace-class controls to preserve. The "block all"
/// posture is the safe default; consumers that need newline /
/// tab pass-through opt in explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StripPolicy {
    /// Preserve U+000A (LF). Most chat consumers want this so
    /// multi-line content renders as separate rows.
    pub keep_newline: bool,
    /// Preserve U+0009 (TAB). Chamber rows expand tabs to spaces
    /// before this point; banners + single-line UIs leave tabs as
    /// space-equivalent and want them stripped (collapse to space).
    pub keep_tab: bool,
}

impl StripPolicy {
    /// Block everything; collapses to plain ASCII / non-control
    /// Unicode. Use for single-line banners, alert rows, MCP log
    /// lines where the rendering layer wraps after we return.
    pub const STRICT: Self = Self {
        keep_newline: false,
        keep_tab: false,
    };

    /// Preserve `\n` for multi-line consumers. Still strips ESC,
    /// CR, DEL, C1. Use for chat text / tool output that the
    /// renderer splits on `\n` itself.
    pub const KEEP_NEWLINE: Self = Self {
        keep_newline: true,
        keep_tab: false,
    };

    /// Preserve both `\n` and `\t`. Use for chat content that
    /// flows through markdown rendering (tabs survive into the
    /// rendered code-block) — e.g. `sanitize_output`.
    pub const KEEP_BOTH: Self = Self {
        keep_newline: true,
        keep_tab: true,
    };
}

/// Strip control bytes from `s` according to `policy`. Review #14:
/// fast-path returns the input unchanged when no chars would be
/// filtered, avoiding the `chars().filter().collect()` allocation
/// for the common case (most MCP log lines / chat tokens have no
/// control bytes to strip).
///
/// NOTE: this drops individual control bytes but does NOT consume
/// the printable payload of escape sequences. `"\x1b]0;EVIL\x07"`
/// becomes `"]0;EVIL"` (the non-control chars between ESC and BEL
/// survive). For text that may contain attacker-crafted escape
/// sequences, use [`strip_escapes`] instead — it consumes the
/// entire sequence including the payload.
pub fn strip_controls(s: &str, policy: StripPolicy) -> String {
    if s.chars().all(|c| keep_char(c, policy)) {
        return s.to_string();
    }
    s.chars().filter(|c| keep_char(*c, policy)).collect()
}

/// Strip full ANSI escape sequences AND control characters.
///
/// Unlike [`strip_controls`], which drops individual control bytes
/// but leaves the printable payload of escape sequences intact,
/// this function consumes the ENTIRE sequence:
///   - CSI: `ESC [...final-byte`  (consumed until alphabetic, `~`, or BEL)
///   - OSC: `ESC ]...BEL` or `ESC ]...ESC \`  (consumed until terminator)
///   - DCS/APC/PM/SOS: `ESC P/X/^/_...ESC \`  (consumed until ST)
///   - Single-byte ESC: the byte after ESC is consumed
///   - C0 controls (U+0000..U+001F), DEL (U+007F), C1 (U+0080..U+009F)
///
/// Caps sequence length at 256 bytes (CSI/OSC) or 4096 bytes (DCS)
/// to prevent DoS on unterminated sequences. `\n` and `\t` are
/// preserved or stripped per `policy`.
///
/// Use this for text from untrusted producers (LLM output, bash
/// results) that may carry attacker-crafted escape sequences.
pub fn strip_escapes(s: &str, policy: StripPolicy) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    // CSI: ESC [...final-byte  (final byte in 0x40..=0x7E)
                    let mut n = 0;
                    for next in &mut chars {
                        let cp = next as u32;
                        if (0x40..=0x7e).contains(&cp) {
                            break;
                        }
                        n += 1;
                        if n >= 256 {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: ESC ]...BEL or ESC ]...ESC \
                    let mut n = 0;
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' {
                            // ST terminator: ESC \ — peek the next
                            // char without consuming it if it's not
                            // a backslash.
                            let mut peek = chars.clone();
                            if peek.next() == Some('\\') {
                                chars = peek;
                                break;
                            }
                            // Not ST — ESC inside payload; continue.
                        }
                        n += 1;
                        if n >= 256 {
                            break;
                        }
                    }
                }
                Some('P') | Some('X') | Some('^') | Some('_') => {
                    let mut prev = '\0';
                    let mut n = 0;
                    for next in &mut chars {
                        if prev == '\x1b' && next == '\\' {
                            break;
                        }
                        prev = next;
                        n += 1;
                        if n >= 4096 {
                            break;
                        }
                    }
                }
                Some(_) => {} // Single-byte ESC — skip the second char.
                None => break,
            }
        } else if !keep_char(c, policy) {
            continue;
        } else {
            result.push(c);
        }
    }
    result
}

/// Strip ANSI CSI escape sequences (`\x1b[…m` and friends) from `s`,
/// returning a clean printable string. Used by clipboard / selection
/// paths: the renderer bakes SGR codes into `LineEntry::text` for
/// inline-styled markdown (see markdown.rs:291), and we don't want
/// the user copying `\x1b[31mbold\x1b[0m` into their clipboard.
///
/// Mirrors the CSI-skip loop in `wrap::visible_width` (line 37) so
/// the two stay consistent. Final-byte range matches the ECMA-48 CSI
/// terminator set (0x40..=0x7E) so non-SGR sequences (cursor moves,
/// scroll regions) also strip cleanly — anything a misbehaving
/// producer might leave behind in the buffer.
pub fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // CSI introducer. Skip to (and past) the final byte in
            // the 0x40..=0x7E range.
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            i = j.saturating_add(1).min(bytes.len());
            continue;
        }
        // Single ESC not followed by `[` — drop it alone to be
        // safe (could be an OSC/DCS start; better not to copy).
        if bytes[i] == 0x1b {
            i += 1;
            continue;
        }
        // UTF-8 step.
        let step = match bytes[i] {
            b if b < 0x80 => 1,
            b if b < 0xC0 => 1,
            b if b < 0xE0 => 2,
            b if b < 0xF0 => 3,
            _ => 4,
        };
        let end = (i + step).min(bytes.len());
        out.push_str(&s[i..end]);
        i = end;
    }
    out
}

fn keep_char(c: char, policy: StripPolicy) -> bool {
    let cp = c as u32;
    if cp == 0x0A {
        return policy.keep_newline;
    }
    if cp == 0x09 {
        return policy.keep_tab;
    }
    // Block C0 controls, DEL, and C1 controls.
    if cp < 0x20 || cp == 0x7F || (0x80..=0x9F).contains(&cp) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_blocks_all_controls() {
        let s = "hello\x1b[31m world\u{9b}\x07\x00\t\n!";
        let out = strip_controls(s, StripPolicy::STRICT);
        assert_eq!(out, "hello[31m world!");
    }

    #[test]
    fn keep_newline_preserves_lf_only() {
        let s = "line1\nline2\x1b[31m\tend";
        let out = strip_controls(s, StripPolicy::KEEP_NEWLINE);
        assert_eq!(out, "line1\nline2[31mend");
    }

    #[test]
    fn keep_both_preserves_lf_and_tab() {
        let s = "a\tb\nc\x1b[0md";
        let out = strip_controls(s, StripPolicy::KEEP_BOTH);
        assert_eq!(out, "a\tb\nc[0md");
    }

    #[test]
    fn c1_csi_blocked() {
        // U+009B is single-byte CSI — must NOT survive any policy.
        let s = "before\u{9b}5;31mafter";
        for policy in [
            StripPolicy::STRICT,
            StripPolicy::KEEP_NEWLINE,
            StripPolicy::KEEP_BOTH,
        ] {
            let out = strip_controls(s, policy);
            assert!(
                !out.contains('\u{9b}'),
                "C1 CSI survived policy {policy:?}: {out:?}"
            );
        }
    }

    #[test]
    fn strip_ansi_removes_sgr_sequences_keeps_payload() {
        let s = "hello \x1b[31mred\x1b[0m world";
        assert_eq!(strip_ansi(s), "hello red world");
    }

    #[test]
    fn strip_ansi_handles_consecutive_and_nested_escapes() {
        let s = "\x1b[1m\x1b[31mbold-red\x1b[0m\x1b[0m";
        assert_eq!(strip_ansi(s), "bold-red");
    }

    #[test]
    fn strip_ansi_drops_lone_esc() {
        // ESC not followed by `[` — drop it on its own to keep the
        // clipboard payload safe (could be the head of an OSC/DCS).
        let s = "a\x1bb";
        assert_eq!(strip_ansi(s), "ab");
    }

    #[test]
    fn strip_ansi_preserves_unicode_payload() {
        let s = "\x1b[32m日本語\x1b[0m 🚀";
        assert_eq!(strip_ansi(s), "日本語 🚀");
    }

    #[test]
    fn strip_ansi_handles_non_sgr_csi() {
        // Cursor moves and scroll regions also end in a 0x40..=0x7E
        // final byte; the helper handles them too.
        let s = "before\x1b[2;5Hafter\x1b[Kend";
        assert_eq!(strip_ansi(s), "beforeafterend");
    }

    #[test]
    fn strip_ansi_handles_truncated_escape() {
        // Trailing ESC with nothing after it (truncated stream).
        // Drop trailing bytes safely.
        let s = "abc\x1b[31";
        // No final byte → we consume to end of input.
        assert_eq!(strip_ansi(s), "abc");
    }

    #[test]
    fn non_ascii_letters_pass_through() {
        let s = "naïve 日本語 🚀";
        for policy in [
            StripPolicy::STRICT,
            StripPolicy::KEEP_NEWLINE,
            StripPolicy::KEEP_BOTH,
        ] {
            assert_eq!(strip_controls(s, policy), s);
        }
    }

    // --- strip_escapes tests ---

    #[test]
    fn strip_escapes_strips_osc_sequence_with_payload() {
        // OSC title-set: the "EVIL" payload must be stripped too,
        // not just the ESC and BEL bytes.
        let s = "hello\x1b]0;EVIL\x07world";
        let out = strip_escapes(s, StripPolicy::STRICT);
        assert_eq!(out, "helloworld");
    }

    #[test]
    fn strip_escapes_strips_csi_sequence() {
        let s = "before\x1b[2Jafter";
        let out = strip_escapes(s, StripPolicy::STRICT);
        assert_eq!(out, "beforeafter");
    }

    #[test]
    fn strip_escapes_strips_sgr_sequence() {
        let s = "\x1b[31mred\x1b[0m";
        let out = strip_escapes(s, StripPolicy::STRICT);
        assert_eq!(out, "red");
    }

    #[test]
    fn strip_escapes_strips_dcs_sequence() {
        let s = "start\x1bP0;data\x1b\\end";
        let out = strip_escapes(s, StripPolicy::STRICT);
        assert_eq!(out, "startend");
    }

    #[test]
    fn strip_escapes_preserves_newline_and_tab_with_keep_both() {
        let s = "line1\n\tindented\x07line2\x1b[31mstyled";
        let out = strip_escapes(s, StripPolicy::KEEP_BOTH);
        assert_eq!(out, "line1\n\tindentedline2styled");
    }

    #[test]
    fn strip_escapes_handles_truncated_csi() {
        // Unterminated CSI — cap at 256 bytes.
        let s = "ab\x1b[9999999999";
        let out = strip_escapes(s, StripPolicy::STRICT);
        assert_eq!(out, "ab");
    }

    #[test]
    fn strip_escapes_handles_esc_inside_osc_not_st() {
        // ESC inside OSC payload that is NOT followed by \ (not ST)
        // must NOT consume the non-backslash char — the ESC is part
        // of the OSC payload, not a terminator. The real terminator
        // is BEL further in.
        let s = "a\x1b]0;payload\x1b[2J\x07b";
        let out = strip_escapes(s, StripPolicy::STRICT);
        assert_eq!(out, "ab");
    }
}
