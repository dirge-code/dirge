//! Inert-command classification — the "spinning on no-ops" signal (#808).
//!
//! Every existing loop guard keys on something the model *does*: the storm
//! breaker needs a call repeated with IDENTICAL args
//! ([`super::storm`]), the failure tracker needs errored results
//! ([`super::failure_tracker`]), the file-touch tracker needs the same
//! file edited over and over ([`super::context_depth`]), and the progress
//! monitor only judges a turn BOUNDARY and is capped at two nudges
//! ([`super::progress`]).
//!
//! A model that has lost the thread slips between all of them. The
//! reported case (#808): the model decided it needed the `memory` tool,
//! could not bring itself to emit that call, and instead issued a run of
//! shell commands that did nothing at all —
//!
//! ```text
//! echo "ready"          →  ready
//! echo done             →  done
//! echo ok               →  ok
//! true                  →  (no output)
//! echo "switching to memory tool"
//! echo "final check before memory call"
//! ```
//!
//! — narrating "I keep calling bash, I need to call the memory tool"
//! between each one. Storm never fired because the echoed strings all
//! differed. The failure tracker never fired because every command
//! *succeeded*. Nothing was edited, so context-depth saw nothing. The run
//! burned turns until the cap.
//!
//! What ties those calls together is not their arguments but their
//! *effect*: none. An inert command changes no state and returns no
//! information the model did not already hold — it echoes back a literal
//! the model itself just wrote. Collapsing them all onto ONE storm
//! signature ([`INERT_ARGS`]) is what lets the existing repeat-loop guard
//! see a spin of varied no-ops as the single repeated call it actually is.
//!
//! ## Conservative by construction
//!
//! A false positive here costs real work, so the classifier only says
//! "inert" for shapes it can prove: literal `echo`/`printf`, the null
//! builtins (`true`, `false`, `:`), and sequences of exactly those. The
//! segment scan is deliberately naive about quoting — splitting on `;`,
//! `&&`, `||` and newlines without tracking quotes can only ever *break*
//! a segment into pieces that fail the exact-token match below, so
//! misparsing a quoted separator turns an inert verdict into a
//! non-inert one and never the reverse. Anything reaching outside the
//! process — a substitution, a redirect, a pipe, a subshell, a
//! backgrounded job — disqualifies the whole command.

use super::tools::ToolCall;

/// The synthetic storm signature every inert command collapses onto.
/// Not valid JSON, so it can never collide with a real
/// [`super::message::canonical_json`] argument blob.
pub const INERT_ARGS: &str = "<inert-command>";

/// Shell metacharacters that take a command outside "prints a literal I
/// already knew". `$` and a backtick substitute in state we can't see;
/// `<`, `>` redirect; `|` pipes the output somewhere it can matter; `&`
/// backgrounds; parens and braces open a subshell or group whose contents
/// this scanner does not read.
const OUTWARD_CHARS: &[char] = &['$', '`', '<', '>', '|', '&', '(', ')', '{', '}'];

/// Builtins that do nothing and say nothing.
const NULL_BUILTINS: &[&str] = &["true", "false", ":"];

/// Verbs whose only effect is to print their (literal) arguments back.
const ECHO_VERBS: &[&str] = &["echo", "printf"];

/// True when `command` cannot change any state or surface any information
/// the caller did not already have.
///
/// See the module docs for why this is deliberately narrow. An empty or
/// whitespace-only command is NOT inert: it is a malformed call, which is
/// the tool-input-repair path's business, not this one's.
pub fn is_inert_command(command: &str) -> bool {
    let mut saw_segment = false;
    for segment in split_segments(command) {
        let segment = segment.trim();
        if segment.is_empty() {
            // A trailing `;` or a blank line between statements.
            continue;
        }
        if !is_inert_segment(segment) {
            return false;
        }
        saw_segment = true;
    }
    saw_segment
}

/// True when `call` is a `bash` invocation whose command is inert.
///
/// Keyed on the tool NAME rather than a permission operation: this is a
/// shell-syntax judgement, and applying it to some future `Execute` tool
/// with different argument semantics would be reading a `command` field
/// that means something else.
pub fn is_inert_call(call: &ToolCall) -> bool {
    if call.name != "bash" {
        return false;
    }
    // A backgrounded shell is never inert regardless of its command: it
    // returns a shell id the model can poll, which is state.
    let background = call.arguments.get("background").and_then(|v| v.as_bool());
    if background == Some(true) {
        return false;
    }
    let command = call.arguments.get("command").and_then(|v| v.as_str());
    command.is_some_and(is_inert_command)
}

