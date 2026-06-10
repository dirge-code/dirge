//! Tab-completion helpers for slash commands.
//!
//! Two-phase completion:
//!   1. **Command name** (token 0, always the first word): cycles through
//!      built-in commands + plugin commands via `all_commands()`.
//!   2. **Subcommands** (token 1+): when the command has registered subcommand
//!      entries in `SUBCOMMAND_ENTRIES`, cycles through those values.
//!
//! `token_spans` replaces the earlier `token_at_cursor` + `token_byte_range`
//! pair — one tokenizer produces byte ranges; cursor-location and splice-range
//! both derive from the same output.

use std::sync::Mutex;

use crate::sync_util::LockExt;

/// Plugin-registered command names (without leading `/`).
/// Populated by `register_plugin_commands` after plugin init.
static PLUGIN_COMMANDS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Register plugin command names for tab completion.
/// Called from `main.rs` after plugin init. Only the `plugin` feature
/// has anything to register — without it the setter is dead (the
/// `PLUGIN_COMMANDS` static stays empty and `all_commands` just reads
/// through it), so gate it to avoid a dead-code error on plugin-less
/// builds like `windows-default`.
#[cfg(feature = "plugin")]
pub fn register_plugin_commands(cmds: Vec<String>) {
    *PLUGIN_COMMANDS.lock_ignore_poison() = cmds;
}

/// All completable slash commands: built-ins + plugin-registered.
/// Sorted, with leading `/`.
#[cfg(feature = "slash-completion")]
pub fn all_commands() -> Vec<String> {
    let mut cmds: Vec<String> = super::slash_command_names()
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    let guard = PLUGIN_COMMANDS.lock_ignore_poison();
    for name in guard.iter() {
        let with_slash = format!("/{}", name);
        if !cmds.contains(&with_slash) {
            cmds.push(with_slash);
        }
    }
    cmds.sort();
    cmds
}

/// Byte-range for each whitespace-delimited token in `input`.
/// Leading `/` is NOT stripped — token 0 is the literal command name
/// (e.g. `"/mode"`). Empty input produces an empty vec.
pub fn token_spans(input: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // skip whitespace
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        spans.push((start, i));
    }
    spans
}

/// Which token (by index) the cursor sits on or just after, plus
/// the position within that token's span that corresponds to the cursor.
/// Returns `(token_index, byte_offset_from_token_start)`.
/// If the cursor is before the first token, returns `(0, 0)`.
pub fn cursor_on_span(spans: &[(usize, usize)], cursor: usize) -> (usize, usize) {
    for (idx, &(start, end)) in spans.iter().enumerate() {
        if cursor >= start && cursor <= end {
            return (idx, cursor - start);
        }
        // cursor in whitespace GAP between this token and the next
        // (or after the last token): count it as "just after" the
        // preceding token.
        if cursor > end {
            if let Some(&(next_start, _)) = spans.get(idx + 1) {
                if cursor < next_start {
                    return (idx, end - start);
                }
            } else {
                // after the last token
                return (idx, end - start);
            }
        }
    }
    (0, 0)
}

