# Tool-name misses

What happens when a model names a tool dirge does not have, how often that
happens, and what the measurements said about each fix. Background for
`agent_loop/tool_aliases.rs`, `agent_loop/suggest.rs`, and the three
`*_tool_names` counters on the `dirge::gates` line.

## The three outcomes

A name reaches dispatch by one of two routes, and they fail differently.

| route | before | after |
|---|---|---|
| native `tool_calls` with an unknown name | `Tool X not found`, plus a nearest-name hint; costs a turn | resolved if the name is placeable, else unchanged |
| a call written as TEXT, lifted by the scavenger | dropped on the allowed-name gate — no result, no error, no counter | resolved if placeable, else counted as `dropped_unknown_names` |

The second is the quiet one. The scavenger's gate rejected the name, the call
never existed, and the turn then ended with no tool call at all — so the loop
read the model's raw call syntax as its final answer. Verified end to end
against the real binary with a mock provider:

```
mode     guess        aliased  halluc  dropped  calls  answer
native   shell        1        0       0        1      DONE
native   frobnicate   0        1       0        1      DONE
text     shell        1        0       0        1      (call ran)
text     frobnicate   0        0       1        0      </tool_call>
```

The last row is the failure with nothing to see: zero calls, and what the user
gets back is the call syntax. Before this work the `text / shell` row behaved
the same way.

(The raw syntax appearing in the answer at all is a separate, pre-existing
defect — the text is streamed before the scavenger ever runs, so it cannot be
fixed here. Filed as `dirge-n00z`.)

## How often does a name miss?

**With a `tools` array in the request: never observed.** 60 tasks worded to
tempt synonyms ("Execute git status", "Open package.json", "Delete the temp/
directory"), across three hosted models and the documented floor model
(qwen3.6-27b, llama.cpp): **90 native tool calls, 0 off-registry names.** The
providers tested constrain the name to the advertised set, so the model has
nothing to guess with.

That is the whole reason the alias table is about the *text* route. A name is
only in play when the model writes the call from memory instead of from the
schema.

**With no `tools` array — the model's own vocabulary:** 60 tasks × 3 models,
asked for the tool call it would make with nothing to copy from.

| | |
|---|---|
| named calls | 175 |
| not a dirge tool | **111 (63%)** |

By family, off-registry names and their counts:

| meant | names produced | n |
|---|---|---|
| `bash` | shell 27, execute_command 16, terminal 7, Bash 6, exec 1, shell_command 1, Terminal 1 | 59 |
| `question` | ask_user 8, ask_followup_question 1, ask 1 | 10 |
| `grep` | search_content 3, grep_search 1, search_file 1, rg 1 | 6 |
| `read` | read_file 4, open_file 1 | 5 |
| `list_dir` | list_files 4 | 4 |
| `write` | write_file 4 | 4 |
| `memory` | create_note 2, save_memory 1, search_memory 1 | 4 |
| `write_todo_list` | todo_write 2, TODO_WRITE 1 | 3 |
| `task` | Task 1, subagent 1, delegate 1 | 3 |
| `webfetch` / `websearch` | web_search 2, web_fetch 1, WebFetch 1, fetch 1 | 5 |
| `issue` | create_issue 2 | 2 |
| `edit` | str_replace_editor 2 | 2 |
| `bash_output` | get_job_output 1, get_build_output 1 | 2 |

None of these are typos. They are other words for the same thing, and edit
distance cannot reach any of them.

Roughly a fifth of the off-registry names differ from a real one only in case
or separators (`Bash`, `WebFetch`, `web_search`, `TODO_WRITE`). Those are the
same name written differently, so they resolve by normalizing against the real
registry rather than by table entry.

## The suggester was pointing at the wrong tools

`suggest::closest` allowed `len/2` edits, capped at 3 — half a short name could
differ and still read as a typo. Run against dirge's own tool names, of the
eleven plausible guesses it resolved, **six pointed at a tool with nothing to
do with the one asked for**:

```
exec   -> spec      shell -> skill     open -> spec
ls     -> lsp       ask   -> task      search -> websearch
```

The message carries no hedge — "Did you mean `spec`?" — so the harness steers a
model that wanted a shell toward a spec-management tool, and then
`hallucinated_tool_names` scores the resulting flailing at 2× as the model
being out of its depth. `capability_cards.rs` names this shape exactly: the
harness manufacturing the signal it reads.

Rules measured against 30 cases (real typos, real synonyms, and names that
should resolve to nothing):

| rule | right | correctly silent | wrong suggestion | missed |
|---|---|---|---|---|
| `len/2` capped at 3, Levenshtein (shipped) | 12 | 8 | **8** | 2 |
| same, with transposition = 1 edit | 12 | 8 | 8 | 2 |
| ≤ 30% of the longer name | 11 | 13 | 3 | 3 |
| `len/3` capped at 3 | 11 | 12 | 4 | 3 |
| **≤ 25% of the longer name** | 11 | 15 | **1** | 3 |

Eight wrong suggestions to one. What it gives up is `ripgrep → grep`, which is
a synonym rather than a typo and belongs in the alias table; and `ask → task`,
the one survivor, is resolved before the suggester ever runs because `ask` is
in the table.

Counting an adjacent transposition as one edit rather than two is what makes
the tighter budget affordable: `raed`/`read` and `wrtie`/`write` are the most
common way a name gets mistyped, and at distance 2 they need a budget wide
enough to also admit `exec`/`spec`.

## What is deliberately not in the table

`search`, `find`, `open`, `plan`. Each is as plausibly one dirge tool as
another — `search` could be `grep` or `websearch`, `open` could be `read` or
`plan_enter`. A confident wrong dispatch is worse than the error it replaces,
because the error at least says what happened.

## Reproducing

The probes are three short scripts against the provider APIs directly, using
the tool schemas captured from a real dirge request (point dirge at a mock
OpenAI-compatible server and dump the request body — see the wire-capture
notes). They are not checked in: they cost real API calls, and the numbers
above are the record.

The mechanism counters are on the `dirge::gates` line and in
`scripts/loop-ab.sh` as three mechanism rows — `hallucinated_tool_names`,
`dropped_unknown_names`, `aliased_tool_names`. All three zero in both arms
means an A/B on anything touching tool names measured nothing.
