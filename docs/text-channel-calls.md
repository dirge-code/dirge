# Text-channel calls

Some models write their tool calls into the text channel instead of the
structured one. `<tool_call>…</tool_call>` is Qwen/Hermes' native channel and
leaks whenever llama.cpp is served without `--jinja`; `<|DSML|invoke …>` is
R1's; a ```` ```json ```` fence is what a model that learned tool use from a
chat log produces. `agent_loop/scavenge.rs` exists to run those calls anyway.

Running them was only half the job. Until `dirge-n00z` the call ran and the
syntax was still treated as prose, which broke three things at once.

## What went wrong

**The user was shown the call as the answer.** Reproduced end to end against
the binary with a mock provider — one text-mode call for `shell`, which the
alias table places on `bash`:

```
$ dirge -p "say hi"
<tool_call>
{"name": "shell", "arguments": {"command": "echo ALIASWORKS"}}
</tool_call>DONE
```

The command ran. The user got its source code.

**The next request was malformed.** The scavenged call never reached the
assistant message, so the turn-2 body carried a tool result answering nothing:

```json
{"role": "assistant", "content": [{"type": "text", "text": "<tool_call>…</tool_call>DONE"}]}
{"role": "tool", "tool_call_id": "", "content": "ALIASWORKS\n"}
```

A `role: "tool"` message with no preceding `tool_calls` is a hard 400 on both
OpenAI and Anthropic. It stayed latent because text-channel calls come from
servers lenient enough to have leaked them in the first place — the failure
was waiting for the first strict provider to see one.

**Results were crossed.** Scavenged calls carried `id: ""`, so two in one turn
were indistinguishable. `find(|c| c.id == result.tool_call_id)` matched
whichever came first, and with it the storm signature, the failure tracker and
the side-effect classification; the publish guard's `blocked_ids` filter would
have dropped both when one was blocked. This one fires on any provider.

## What it does now

One rule, in `agent_loop/call_syntax.rs`: **a region of text that the loop
dispatches is a call, not prose** — so it does not reach the user, and the
transcript records it as a call.

| | |
|---|---|
| `call_syntax::scan` | where a call region starts and ends |
| `call_syntax::is_call` | whether that region will actually run |
| `call_syntax::DisplayFilter` | withholds it from the text the user sees |
| `call_syntax::absorb_text_calls` | rewrites the assistant message to make the call |
| `scavenge::scavenge_tool_calls` | reads the same regions to decide what to run |

The scavenger and the display filter read the same region finder, which is the
point. Before, only dispatch had an opinion about where a call started, so
nothing else could agree with it.

The same run, after:

```
$ dirge -p "say hi"
DONEALL DONE
```

```json
{"role": "assistant",
 "content": [{"type": "text", "text": "DONE"}],
 "tool_calls": [{"id": "scav-1", "type": "function",
                 "function": {"name": "bash", "arguments": "{\"command\":\"echo ALIASWORKS\"}"}}]}
{"role": "tool", "tool_call_id": "scav-1", "content": "ALIASWORKS\n"}
```

Note the recorded name is `bash`, not the `shell` the model wrote: the rewrite
happens after alias resolution, so the message says what ran.

## Why syntax that runs nothing is left alone

A call naming a tool that does not exist is dropped silently and on purpose
(`dirge-knt8` — erroring on scavenged text forces a continuation turn, which
was the duplicate-response bug). Nothing dispatches, so the turn ends there. If
the display hid the syntax as well, the user would get an empty answer with no
account of what happened. So it stays on screen, and `dropped_unknown_names`
counts it.

That is also what keeps a ```` ```json ```` fence the model is *showing*
someone from vanishing: the fence is only withheld when the JSON in it names a
tool this run has. A fence carrying a real call is withheld — the scavenger
runs it, and display agreeing with dispatch is the property worth having.

The full matrix, measured against the binary:

| written as | names | shown to the user | recorded as a call |
|---|---|---|---|
| text | `shell` (aliases to `bash`) | nothing | yes, `scav-1` → `bash` |
| text | `frobnicate` | the raw syntax | no |
| native | `shell` | nothing | yes (native id) |
| native | `frobnicate` | `Tool frobnicate not found` | no |

## The streaming problem

Text is streamed to the user as it arrives and the scavenger does not run until
the message is complete, so nothing can be un-printed after the fact. The
filter therefore has to answer a question the scavenger never does: *could this
still become a call?* A trailing `` ``` `` is either the start of a fenced call
or three backticks, and which one has not been written yet.

`scan` reports three outcomes — decided, undecidable-yet, and clear — and the
filter withholds only on the middle one. Once a region opens, the text after it
is withheld too, until the region resolves: releasing the tail separately can
only put the two on screen out of order.

Two callers read the filter, one watching the message arrive and one holding it
finished (`-p` treats `Done.response` as authoritative and overwrites what it
echoed). They are separate code paths, so
`watching_a_message_arrive_and_reading_it_whole_agree` pins them together.

## Reproducing

A mock OpenAI-compatible server returning one text-mode call over SSE, with
`providers.<alias>.allow_insecure = true` so `http://` is accepted. The mock
answers the first request with the leaked call and every later one with a plain
answer, keyed on whether the history already contains an assistant or tool
message — a request counter breaks the moment a run is repeated against the
same process. `--output-format stream-json` shows the turn structure; dumping
the request bodies shows the transcript the provider actually receives, which
is where the second and third defects are visible at all.
