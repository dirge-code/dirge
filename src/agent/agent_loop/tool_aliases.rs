//! Names models reach for that dirge does not have (dirge-e31n.8).
//!
//! # What this fixes
//!
//! When the provider hands the model a `tools` array, the name comes back
//! right: across four models and 90 native calls — three hosted, plus the
//! documented floor model — not one named a tool that did not exist. The name
//! is only in play when the model writes the call from memory instead of from
//! the schema: a call emitted as TEXT, which the scavenger lifts back out.
//!
//! Asked for the same 60 tasks with NO tools array — the model's own
//! vocabulary, with nothing to copy — **111 of 175 names it produced were not
//! dirge tools** (see `docs/tool-name-misses.md`). They are not typos. They
//! are other words for the same thing: `shell` and `execute_command` for
//! `bash`, `ask_user` for `question`, `read_file` for `read`. Edit distance
//! cannot reach any of them, and should not try — `shell` is not a mistyped
//! `bash`.
//!
//! Two things used to happen to such a name, both bad and neither visible:
//! dispatched, it came back "Tool shell not found" and cost a turn; written as
//! text, the scavenger dropped it on the allowed-name gate and the turn ended
//! with no call at all, so the raw call syntax became the final answer.
//!
//! Entries carry the count that put them here. The table starts from that
//! measurement and is meant to grow from production: both miss paths now log
//! the guessed name on `dirge::tool_miss`, so the next entry is an
//! observation rather than a guess about what a model might say.
//!
//! # The rules this table follows
//!
//! - **An alias never shadows a real tool.** Enforced by test, both ways: no
//!   alias may normalize onto a built-in's name, and every target must be a
//!   name in [`BUILTIN_TOOL_NAMES`] — so renaming a tool breaks the build here
//!   rather than leaving a table that quietly points at nothing.
//! - **Only unambiguous synonyms.** `search` is not here: it is as plausibly
//!   `grep` as `websearch`, and a confident wrong answer is worse than the
//!   error, which at least says what happened. Same for `find` (`find_files`
//!   or `find_definition`?) and `open` (`read` or `plan_enter`?).
//! - **Case and separators are not aliases**, they are the same name written
//!   differently, so they resolve against the real registry rather than
//!   needing an entry each. `Bash`, `BASH` and `write_todolist` all land
//!   without the table being consulted.
//!
//! Resolution happens BEFORE the nearest-name suggester, which is left to do
//! the one job it is good at — typos.

use crate::agent::tools::BUILTIN_TOOL_NAMES;

/// A name models use for a tool dirge calls something else.
///
/// Keys are stored NORMALIZED (see [`normalize`]), so one entry covers
/// `execute_command`, `executeCommand` and `execute-command`.
///
/// Counts in the comments are from the vocabulary probe in
/// `docs/tool-name-misses.md`: 60 tasks × 3 models, no tools array. Entries
/// without a count are the same word in a shape the probe happened not to
/// emit, and are here because splitting a synonym family on which member a
/// sample caught would be fitting the sample rather than the phenomenon.
const ALIASES: &[(&str, &str)] = &[
    // Running a command. The largest family by a distance: 52 of the 111
    // off-registry names were one of these.
    ("shell", "bash"),          // 27
    ("executecommand", "bash"), // 16
    ("terminal", "bash"),       // 7
    ("exec", "bash"),           // 1
    ("shellcommand", "bash"),   // 1
    ("runcommand", "bash"),     // 1
    // Asking the user. dirge calls it `question`; models build the name on
    // "ask" without exception.
    ("askuser", "question"),             // 8
    ("askfollowupquestion", "question"), // 1
    ("ask", "question"),                 // 1
    // Reading a file. Bare `open` is NOT here — it is as plausibly
    // `plan_enter` as `read` — but `open_file` names the object.
    ("readfile", "read"), // 4
    ("openfile", "read"), // 1
    // Writing one.
    ("writefile", "write"), // 4
    // Searching file CONTENT, as distinct from finding files by name. Bare
    // `search` and bare `find` are deliberately absent for that reason.
    ("searchcontent", "grep"), // 3
    ("grepsearch", "grep"),    // 1
    ("searchfile", "grep"),    // 1
    ("rg", "grep"),            // 1
    // Listing a directory.
    ("listfiles", "list_dir"), // 4
    // Remembering across sessions.
    ("createnote", "memory"),   // 2
    ("savememory", "memory"),   // 1
    ("searchmemory", "memory"), // 1
    ("updatememory", "memory"), // 1
    // The todo list. `todo_write` does NOT normalize onto `write_todo_list`,
    // so it needs an entry despite looking like a word-order variant.
    ("todowrite", "write_todo_list"), // 3
    // Issues.
    ("createissue", "issue"), // 2
    // Editing in place.
    ("strreplaceeditor", "edit"), // 2
    ("strreplace", "edit"),
    // Handing work to a sub-agent. Plain `Task` needs no entry — it
    // normalizes onto `task`.
    ("subagent", "task"), // 1
    ("delegate", "task"), // 1
    // Output of a job left running. `web_fetch` / `WebFetch` likewise need no
    // entry; they normalize onto `webfetch`.
    ("getjoboutput", "bash_output"),   // 1
    ("getbuildoutput", "bash_output"), // 1
    ("fetch", "webfetch"),             // 1
];