/// Subcommand entries: `(command_name, &[candidate, ...])`.
/// Each entry maps a slash command (with leading `/`) to its completable
/// subcommand / argument values.
#[cfg(feature = "slash-completion")]
static SUBCOMMAND_ENTRIES: &[(&str, &[&str])] = &[
    ("/mode", &["standard", "restrictive", "accept", "yolo"]),
    ("/toggle", &["todo"]),
    ("/prompt", &["default"]),
    ("/agent", &["off"]),
    (
        "/sandbox",
        &["attach", "snapshot", "reboot", "start", "help"],
    ),
    (
        "/sandbox snapshot",
        &["save", "list", "restore", "delete", "help"],
    ),
    ("/sessions", &["delete"]),
    ("/tree", &[]),  // dynamic: leaf IDs
    ("/fork", &[]),  // dynamic: leaf IDs
    ("/clone", &[]), // dynamic: leaf IDs
    ("/allow", &["list", "add", "remove", "clear"]),
    ("/loop", &["start", "stop", "status"]),
    (
        "/debug",
        &[
            "launch",
            "attach",
            "step",
            "step_in",
            "step_out",
            "continue",
            "breakpoint",
            "bp",
            "evaluate",
            "eval",
            "sessions",
            "status",
            "terminate",
            "stop",
            "panel",
        ],
    ),
    (
        "/dap-repl",
        &[
            "launch",
            "attach",
            "bp",
            "c",
            "n",
            "s",
            "o",
            "p",
            "bt",
            "status",
            "terminate",
            "help",
        ],
    ),
    ("/panel", &["on", "off", "auto", "debug"]),
    ("/display", &[]), // dynamic: pane spec
    ("/kill", &[]),    // dynamic: subagent ID
    ("/cd", &[]),      // dynamic: directory path
    ("/btw", &[]),     // freeform
    ("/why", &[]),     // dynamic: tool name
];

/// Build the parent command name for subcommand lookup.
/// For token 1, this is just the command name (e.g. "/mode").
/// For deeper tokens, it's the slice through the previous token's end
/// (e.g. "/sandbox snapshot").
fn parent_command(input: &str, spans: &[(usize, usize)], token_idx: usize) -> String {
    if token_idx >= 2 {
        let parent_end = spans[token_idx - 1].1;
        input[0..parent_end].trim_end().to_string()
    } else {
        input[spans[0].0..spans[0].1].to_string()
    }
}

