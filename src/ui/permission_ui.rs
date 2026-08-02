const ALLOW_PLACEHOLDER: &str = "<edit this pattern>";

/// Whether a pattern was returned by `suggest_pattern` as the
/// "empty input — please type a real pattern" placeholder rather
/// than a real glob. Used by the ask-dialog to detect when the
/// user pressed "allow always" on a degenerate input and refuse
/// to store the placeholder as an actual allowlist entry.
pub(crate) fn is_placeholder_pattern(p: &str) -> bool {
    p == ALLOW_PLACEHOLDER
}

/// Why "allow always" can't produce a usable grant for this input, or `None`
/// when it can. The dialog prints this and downgrades to allow-once.
///
/// dirge-jktn: the complex-command arm exists because the engine REFUSES to
/// honor a session grant on a command containing substitution / a subshell /
/// arithmetic expansion (`SessionAllowlistPolicy::decide`, dirge-g9qj — the
/// inner command is invisible, so a head-shaped grant like `echo *` must not
/// cover `echo $(rm -rf ~)`). Offering "allow always" anyway saved an entry
/// that could never match, told the user it was saved, and then re-prompted
/// on the very next identical command.
pub(crate) fn allow_always_downgrade_reason(tool: &str, input: &str) -> Option<&'static str> {
    if input.trim().is_empty() {
        return Some("can't derive a useful pattern from empty input");
    }
    if tool == "bash" && crate::semantic::adapters::bash::command_is_complex(input) {
        return Some(
            "commands with shell substitution or a subshell are never covered by a saved rule \
             (the inner command can't be inspected), so this can only be allowed once",
        );
    }
    None
}

/// Whether a bash segment is ALREADY authorized by the built-in rules, so
/// an allow-always grant derived from it would add nothing.
///
/// Last-match-wins over `default_bash_rules`, mirroring how the engine orders
/// them (`permission::engine::build`), and only `Allow` counts — a `Deny` rule
/// matching the segment obviously doesn't make it permitted.
fn segment_already_allowed(segment: &str) -> bool {
    use crate::permission::{Action, default_bash_rules, pattern::Pattern};
    default_bash_rules()
        .into_iter()
        .rfind(|(pat, _)| Pattern::new_command(pat).matches(segment))
        .is_some_and(|(_, action)| action == Action::Allow)
}

