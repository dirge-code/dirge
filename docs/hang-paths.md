# How dirge can stop responding

An audit of the ways a run can stop making progress with nothing said about it.
Prompted by a report of the TUI hanging mid-session, possibly around a
`websearch`. None of what follows is confirmed as *that* hang — see
[What would identify it](#what-would-identify-it) — but each is reachable from
the code as it stands, and the first two are structural.

## The fact that shapes all of this

```rust
#[tokio::main(flavor = "current_thread")]   // src/main.rs:435
```

dirge runs on a **single-threaded runtime**. The TUI event loop, the agent
task, every tool, and the terminal input reader are all on one thread, taking
turns. Two consequences run through everything below:

- Any work that does not `.await` — a synchronous file read, a SQLite query, a
  regex over a large body, a `join()` on an OS thread — stops *everything*,
  including keystroke handling. Ctrl+C cannot be seen because the thread that
  would notice it is the one that is busy.
- Any task panic happens **on the UI thread**, which the panic hook treats as
  fatal even though tokio catches it and the process keeps going.

## 1. A panic in the agent task hangs the UI for good

Three separate pieces, none wrong alone:

- `panic = "abort"` is not set, so a panic unwinds. `tokio` catches a task
  panic and stores it in the `JoinHandle`; the process survives.
- `ui.agent_abort: Option<JoinHandle<()>>` is only ever `.abort()`ed — never
  awaited, never polled. Nothing reads that handle's result, so the panic is
  never observed.
- The UI's event arm is `Some(event) = async { rx.recv().await }`. When the
  agent's channel closes, `recv()` returns `None`, the pattern fails, and
  `tokio::select!` **disables the branch**. No `else`, no handler.

`ui.is_running` is cleared only by `Done`, `Error`, `Interjected`,
`PlanReview`, `ContextOverflow`, and `/quit` — none of which can arrive from a
dead task. So the UI sits at "running" indefinitely.

Tools are awaited inline in the agent task
(`execute_prepared_tool_call` → `tool.execute(...).await`), so **any panic in
any tool takes the whole run down this path**: an `unwrap`, a slice index that
isn't a char boundary, an arithmetic overflow in a debug build.

### …and it resets the terminal on the way out

`install_panic_hook` (`src/ui/terminal.rs:134`) skips the terminal reset when
the panicking thread is not the one that built the `TerminalGuard`:

```rust
if !thread_owns_terminal(UI_THREAD_ID.get().copied(), std::thread::current().id()) {
    previous(info);
    return;
}
```

The comment explains it as protecting against panics on "worker/blocking
threads … the TUI keeps running, so leave the terminal alone". That reasoning
is right and the test is wrong: under a current-thread runtime the agent task
IS the UI thread, so a caught, survivable task panic takes the fatal branch. It
writes `PANIC_RESET_SEQ`, calls `disable_raw_mode()`, and sets
`PANIC_HOOK_FIRED` — on a process that then keeps running.

The resulting symptom is exactly "it hung": the screen resets, raw mode is off
so keys behave strangely, the run never finishes, and because
`PANIC_HOOK_FIRED` is latched, `TerminalGuard::drop` skips its own reset when
the user finally kills it.

The guard is checking thread identity when the question is whether the process
is about to die. Those were the same thing under a multi-threaded runtime.

## 2. Blocking work stops the whole program

On one thread there is no other worker to keep the UI alive. Anything
synchronous on the hot path freezes input handling for its full duration:

- `refresh_openai_credential` / `refresh_anthropic_token_sync` do
  `std::thread::spawn(...).join()` — correct in that they avoid a nested
  runtime, but `join()` blocks the only thread. Bounded, at least: both inner
  clients set a request timeout (30s for the OpenAI device flow,
  `TOKEN_REQUEST_TIMEOUT` for Anthropic). A freeze, not a hang.
- The tools make ~280 direct `std::fs::` calls. A read off a stalled network
  mount blocks with no timeout and no way to interrupt.
- Response post-processing — `guard_untrusted_result`'s injection scan over a
  web body, for instance — is synchronous CPU work on the same thread.

The dividing line is whether a path uses `spawn_blocking` (72 call sites do).
Everything else is on the critical thread.

## 3. Nothing bounds a built-in tool call

`src/timeout.rs` is the source of truth and covers `stream_chunk`,
`request_establish`, `tool_call_gap`, `mcp_call`, `mcp_init`, `lsp_request`,
`lsp_initialize`, `bash`, `bash_max`. There is **no timeout for a built-in
tool**: `websearch`, `webfetch`, `read`, `grep`, `task`, and the rest are
bounded only by whatever they set for themselves.

Some do — `websearch` and `webfetch` both build a client with a 15s timeout.
The point is that it is per-tool convention rather than a property of dispatch,
so a new tool, or an existing one on a path that skips its own client, has
nothing underneath it.

Dispatch does race the abort signal (`tokio::select!` on `wait_for_cancel`),
so a stuck *async* tool is at least interruptible — provided the thread is free
enough to notice the signal, which §2 is about.

## 4. A permission ask nobody answers

`handle_ask_inner` sends an `AskRequest` and then `reply_rx.await`s with no
deadline. It handles the channel being *dropped* (treated as a deny) but not
the request being held forever.

The TUI's arm that services those requests is gated:

```rust
Some(ask_req) = async { ... }, if !ui.input_mode.is_modal() => {
```

so while any modal is up — question, dialog, plan approval, another permission
prompt — permission requests queue and the agent blocks. That is deliberate
(one modal at a time) and normally ends when the user answers, but any modal
that is entered and never left becomes a silent hang of the run.

This shape has bitten before: issue #523 is the same wait in headless mode,
fixed by adding an auto-deny drain, and ACP has its own drain for the same
reason. The interactive path has no equivalent backstop because it assumes the
UI always gets there eventually.

## What would identify it

The audit cannot say which of these fired. The log can — dirge writes one when
`--verbose` or `DIRGE_LOG` is set, and the default hook's panic backtrace goes
there rather than to the screen (that is what `log_path_hint()` is for). Three
things separate the cases:

| in the log | reading |
|---|---|
| a panic backtrace, then nothing | §1 — the agent task died |
| last line is a tool starting, no result | §2 or §3 — stuck inside a tool |
| a permission ask with no decision after it | §4 |
| nothing after a normal turn boundary | the provider stream, which has its own timeouts |

Whether the terminal looked reset (colors gone, prompt misplaced, keys
echoing) also separates §1 from the rest on sight.