/// Normalize a name for comparison: lowercase, and drop the separators that
/// distinguish `execute_command` from `executeCommand` from `execute-command`.
///
/// This is what makes case and snake/camel differences resolve without an
/// entry each — they are the same name, not different ones.
fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The real tool `guessed` most likely meant, or `None`.
///
/// Consulted only after an exact match has already failed. Two steps, in
/// order: the same name written differently, then a synonym. Both are checked
/// against `candidates` — the tools this run actually has — so an alias for a
/// tool that is absent (a cargo feature off, a profile's `allow_tools`)
/// resolves to nothing rather than to a name that would fail one layer later.
pub fn resolve<'a, I, S>(guessed: &str, candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a S> + Clone,
    S: AsRef<str> + 'a + ?Sized,
{
    let want = normalize(guessed);
    if want.is_empty() {
        return None;
    }
    let find = |target: &str| {
        // Exactly one, or none. Two tools can normalize onto the same string —
        // an MCP server exporting `Read` beside the built-in `read` passes the
        // collision gate, which compares names exactly. Picking one would
        // depend on iteration order of a `HashSet`, i.e. it would differ
        // between runs. Same rule `closest` uses for a distance tie: an
        // ambiguous field resolves to nothing.
        let mut hits = candidates
            .clone()
            .into_iter()
            .map(AsRef::as_ref)
            .filter(|c| normalize(c) == target);
        let only = hits.next()?;
        hits.next().is_none().then_some(only)
    };
    find(&want).or_else(|| {
        let target = ALIASES.iter().find(|(k, _)| *k == want)?.1;
        find(&normalize(target))
    })
}