/// Split on the statement separators `;`, `&&`, `||` and newlines.
///
/// Quote-blind on purpose — see the module docs. `&&` and `||` are
/// matched before the single `&`/`|` they contain so a legitimate
/// separator isn't left behind as a disqualifying [`OUTWARD_CHARS`]
/// character in the segment.
fn split_segments(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let (is_sep, width) = match bytes[i] {
            b'&' if bytes.get(i + 1) == Some(&b'&') => (true, 2),
            b'|' if bytes.get(i + 1) == Some(&b'|') => (true, 2),
            b';' | b'\n' => (true, 1),
            _ => (false, 1),
        };
        if is_sep {
            out.push(&command[start..i]);
            i += width;
            start = i;
        } else {
            i += width;
        }
    }
    out.push(&command[start..]);
    out
}

/// True when one already-trimmed, non-empty statement is inert.
fn is_inert_segment(segment: &str) -> bool {
    if segment.contains(OUTWARD_CHARS) {
        return false;
    }
    if NULL_BUILTINS.contains(&segment) {
        return true;
    }
    let verb = segment.split_whitespace().next().unwrap_or("");
    // `echo` alone prints a newline; `echo <literals>` prints them back.
    // Either way the model learns nothing, and OUTWARD_CHARS above has
    // already ruled out substitutions and redirects in the arguments.
    ECHO_VERBS.contains(&verb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(command: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            arguments: json!({ "command": command }),
        }
    }

    #[test]
    fn the_commands_from_the_report_are_all_inert() {
        // Verbatim from #808 — the run that no guard could see.
        for cmd in [
            "echo \"ready\"",
            "echo done",
            "echo ok",
            "true",
            "echo \"switching to memory tool\"",
            "echo \"final check before memory call\"",
        ] {
            assert!(is_inert_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn null_builtins_and_sequences_of_them_are_inert() {
        assert!(is_inert_command(":"));
        assert!(is_inert_command("false"));
        assert!(is_inert_command("true; true"));
        assert!(is_inert_command("echo a && true"));
        assert!(is_inert_command("echo a\necho b\n"));
        assert!(is_inert_command("  echo spaced  ;  "));
        assert!(is_inert_command("printf 'hello'"));
    }

    #[test]
    fn real_work_is_never_inert() {
        for cmd in [
            "cargo test",
            "git status",
            "ls",
            "pwd",
            "date",
            "cat src/main.rs",
            // Anything reaching outward, even wrapped around an echo.
            "echo hi > file.txt",
            "echo $HOME",
            "echo `hostname`",
            "echo hi | wc -l",
            "echo hi &",
            "(echo hi)",
            "python3 - << 'PYEOF'\nprint(\"x\")\nPYEOF",
            // From #808: this one genuinely attempted something.
            "cd /home/x && memory_replace 2>/dev/null || echo \"no shell memory tool\"",
            // A real command anywhere in the sequence disqualifies it.
            "echo starting && cargo build",
            "true; rm -rf build",
        ] {
            assert!(!is_inert_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn quote_blind_splitting_only_ever_errs_toward_not_inert() {
        // A separator inside quotes splits a segment into pieces that
        // fail the exact-token match, so the verdict is "not inert".
        // Wrong, but wrong in the safe direction.
        assert!(!is_inert_command("echo \"a; true\""));
        assert!(!is_inert_command("echo 'x && true'"));
    }

    #[test]
    fn an_empty_command_is_not_inert() {
        assert!(!is_inert_command(""));
        assert!(!is_inert_command("   \n  "));
        assert!(!is_inert_command(";;"));
    }

    #[test]
    fn only_bash_calls_are_classified() {
        assert!(is_inert_call(&bash("echo ok")));
        assert!(!is_inert_call(&bash("cargo test")));

        // Same argument shape, different tool: not our judgement to make.
        let mut other = bash("echo ok");
        other.name = "shell_plugin".to_string();
        assert!(!is_inert_call(&other));

        // Missing or non-string `command`.
        let mut malformed = bash("echo ok");
        malformed.arguments = json!({});
        assert!(!is_inert_call(&malformed));
    }

    #[test]
    fn a_backgrounded_shell_is_never_inert() {
        // It returns a pollable shell id, which is state the model gains.
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            arguments: json!({ "command": "echo ok", "background": true }),
        };
        assert!(!is_inert_call(&call));
    }
}