/// Split a bash line into command segments.
///
/// Prefers the same tree-sitter splitter the permission layer itself runs
/// (`parse_bash_segments_full`), so the suggestion is derived from the exact
/// segments that were authorized — a quoted `|` or a heredoc body can't
/// manufacture a phantom segment. Falls back to the coarse separator split
/// when that parser is unavailable or declines to decompose the command.
fn bash_segments(command: &str) -> Vec<String> {
    if let Ok((segments, complex)) =
        crate::semantic::adapters::bash::parse_bash_segments_full(command)
        && !complex
        && segments.len() > 1
    {
        return segments;
    }
    command
        .split(['&', '|', ';', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Find the head (first word) of the first command segment in a bash line that
/// is NOT already auto-allowed. Used so an allow-always suggestion targets the
/// command that actually needs permission (e.g. `python3` in `cd /x &&
/// python3 …`) rather than a prefix that already passes. Returns `None` when
/// every segment already passes (then the caller falls back to the first
/// token).
///
/// dirge-mirm: this used to consult a hardcoded 9-entry list of shell
/// builtins, while `default_bash_rules` auto-allows ~22 more (`cat`, `ls`,
/// `grep`, `echo`, `head`, `tail`, `rg`, `find`, …). The two drifted, and a
/// compound led by one of the missing entries — `cat f.txt | clojure -M -` —
/// produced a suggestion the defaults already covered. "Allow always" then
/// saved a rule that granted nothing and the blocking segment re-prompted on
/// every invocation. Asking the rule set directly makes that class of drift
/// impossible: whatever `default_bash_rules` allows is skipped, by
/// construction.
///
/// `source`/`.` are still correctly targeted rather than skipped — they
/// execute arbitrary script code and no built-in rule allows them.
fn significant_bash_head(command: &str) -> Option<String> {
    bash_segments(command).into_iter().find_map(|seg| {
        (!segment_already_allowed(&seg))
            .then(|| seg.split_whitespace().next().map(str::to_string))
            .flatten()
    })
}

pub(crate) fn suggest_pattern(tool: &str, input: &str) -> String {
    // Refuse to suggest a catch-all wildcard for empty / whitespace-
    // only input. A user mis-clicking "(a) allow always" on an empty
    // invocation would otherwise pin an "allow everything for this
    // tool forever" rule into their session. The placeholder string
    // is intentionally not a valid glob — the UI shows it as the
    // suggested pattern, the user edits it before confirming.
    const PLACEHOLDER: &str = ALLOW_PLACEHOLDER;
    let trimmed = input.trim();
    // Covers empty input AND commands no session grant can ever match
    // (dirge-jktn); both downgrade the dialog to allow-once.
    if allow_always_downgrade_reason(tool, input).is_some() {
        return PLACEHOLDER.to_string();
    }
    match tool {
        "bash" => {
            // Base the suggestion on the first segment that actually needs
            // permission, not literally the first token. A compound command
            // is split into a permission claim per segment, so a suggestion
            // derived from an already-allowed prefix saves a rule that
            // covers nothing while the blocking segment keeps prompting —
            // `cd /x && python3 …` must yield `python3 *`, not `cd *`.
            let head = significant_bash_head(trimmed).unwrap_or_else(|| {
                trimmed
                    .split_whitespace()
                    .next()
                    .unwrap_or(PLACEHOLDER)
                    .to_string()
            });
            format!("{} *", head)
        }
        // Path-arg tools: suggest a `<parent>/**` glob from the input
        // path. One arm for all of them — previously read/write/edit/
        // list_dir, apply_patch, and the semantic tools each had an
        // identical copy of this body (dirge-t1wh).
        "read" | "write" | "edit" | "list_dir" | "apply_patch" | "list_symbols"
        | "get_symbol_body" | "find_definition" | "find_callers" | "find_callees" => {
            let path = std::path::Path::new(trimmed);
            let parent = path
                .parent()
                .map(|p| p.to_string_lossy())
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            if parent.is_empty() {
                "**".to_string()
            } else {
                format!("{}/**", parent)
            }
        }
        "grep" | "find_files" => {
            let first = trimmed.split_whitespace().next().unwrap_or(PLACEHOLDER);
            format!("{}*", first)
        }
        "mcp_tool" => {
            let mut parts = trimmed.splitn(3, ':');
            let umbrella = parts.next().unwrap_or("");
            let server = parts.next().unwrap_or("");
            if umbrella.eq_ignore_ascii_case("mcp_tool") && !server.is_empty() {
                format!("mcp_tool:{}:*", server)
            } else {
                PLACEHOLDER.to_string()
            }
        }
        "webfetch" => "webfetch:*".to_string(),
        "websearch" => "websearch:*".to_string(),
        "task" | "task_status" | "question" => "**".to_string(),
        "glob" | "repo_overview" | "skill" | "memory" | "write_todo_list" | "lsp" => {
            "**".to_string()
        }
        _ => PLACEHOLDER.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `suggest_pattern` returns a literal placeholder for empty
    /// input. The ask-dialog path that consumes it must detect the
    /// placeholder and refuse to add it as an allowlist entry —
    /// otherwise pressing "a" (allow always) on an empty invocation
    /// would silently store `<edit this pattern>` as a real pattern.
    /// The detection is exposed via `is_placeholder_pattern` so the
    /// dialog code is unit-testable.
    #[test]
    fn placeholder_pattern_is_detectable() {
        let p = suggest_pattern("bash", "");
        assert!(
            is_placeholder_pattern(&p),
            "empty input should yield a detectable placeholder; got {p:?}",
        );
        let p = suggest_pattern("grep", "  \t  ");
        assert!(is_placeholder_pattern(&p));
        // A legit suggestion is NOT flagged as a placeholder.
        let p = suggest_pattern("bash", "cargo test");
        assert!(!is_placeholder_pattern(&p), "real pattern flagged: {p:?}");
    }

    // Whitespace-only or empty input must NOT collapse to a "* *"
    // / "*" wildcard pattern that matches every subsequent call.
    // The audit flagged this as a footgun: a user accidentally
    // hitting "(a) allow always" on an empty bash invocation would
    // permanently auto-allow ALL bash. Now we return a literal
    // placeholder + the user has to type the pattern themselves.
    #[test]
    fn suggest_pattern_refuses_wildcard_on_empty_input() {
        // Bash: empty / whitespace input should NOT yield "* *".
        let p = suggest_pattern("bash", "");
        assert_ne!(p, "* *", "empty bash input must not yield catch-all");
        assert!(
            !p.contains('*'),
            "empty input should not contain wildcards: {p:?}"
        );

        let p = suggest_pattern("bash", "   \t  ");
        assert_ne!(
            p, "* *",
            "whitespace-only bash input must not yield catch-all"
        );
        assert!(
            !p.contains('*'),
            "ws-only input should not contain wildcards: {p:?}"
        );

        // grep / find_files: same — empty must not yield "*"
        let p = suggest_pattern("grep", "");
        assert!(
            !p.contains('*'),
            "empty grep input must not yield wildcard: {p:?}"
        );

        // Unknown tool with empty input shouldn't yield catch-all.
        let p = suggest_pattern("mcp_tool:foo", "");
        assert!(!p.contains('*'), "unknown tool empty input: {p:?}");
    }

    /// A compound command with a benign `cd` prefix must suggest the
    /// SIGNIFICANT command, not `cd *` (which is already auto-allowed and
    /// leaves the real command prompting forever). Regression for the
    /// "permission keeps re-asking" report.
    #[test]
    fn compound_bash_suggests_significant_command_not_cd() {
        assert_eq!(
            suggest_pattern("bash", "cd /tmp/proj && python3 gen.py"),
            "python3 *"
        );
        // Heredoc body (with its own punctuation) doesn't confuse the head pick.
        assert_eq!(
            suggest_pattern(
                "bash",
                "cd src && python3 - <<PY\nwith open('a','w') as f: f.write(x)\nPY"
            ),
            "python3 *"
        );
        // Multiple benign prefixes are all skipped.
        // NOTE: the trailing command must be one that genuinely needs
        // approval. This case used to end in `npm run build`, which the
        // built-in `npm run **` rule already allows — so the asserted `npm *`
        // was itself a dead-or-over-broad suggestion (it would have granted
        // `npm install`/`npm publish` for a command that needed no grant at
        // all). `npm install` is deliberately NOT auto-allowed, so it's a
        // real target (dirge-mirm).
        assert_eq!(
            suggest_pattern("bash", "export X=1 && cd app && npm install"),
            "npm *"
        );
        // A plain significant command is unchanged.
        assert_eq!(suggest_pattern("bash", "cargo test --all"), "cargo *");
        // cd-only (no significant segment) falls back to the first token.
        assert_eq!(suggest_pattern("bash", "cd /tmp"), "cd *");
    }

    /// dirge-9zbd: `source`/`.` execute arbitrary script code and are NOT
    /// auto-allowed, so they must NOT be skipped — the suggestion targets
    /// them, so granting it covers the (otherwise un-allowed) source while
    /// any default-allowed sibling (`python …`) already passes.
    #[test]
    fn source_is_the_suggestion_target_not_skipped() {
        assert_eq!(
            suggest_pattern("bash", "source venv/bin/activate && python app.py"),
            "source *"
        );
        assert_eq!(suggest_pattern("bash", ". ./env.sh && cargo run"), ". *");
        // But genuinely-benign, auto-allowed prefixes ARE still skipped.
        assert_eq!(
            suggest_pattern("bash", "export TOKEN=x && unset Y && mycli run"),
            "mycli *"
        );
    }

    /// dirge-mirm: the skip-set was a hardcoded list of 9 shell builtins while
    /// `default_bash_rules` auto-allows ~22 more (`cat`, `ls`, `grep`, `echo`,
    /// …). A compound led by one of those resolved to a suggestion the default
    /// rules ALREADY cover, so "allow always" saved a rule that granted
    /// nothing and the blocking segment re-prompted forever.
    ///
    /// Reported case: `cat f.txt | clojure -M -` suggested `cat *` (subsumed
    /// by the built-in `cat **`), so every subsequent invocation asked again.
    #[test]
    fn compound_skips_every_already_allowed_prefix_not_just_builtins() {
        assert_eq!(
            suggest_pattern("bash", "cat f.txt | clojure -M -"),
            "clojure *",
            "the reported re-prompt case",
        );
        assert_eq!(suggest_pattern("bash", "ls src | xargs wc"), "xargs *");
        assert_eq!(
            suggest_pattern("bash", "grep -l TODO . | xargs sed -i s/a/b/"),
            "xargs *"
        );
        assert_eq!(suggest_pattern("bash", "echo hi | mycli stdin"), "mycli *");
        // A default-allowed SUBCOMMAND rule is honored at segment granularity:
        // `git status` passes, bare `git` does not, so a compound with an
        // un-allowed git verb targets the git segment rather than skipping it.
        assert_eq!(
            suggest_pattern("bash", "git status && git push origin main"),
            "git *"
        );
    }

    /// Drift guard for the above: derived from `default_bash_rules` itself, so
    /// adding a rule there can never silently reintroduce the dead-suggestion
    /// bug. For every built-in Allow rule, a compound of "that command, then
    /// something un-allowed" must target the un-allowed part.
    #[test]
    fn suggestion_never_lands_on_an_already_allowed_head() {
        use crate::permission::{Action, default_bash_rules};
        for (pat, action) in default_bash_rules() {
            if action != Action::Allow {
                continue;
            }
            // Concrete invocation of the rule: drop the trailing glob.
            let invocation = pat
                .trim_end_matches("**")
                .trim_end_matches('*')
                .trim()
                .to_string();
            if invocation.is_empty() {
                continue;
            }
            let cmd = format!("{invocation} && zzunallowed --go");
            assert_eq!(
                suggest_pattern("bash", &cmd),
                "zzunallowed *",
                "rule {pat:?} left the suggestion on an already-allowed prefix",
            );
        }
    }

    /// dirge-jktn: the engine never honors a session grant on a complex
    /// command (substitution / subshell / arithmetic expansion) — see
    /// `SessionAllowlistPolicy::decide`, dirge-g9qj. So offering "allow
    /// always" for one saves an entry that provably cannot match, prints a
    /// confirmation that it was saved, and re-prompts on the very next
    /// identical invocation. Suppress the suggestion instead, so the dialog
    /// takes its existing downgrade-to-allow-once path.
    #[test]
    fn complex_commands_get_no_allow_always_pattern() {
        for cmd in [
            "echo $(date)",
            "rm -rf $(cat /tmp/target)",
            "ls `which python3`",
            "foo <(bar)",
        ] {
            let p = suggest_pattern("bash", cmd);
            assert!(
                is_placeholder_pattern(&p),
                "a grant for {cmd:?} could never fire, so none should be offered; got {p:?}",
            );
        }
        // A plain compound is unaffected — grants for these DO fire.
        assert!(!is_placeholder_pattern(&suggest_pattern(
            "bash",
            "cat f.txt | clojure -M -"
        )));
        assert!(!is_placeholder_pattern(&suggest_pattern(
            "bash",
            "cargo test --all"
        )));
    }

    /// The reason string shown on the downgrade must match why it happened —
    /// the empty-input path and the complex-command path are different facts,
    /// and telling a user their substitution command had "empty input" would
    /// be nonsense.
    #[test]
    fn allow_always_downgrade_reasons_are_distinct_and_accurate() {
        let empty = allow_always_downgrade_reason("bash", "");
        let complex = allow_always_downgrade_reason("bash", "echo $(date)");
        assert!(empty.is_some() && complex.is_some());
        assert_ne!(empty, complex);
        assert!(
            complex.unwrap().contains("substitution"),
            "complex reason should name the cause: {complex:?}",
        );
        // A grantable command has no downgrade reason at all.
        assert_eq!(allow_always_downgrade_reason("bash", "cargo test"), None);
    }

    // Non-empty inputs still produce the expected suggestion.
    #[test]
    fn suggest_pattern_works_for_non_empty_inputs() {
        assert_eq!(suggest_pattern("bash", "cargo test --all"), "cargo *");
        assert_eq!(suggest_pattern("grep", "fn foo bar"), "fn*");
    }

    /// User-reported bug: "allow always" on a write inside `src/`
    /// stored `src/*` (single `*`, no slash-spanning), so the next
    /// write under `src/agent/…` re-prompted. Maki's equivalent
    /// (`maki-agent/src/permissions.rs:519`) uses `parent/**`. Pin
    /// that the fix is in place for every path-shaped tool.
    #[test]
    fn suggest_pattern_path_tools_use_recursive_glob() {
        assert_eq!(suggest_pattern("write", "src/main.rs"), "src/**");
        assert_eq!(suggest_pattern("edit", "src/main.rs"), "src/**");
        assert_eq!(
            suggest_pattern("write", "src/agent/tools/foo.rs"),
            "src/agent/tools/**"
        );
        assert_eq!(suggest_pattern("read", "src/main.rs"), "src/**");
        assert_eq!(suggest_pattern("list_dir", "src/agent"), "src/**");
        // Files at the repo root: `Path::parent` is "" — keep the
        // existing `**` fallback so the rule is broad but explicit.
        assert_eq!(suggest_pattern("write", "main.rs"), "**");
    }

    /// User-reported bug: `[a] allow always` on an MCP tool call
    /// silently degraded to `allow once` because the catch-all
    /// `_ => PLACEHOLDER` branch fired for `mcp_tool`. Result: the
    /// permission allowlist never got an entry and every
    /// subsequent call to the same MCP server re-prompted the
    /// user.
    #[test]
    fn suggest_pattern_derives_server_wildcard_for_mcp_tool() {
        let p = suggest_pattern("mcp_tool", "mcp_tool:lattice:lattice_expand");
        assert_eq!(p, "mcp_tool:lattice:*");
        // Multi-segment server names also work.
        let p = suggest_pattern("mcp_tool", "mcp_tool:my-server:do_thing");
        assert_eq!(p, "mcp_tool:my-server:*");
    }

    /// Malformed MCP input (missing colons, wrong umbrella) still
    /// falls through to the placeholder rather than producing a
    /// nonsense pattern.
    #[test]
    fn suggest_pattern_mcp_tool_malformed_input_uses_placeholder() {
        assert!(is_placeholder_pattern(&suggest_pattern(
            "mcp_tool", "garbage"
        )));
        assert!(is_placeholder_pattern(&suggest_pattern(
            "mcp_tool",
            "mcp_tool:"
        )));
        assert!(is_placeholder_pattern(&suggest_pattern(
            "mcp_tool",
            "mcp_tool::"
        )));
        assert!(is_placeholder_pattern(&suggest_pattern(
            "mcp_tool",
            "wrong:lattice:foo"
        )));
    }
}