/// Return subcommand candidates for a (command, prefix) pair.
#[cfg(feature = "slash-completion")]
fn sub_candidates(command: &str, prefix: &str) -> Vec<String> {
    SUBCOMMAND_ENTRIES
        .iter()
        .find(|(cmd, _)| *cmd == command)
        .map(|(_, entries)| {
            entries
                .iter()
                .filter(|e| e.starts_with(prefix))
                .map(|e| e.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Cycle `current` through `candidates`. Returns the next candidate
/// after `current`, wrapping around. If `current` is not in the list,
/// returns the first candidate.
fn cycle_candidate(candidates: &[String], current: &str) -> Option<String> {
    if candidates.is_empty() {
        return None;
    }
    let pos = candidates.iter().position(|c| c == current);
    let next = match pos {
        Some(i) => (i + 1) % candidates.len(),
        None => 0,
    };
    Some(candidates[next].clone())
}

/// Result of a slash-command tab completion.
#[derive(Debug, Clone)]
pub struct CompletionResult {
    pub new_buffer: String,
    pub new_cursor: usize,
    /// The full sorted command list and the index of the currently-selected
    /// command, so the renderer can show a preview of upcoming items.
    pub all_commands: Vec<String>,
    pub current_index: usize,
}

/// Try to complete the slash command at `cursor` in `buffer`.
/// Returns `Some(CompletionResult)` if completion was possible.
///
/// Two-phase:
///   1. Cursor in token 0 → complete command name.
///   2. Cursor in token 1+ → complete subcommand/argument if registered.
#[cfg(feature = "slash-completion")]
pub fn try_complete(buffer: &str, cursor: usize) -> Option<CompletionResult> {
    if !buffer.starts_with('/') {
        return None;
    }
    let cursor = cursor.min(buffer.len());
    let spans = token_spans(buffer);
    let (token_idx, _offset) = cursor_on_span(&spans, cursor);

    if spans.is_empty() {
        return None;
    }

    let cmd_span = spans[0];
    let command = &buffer[cmd_span.0..cmd_span.1];

    // If the cursor sits in whitespace past the last known token (e.g.
    // "/mode █"), treat it as the start of the next token — subcommand
    // position — rather than the tail of token 0.
    let token_idx = if token_idx == spans.len().saturating_sub(1) && cursor > spans[token_idx].1 {
        token_idx + 1
    } else {
        token_idx
    };

    // Phase 1: completing the command name (token 0).
    if token_idx == 0 {
        let all_cmds = all_commands();
        let matching: Vec<String> = all_cmds
            .iter()
            .filter(|c| c.starts_with(command))
            .cloned()
            .collect();

        if matching.is_empty() {
            return None;
        }

        let is_exact = all_cmds.contains(&command.to_string());
        let (replacement, current_index) = if is_exact {
            let all_idx = all_cmds.iter().position(|c| c == command);
            let next_idx = match all_idx {
                Some(i) => (i + 1) % all_cmds.len(),
                None => 0,
            };
            (all_cmds[next_idx].clone(), next_idx)
        } else {
            let current_idx = matching.iter().position(|c| c == command);
            let next_idx = match current_idx {
                Some(i) => (i + 1) % matching.len(),
                None => 0,
            };
            let cmd = matching[next_idx].clone();
            let all_idx = all_cmds.iter().position(|c| *c == cmd).unwrap_or(0);
            (cmd, all_idx)
        };

        let mut new_buffer = String::with_capacity(replacement.len() + buffer.len() - cmd_span.1);
        new_buffer.push_str(&replacement);
        new_buffer.push_str(&buffer[cmd_span.1..]);
        let new_cursor = replacement.len();
        return Some(CompletionResult {
            new_buffer,
            new_cursor,
            all_commands: all_cmds,
            current_index,
        });
    }

    // Phase 2: completing a subcommand/argument (token 1+).
    let candidates = sub_candidates(command, "");
    if candidates.is_empty() {
        return None;
    }

    // Determine which sub-token we're on: if token_idx is 1, the user
    // is typing the first subcommand. If token_idx > 1, we're deeper
    // and build the composite parent command (e.g. "/sandbox snapshot").
    let parent_cmd = parent_command(buffer, &spans, token_idx);

    // If the cursor is past all tokens (empty next token, e.g. "/mode █"),
    // synthesize an empty span at the cursor position.
    let sub_span: (usize, usize);
    let current_sub: &str;
    if token_idx < spans.len() {
        sub_span = spans[token_idx];
        current_sub = &buffer[sub_span.0..sub_span.1];
    } else {
        sub_span = (cursor, cursor);
        current_sub = "";
    }

    // Re-resolve candidates for the potentially-composite parent.
    let candidates = sub_candidates(&parent_cmd, current_sub);
    if candidates.is_empty() || (candidates.len() == 1 && candidates[0] == current_sub) {
        // Exact match or single match that equals current — cycle all
        // sub-entries for the parent so the user can reach the next entry.
        let all_for_parent = sub_candidates(&parent_cmd, "");
        if all_for_parent.is_empty() {
            return None;
        }
        return cycle_subcommand(buffer, sub_span, &all_for_parent, current_sub);
    }

    cycle_subcommand(buffer, sub_span, &candidates, current_sub)
}

/// Cycle the subcommand at `token_idx` through `candidates`.
/// Builds the new buffer by replacing the token span with the next candidate.
#[cfg(feature = "slash-completion")]
fn cycle_subcommand(
    buffer: &str,
    span: (usize, usize),
    candidates: &[String],
    current: &str,
) -> Option<CompletionResult> {
    let next = cycle_candidate(candidates, current)?;
    let mut new_buffer = String::with_capacity(buffer.len() + next.len() - (span.1 - span.0));
    new_buffer.push_str(&buffer[..span.0]);
    new_buffer.push_str(&next);
    new_buffer.push_str(&buffer[span.1..]);
    let new_cursor = span.0 + next.len();

    // Preview shows all sub-candidates for the parent command.
    let all_cmds = candidates.to_vec();
    let current_index = candidates.iter().position(|c| c == &next).unwrap_or(0);

    Some(CompletionResult {
        new_buffer,
        new_cursor,
        all_commands: all_cmds,
        current_index,
    })
}

/// Ghost suffix for slash commands: the completion tail for a unique
/// prefix match.  Works for both command names and subcommands.
///
/// - 0 words: `/mod` + Right → `/mode`
/// - 1 word, exact command: `/mode ` + Right → no suffix (use Tab for args)
/// - 1 word, unique prefix: `/mode sta` + Right → `ndard`
#[cfg(feature = "slash-completion")]
pub fn ghost_suffix(input: &str) -> Option<String> {
    if !input.starts_with('/') || input.len() < 2 {
        return None;
    }

    let spans = token_spans(input);

    // No tokens yet — just the slash. Too ambiguous.
    if spans.is_empty() {
        return None;
    }

    let cmd_span = spans[0];
    let command = &input[cmd_span.0..cmd_span.1];

    // If the cursor is still in the command-name token (no space after):
    // complete the command name.
    if spans.len() == 1 {
        // Already an exact command — nothing to ghost.
        let all_cmds = all_commands();
        if all_cmds.contains(&command.to_string()) {
            return None;
        }
        return all_cmds
            .iter()
            .find(|c| c.len() > command.len() && c.starts_with(command))
            .map(|c| c[command.len()..].to_string());
    }

    // The user has typed at least one argument.
    // Find the unique subcommand prefix match.
    let token_idx = spans.len() - 1;
    let sub_span = spans[token_idx];
    let current_sub = &input[sub_span.0..sub_span.1];

    // Build parent command (may be composite like "/sandbox snapshot").
    let parent_cmd = parent_command(input, &spans, token_idx);

    let candidates = sub_candidates(&parent_cmd, current_sub);
    if candidates.len() == 1 && candidates[0] != current_sub {
        return Some(candidates[0][current_sub.len()..].to_string());
    }
    None
}

/// Format a completion preview string showing upcoming commands.
/// Returns an empty string when `cr` is `None`. The result is shaped
/// to fit within `avail_w` display cells (after the continuation
/// prompt), showing as many upcoming command names as will fit.
#[cfg(feature = "slash-completion")]
pub fn format_completion_preview(cr: Option<&CompletionResult>, avail_w: usize) -> String {
    let cr = match cr {
        Some(c) => c,
        None => return String::new(),
    };
    if cr.all_commands.is_empty() || avail_w < 4 {
        return String::new();
    }
    let all = &cr.all_commands;
    let start = (cr.current_index + 1) % all.len();
    let mut result = String::new();
    for i in 0..all.len() {
        let cmd = &all[(start + i) % all.len()];
        let candidate = if result.is_empty() {
            cmd.clone()
        } else {
            format!("{result}  {cmd}")
        };
        use unicode_width::UnicodeWidthStr;
        if UnicodeWidthStr::width(candidate.as_str()) > avail_w {
            break;
        }
        result = candidate;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_spans_simple() {
        assert_eq!(token_spans("/mode standard"), vec![(0, 5), (6, 14)]);
    }

    #[test]
    fn token_spans_single() {
        assert_eq!(token_spans("/mode"), vec![(0, 5)]);
    }

    #[test]
    fn token_spans_empty() {
        assert_eq!(token_spans(""), vec![]);
    }

    #[test]
    fn token_spans_extra_whitespace() {
        assert_eq!(token_spans("  /mode   standard  "), vec![(2, 7), (10, 18)]);
    }

    #[test]
    fn cursor_on_first_token() {
        let spans = token_spans("/mode standard");
        assert_eq!(cursor_on_span(&spans, 0), (0, 0));
        assert_eq!(cursor_on_span(&spans, 3), (0, 3));
        assert_eq!(cursor_on_span(&spans, 5), (0, 5));
    }

    #[test]
    fn cursor_on_second_token() {
        let spans = token_spans("/mode standard");
        assert_eq!(cursor_on_span(&spans, 6), (1, 0));
        assert_eq!(cursor_on_span(&spans, 10), (1, 4));
        assert_eq!(cursor_on_span(&spans, 14), (1, 8));
    }

    #[test]
    fn cursor_between_tokens() {
        let spans = token_spans("/mode standard");
        // cursor in the whitespace gap counts as "just after" token 0
        assert_eq!(cursor_on_span(&spans, 5), (0, 5));
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn ghost_suffix_completes_command() {
        assert_eq!(ghost_suffix("/disp").as_deref(), Some("lay"));
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn ghost_suffix_subcommand() {
        assert_eq!(ghost_suffix("/mode sta").as_deref(), Some("ndard"));
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn ghost_suffix_returns_none_when_not_completable() {
        assert_eq!(ghost_suffix("/"), None);
        assert_eq!(ghost_suffix("not-a-command"), None);
        assert_eq!(ghost_suffix("/zzzznope"), None);
        assert_eq!(ghost_suffix("/mode standard"), None); // exact, not a prefix
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn subcommand_completion_cycles() {
        let r = try_complete("/mode ", 6).unwrap();
        assert!(r.new_buffer.starts_with("/mode "));
        assert!(r.new_buffer.len() > 6);
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn subcommand_cycles_past_first_match() {
        // First tab from empty subcommand: picks first candidate.
        let r1 = try_complete("/mode ", 6).unwrap();
        assert_eq!(r1.new_buffer, "/mode standard");
        // Second tab: should cycle to the next, not stay stuck on "standard".
        let r2 = try_complete(&r1.new_buffer, r1.new_cursor).unwrap();
        assert_eq!(r2.new_buffer, "/mode restrictive");
        // Third tab: cycles further.
        let r3 = try_complete(&r2.new_buffer, r2.new_cursor).unwrap();
        assert_eq!(r3.new_buffer, "/mode accept");
        // Wraps around.
        let r4 = try_complete(&r3.new_buffer, r3.new_cursor).unwrap();
        assert_eq!(r4.new_buffer, "/mode yolo");
        let r5 = try_complete(&r4.new_buffer, r4.new_cursor).unwrap();
        assert_eq!(r5.new_buffer, "/mode standard");
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn subcommand_ghost_for_unique_prefix() {
        // "/sandbox sna" → unique match "snapshot"
        assert_eq!(ghost_suffix("/sandbox sna").as_deref(), Some("pshot"));
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn complete_partial_command() {
        let r = try_complete("/mod", 4).unwrap();
        assert_eq!(r.new_buffer, "/mode");
        assert_eq!(r.new_cursor, 5);
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn empty_buffer_returns_none() {
        assert!(try_complete("", 0).is_none());
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn no_completion_without_slash() {
        assert!(try_complete("hello", 5).is_none());
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn unknown_command_returns_none() {
        assert!(try_complete("/nonexistent", 12).is_none());
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn sub_candidates_for_mode() {
        let c = sub_candidates("/mode", "");
        assert_eq!(c, vec!["standard", "restrictive", "accept", "yolo"]);
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn sub_candidates_filtered() {
        let c = sub_candidates("/mode", "s");
        assert_eq!(c, vec!["standard"]);
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn plugin_command_in_all_commands() {
        register_plugin_commands(vec!["myplugin".to_string()]);
        let cmds = all_commands();
        assert!(cmds.contains(&"/myplugin".to_string()));
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn plugin_command_completion_cycles() {
        register_plugin_commands(vec!["myplugin".to_string()]);
        // "/mypl" should complete to "/myplugin"
        let r = try_complete("/mypl", 5).unwrap();
        assert_eq!(r.new_buffer, "/myplugin");
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn plugin_command_ghost_suffix() {
        register_plugin_commands(vec!["myplugin".to_string()]);
        assert_eq!(ghost_suffix("/mypl").as_deref(), Some("ugin"));
    }
}
