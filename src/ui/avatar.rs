//! Inline ASCII avatar.
//!
//! A tiny single-row face that lives on the input row, centered in the
//! left margin between the screen edge and the input prompt. Updates
//! based on what the agent is doing — thinking, speaking, running a
//! tool, erroring, resting — to give the chat a personable focal
//! point and visible activity feedback even when no tokens are
//! streaming yet.
//!
//! Single-row so it never gets caught in chat scroll: chat content
//! lives on rows 0..input_top-1, the avatar lives on input_top
//! beside the prompt, and `crossterm::ScrollUp` operations don't
//! touch the input row.

use crossterm::style::Color;

/// What the agent is currently doing. The renderer picks an ascii
/// face per state and draws it next to the input prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum AvatarState {
    /// Nothing happening — neutral idle face.
    Idle,
    /// Model is thinking (reasoning tokens streaming).
    Thinking,
    /// Model is producing visible output (regular tokens streaming).
    Speaking,
    /// A read-family tool is active (read, grep, list_dir, find_files).
    Reading,
    /// A write-family tool is active (write, edit, apply_patch).
    Writing,
    /// A bash / shell tool is active.
    Bash,
    /// Permission alert or other thing demanding attention.
    Alert,
    /// Agent encountered an error.
    Error,
    /// Turn completed successfully.
    Done,
}

impl AvatarState {
    /// Choose an avatar state for a tool name. Maps well-known tool
    /// names to read/write/bash families; unknown tools default to
    /// the generic `Reading` face since most plugin / MCP tools are
    /// observational.
    pub fn from_tool_name(name: &str) -> Self {
        match name {
            "read" | "grep" | "find_files" | "list_dir" | "lsp" | "semantic" => Self::Reading,
            "write" | "edit" | "apply_patch" | "write_todo_list" => Self::Writing,
            "bash" | "shell" => Self::Bash,
            _ => Self::Reading,
        }
    }

    /// Face to show once a turn's terminal event has been handled, chosen from
    /// whether the runner is still busy. `Done` (`^_^`) only when the session
    /// is genuinely idle; if a follow-up keeps `is_running` true (a plugin
    /// hook chain, a `/plan` reviewer, a drained interjection turn, or a
    /// background review) show a working face instead. Without this the idle
    /// `Done` face was painted at the top of finalization while the runner
    /// stayed busy, so typed messages queued behind an idle-looking avatar and
    /// could be dropped when the deferred work finished (GH #621).
    pub fn settled(is_running: bool) -> Self {
        if is_running {
            Self::Thinking
        } else {
            Self::Done
        }
    }
}

/// Width of the avatar in terminal columns. Used by the avatar
/// tests to assert each face string is exactly this many cells;
/// production now reads the face width directly from the string
/// length via ratatui's set_stringn.
#[allow(dead_code)]
pub const AVATAR_W: usize = 5;

/// Return the ASCII face for the given state + animation tick. `tick`
/// alternates between two slightly different poses (blinking eyes,
/// shifting mouth) so the avatar visibly animates while the agent
/// runs without being noisy.
pub fn art(state: AvatarState, tick: bool) -> &'static str {
    use AvatarState::*;
    match state {
        Idle => {
            if tick {
                "(o o)"
            } else {
                "(- -)"
            }
        }
        Thinking => {
            if tick {
                "(o .)"
            } else {
                "(. o)"
            }
        }
        Speaking => {
            if tick {
                "(o o)"
            } else {
                "(o O)"
            }
        }
        Reading => "[@ @]",
        Writing => {
            if tick {
                "(>_<)"
            } else {
                "(-_-)"
            }
        }
        Bash => "[$_$]",
        Alert => "(O_O)",
        Error => "(x_x)",
        Done => "(^_^)",
    }
}

/// Color the avatar should render in for the given state. Errors and
/// alerts override to the theme's perm / error tones; everything else
/// uses the agent tone so it visually belongs to the chat.
pub fn color(state: AvatarState) -> Color {
    use AvatarState::*;
    match state {
        Alert => crate::ui::theme::perm(),
        Error => crate::ui::theme::error(),
        Done => crate::ui::theme::accent(),
        _ => crate::ui::theme::agent(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every face must be exactly `AVATAR_W` cols wide so the avatar's
    /// position is stable across state transitions.
    #[test]
    fn every_state_has_uniform_width() {
        let states = [
            AvatarState::Idle,
            AvatarState::Thinking,
            AvatarState::Speaking,
            AvatarState::Reading,
            AvatarState::Writing,
            AvatarState::Bash,
            AvatarState::Alert,
            AvatarState::Error,
            AvatarState::Done,
        ];
        for state in states {
            for tick in [false, true] {
                let face = art(state, tick);
                assert_eq!(
                    face.chars().count(),
                    AVATAR_W,
                    "{:?} tick={} is {:?}",
                    state,
                    tick,
                    face,
                );
            }
        }
    }

    /// Tool-name → state mapping covers the common families.
    #[test]
    fn tool_name_maps_to_state() {
        assert_eq!(AvatarState::from_tool_name("read"), AvatarState::Reading);
        assert_eq!(AvatarState::from_tool_name("grep"), AvatarState::Reading);
        assert_eq!(AvatarState::from_tool_name("edit"), AvatarState::Writing);
        assert_eq!(AvatarState::from_tool_name("write"), AvatarState::Writing);
        assert_eq!(AvatarState::from_tool_name("bash"), AvatarState::Bash);
        // Unknown tools fall back to Reading.
        assert_eq!(
            AvatarState::from_tool_name("mcp_some_tool"),
            AvatarState::Reading
        );
    }

    /// Regression guard for the "avatar stuck on (O_O) Alert after a
    /// permission dialog resolves" bug. When the user lets a tool
    /// proceed, the UI loop resets the avatar via
    /// `from_tool_name(&ask_req.tool)`. That reset must always land on
    /// a working face — never on `Alert` (the prompt-time face) and
    /// never on `Done`/`Error`/`Idle` — for every tool that can ever
    /// be gated behind a permission prompt.
    #[test]
    fn permission_allow_reset_never_lands_on_alert() {
        let gated_tools = [
            "read",
            "grep",
            "find_files",
            "list_dir",
            "lsp",
            "semantic",
            "write",
            "edit",
            "apply_patch",
            "write_todo_list",
            "bash",
            "shell",
            "memory",
            "skill",
            "webfetch",
            "task",
            "mcp_tool:server:name",
        ];
        for tool in gated_tools {
            let state = AvatarState::from_tool_name(tool);
            assert!(
                matches!(
                    state,
                    AvatarState::Reading | AvatarState::Writing | AvatarState::Bash
                ),
                "tool {:?} reset to non-working avatar state {:?}",
                tool,
                state,
            );
            assert_ne!(
                state,
                AvatarState::Alert,
                "tool {:?} must not reset to the Alert face",
                tool,
            );
        }
    }

    /// Regression guard for GH #621: the `Done` `(^_^)` face must only appear
    /// when the session is genuinely idle. While a follow-up keeps the runner
    /// busy (plugin hook chain, `/plan` reviewer, drained interjection, or a
    /// background review) the settled face must be a working one, so a typed
    /// message isn't silently queued behind an idle-looking avatar.
    #[test]
    fn settled_shows_done_only_when_idle() {
        assert_eq!(AvatarState::settled(false), AvatarState::Done);
        assert_eq!(AvatarState::settled(true), AvatarState::Thinking);
        assert_ne!(
            AvatarState::settled(true),
            AvatarState::Done,
            "a busy runner must never settle on the idle Done face",
        );
    }
}