/// Rewrite every call whose name is not a tool but resolves to one, and report
/// what was rewritten as `(guessed, real)` pairs.
///
/// One pass over the whole batch, so a native call and a scavenged one
/// carrying the same guessed name are treated identically. Runs before the
/// storm breaker sees the batch: `shell` and `bash` with the same arguments
/// are one call, and only a rewrite that happens first can let storm say so.
pub fn resolve_call_names(
    calls: &mut [crate::agent::agent_loop::tools::ToolCall],
    allowed: &std::collections::HashSet<String>,
) -> Vec<(String, String)> {
    let mut resolved = Vec::new();
    for call in calls.iter_mut() {
        if allowed.contains(&call.name) {
            continue;
        }
        let Some(real) = resolve(&call.name, allowed.iter()).map(str::to_string) else {
            continue;
        };
        resolved.push((std::mem::replace(&mut call.name, real.clone()), real));
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard that keeps the table from rotting: every target must still be
    /// a tool. Rename or drop a built-in and this fails, rather than leaving
    /// an alias that resolves to nothing at the one moment it is needed.
    #[test]
    fn every_alias_points_at_a_real_tool() {
        for (from, to) in ALIASES {
            assert!(
                BUILTIN_TOOL_NAMES.contains(to),
                "alias {from} -> {to}, which is not a built-in tool",
            );
        }
    }

    /// The other direction: an alias must never normalize onto a real tool's
    /// name. Such an entry is unreachable at best — the registry match runs
    /// first — and at worst it is a rename waiting to shadow the real tool.
    #[test]
    fn no_alias_shadows_a_real_tool() {
        for (from, _) in ALIASES {
            assert!(
                !BUILTIN_TOOL_NAMES.iter().any(|b| normalize(b) == *from),
                "alias key {from} is a real tool name",
            );
        }
    }

    /// Keys must already be normalized, or they can never match: `resolve`
    /// normalizes the guess and compares against the key verbatim.
    #[test]
    fn alias_keys_are_stored_normalized() {
        for (from, _) in ALIASES {
            assert_eq!(&normalize(from), from, "alias key is not normalized");
        }
    }

    #[test]
    fn no_duplicate_alias_keys() {
        let mut seen = std::collections::HashSet::new();
        for (from, _) in ALIASES {
            assert!(seen.insert(*from), "duplicate alias key {from}");
        }
    }

    /// The negative half, and the one that matters: a name we cannot place
    /// must stay unplaced. Guessing here would dispatch a tool the model did
    /// not ask for, which is strictly worse than the error it replaces —
    /// the error at least says what happened.
    #[test]
    fn an_unplaceable_name_resolves_to_nothing() {
        let tools = dirge_tools();
        for guess in [
            // Genuinely ambiguous, and deliberately absent from the table.
            "search",
            "find",
            "open",
            "view",
            "run_tests", // Not a tool name at all.
            "frobnicate",
            "",
            "___",
        ] {
            assert_eq!(resolve(guess, &tools), None, "{guess} should not resolve");
        }
    }

    /// The same name written differently is not an alias and needs no entry:
    /// case and separators fall out of normalizing against the real registry.
    #[test]
    fn case_and_separator_variants_resolve_without_a_table_entry() {
        let tools = dirge_tools();
        for (guess, want) in [
            ("Bash", "bash"),
            ("BASH", "bash"),
            ("Grep", "grep"),
            ("writetodolist", "write_todo_list"),
            ("write-todo-list", "write_todo_list"),
            ("readMinified", "read_minified"),
            ("web_search", "websearch"),
            ("web_fetch", "webfetch"),
        ] {
            assert_eq!(resolve(guess, &tools), Some(want), "{guess}");
        }
    }

    /// The positive half: the synonym families the vocabulary probe measured.
    #[test]
    fn measured_synonyms_resolve() {
        let tools = dirge_tools();
        for (guess, want) in [
            ("shell", "bash"),
            ("execute_command", "bash"),
            ("exec", "bash"),
            ("terminal", "bash"),
            ("ask_user", "question"),
            ("fetch", "webfetch"),
            ("update_memory", "memory"),
            ("search_content", "grep"),
        ] {
            assert_eq!(resolve(guess, &tools), Some(want), "{guess}");
        }
    }

    /// Two tools normalizing onto the same string resolve to NEITHER. The
    /// alternative is a pick that depends on `HashSet` iteration order, so the
    /// same session would dispatch differently on different runs.
    #[test]
    fn an_ambiguous_normalization_resolves_to_nothing() {
        // An MCP server may export `Read` beside the built-in `read`: the
        // collision gate compares names exactly, so both are registered.
        let both = vec!["read", "Read", "bash"];
        assert_eq!(resolve("READ", &both), None);
        // ...and one alone still resolves, so the case above cannot pass for
        // the wrong reason.
        assert_eq!(resolve("READ", &vec!["read", "bash"]), Some("read"));
    }

    /// An alias for a tool this RUN does not have resolves to nothing. A
    /// build without a feature, or a profile that caps `allow_tools`, must not
    /// get a name that fails one layer later with a worse message.
    #[test]
    fn an_alias_for_an_absent_tool_resolves_to_nothing() {
        let without_bash: Vec<&str> = dirge_tools().into_iter().filter(|t| *t != "bash").collect();
        assert_eq!(resolve("shell", &without_bash), None);
        assert_eq!(resolve("bash", &without_bash), None);
        // ...and still resolves when it is present, so the test above cannot
        // pass for the wrong reason.
        assert_eq!(resolve("shell", &dirge_tools()), Some("bash"));
    }

    fn dirge_tools() -> Vec<&'static str> {
        BUILTIN_TOOL_NAMES.to_vec()
    }

    fn allowed(names: &[&str]) -> std::collections::HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn call(name: &str) -> crate::agent::agent_loop::tools::ToolCall {
        crate::agent::agent_loop::tools::ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: serde_json::json!({}),
        }
    }

    /// The batch pass rewrites in place and reports what it changed, so the
    /// caller can count and log it — and a name it cannot place is left
    /// exactly as the model wrote it, to fail with its own name in the error.
    #[test]
    fn the_batch_pass_rewrites_only_what_it_can_place() {
        let mut calls = vec![
            call("shell"),
            call("read"),
            call("frobnicate"),
            call("Grep"),
        ];
        let changed = resolve_call_names(&mut calls, &allowed(&["bash", "read", "grep"]));
        assert_eq!(
            calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["bash", "read", "frobnicate", "grep"]
        );
        assert_eq!(
            changed,
            vec![
                ("shell".to_string(), "bash".to_string()),
                ("Grep".to_string(), "grep".to_string()),
            ]
        );
    }

    /// A batch with nothing to fix must report nothing. Without this the
    /// counter could read high on every healthy run and still look like
    /// evidence the table was earning its keep.
    #[test]
    fn a_clean_batch_reports_no_aliases() {
        let mut calls = vec![call("bash"), call("read")];
        assert!(resolve_call_names(&mut calls, &allowed(&["bash", "read"])).is_empty());
    }
}
