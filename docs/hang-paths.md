# How dirge can stop responding

An audit of the ways a run can stop making progress with nothing said about it.
Prompted by a report of the TUI hanging mid-session, possibly around a
`websearch`. None of what follows is confirmed as *that* hang — see
[What would identify it](#what-would-identify-it) — but each is reachable from
the code as it stands, and the first two are structural.

**§1, §1b and §3 are fixed** (`dirge-r5l1`, `dirge-u9xv`, `dirge-9tl3` +
`dirge-8cbm`); §2 and §4 are not, though §2 turned out to have one specific
instance worth naming (`dirge-bz0a`, below). The description of each is kept
as it was, followed by what changed.

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

### Fixed

The UI's run state hung off the *events a run chose to emit*, so a run that
emitted none left nothing that could change its mind. That is now two
guarantees, one at each end:

`agent_loop/run_end.rs` makes the terminal event a property of the run's
**lifetime**. `RunEpitaph` holds a sender for as long as the spawned task
exists and sends `AgentEvent::Error` on its way down if nothing terminal went
out; unwinding runs drop glue, so it fires on the panic path, which is the one
that needed it. The panic text comes from the record the hook left behind
(below), so the error names the panic and its source location rather than
sending the reader to the log. Every consumer benefits — `--print` and ACP
drain the same channel.

`ui/run_handlers/ended.rs` is the backstop underneath it, for endings the task
cannot narrate from inside itself: a `try_send` against a full channel, an
abort from something that isn't the UI. The loop grew a `select!` arm that
awaits the run's `JoinHandle`, placed after the event arm so `biased` delivers
every buffered event — including the epitaph's — first, and guarded on
`is_running` so it stays off the paths that deliberately keep a run alive past
`Done`. A handled terminal event takes `agent_abort`, which disables the arm;
so exactly one of the two reports, never both.

Measured against the binary with a panic injected into tool dispatch: the TUI
prints `error: the agent run crashed: <panic> (at <file:line>)`, closes the
tool chamber and returns the prompt; `--print` exits 1 with the same message.

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

#### Fixed

The hook stopped predicting. It now only writes the panic down
(`panic_report`) and chains to the previous hook, whose backtrace still lands
in the log. Two parties read the record afterwards, and each knows something
the hook could not:

- whoever caught the panic and carried on — the run's epitaph, which reports it
  in band as the run's error, and *claims* the record by taking it;
- `TerminalGuard::drop`, which runs exactly when the process really is tearing
  down. It resets the terminal as it always did and then prints an unclaimed
  record on the restored screen.

So a caught panic no longer touches a live terminal, and a fatal one still gets
its notice — and the decision is made where the answer is known rather than
guessed where it isn't. `PANIC_HOOK_FIRED`, `UI_THREAD_ID` and
`thread_owns_terminal` are gone with the prediction; the skip-branch they fed in
`Drop` went too, so the teardown has one path again.

A side effect worth naming: a panic that some `catch_unwind` swallowed and
nobody reported now prints one line at exit instead of being visible only in a
log the user probably wasn't capturing.

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

### One instance is not diffuse at all — `dirge-bz0a`

Treating this as "~280 `std::fs::` calls to sweep" hid the case that actually
bites. Three transports — `anthropic_http.rs`, `kimi_http.rs`,
`codex_http.rs` — resolve their bearer in a sync `RefreshableToken::bearer()`
called from `normalized_request`, i.e. **on the per-request path**:

```rust
fn bearer(&self) -> String {
    let mut state = self.state.lock()…;   // std::sync::Mutex, held throughout
    if expired {
        match (self.refresher)() { … }    // → std::thread::spawn(…).join()
    }
}
```

The refresher spawns an OS thread, builds a whole tokio runtime, and
`block_on`s an HTTP exchange — while the only runtime thread waits on
`join()`. Nothing paints, no keystroke is read, and **no timer can fire**,
including the §3 watchdog meant to bound whatever is stuck. Bounded per
attempt (30s) but Kimi retries three times with backoff, and Kimi access
tokens live 15 minutes, so it recurs through a long session. All three carry
the same comment — *"Refresh is rare (once per token lifetime) so doing it
synchronously here is acceptable"* — which predates the single-threaded
runtime.

The fix is to hoist the bearer resolution into the async `send` and run the
refresh through `spawn_blocking`; the `Mutex` must not be held across it
either.

For contrast, the other candidate checked and cleared: `guard_untrusted_result`
runs ~25 regexes over an untrusted body with no size cap, on this thread. The
`regex` crate is linear-time and websearch truncates each result to 500 chars,
so it is sub-second on anything realistic. Not a hang.

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

### Fixed

The same race now bounds the call in time. `timeouts.tool_call` (600s) is a
**ceiling on one dispatch**, not a per-tool default: every tool keeps its own
tighter bound, and this exists so a tool that forgets — or a path that skips
its own client — cannot stall a run silently. A tool whose own bound
legitimately exceeds it declares that through `LoopTool::call_budget`; `bash`
derives it from `resolve_foreground_timeout`, the subagent from its clamp
ceiling, each plus a grace so the tool's own timeout fires first and produces
the better message. Wiring is a builder on `RigToolAdapter` set where the
concrete tool is constructed, so a new tool cannot be missed by a name table.
On expiry the tool future is dropped, which runs the same RAII guards
cancellation relies on — bash's `PgKillGuard` and the rest.

Two rules keep it from doing harm, both of which cost a round of rework to
find (`dirge-8cbm`):

**An override may only raise the ceiling, never lower it.** An override says
"this tool needs longer"; a tool that wants to be cut sooner should bound
itself, where it can say why.

**The budget bounds work, not a person.** The permission prompt, the
`question` tool and `/plan` approval all wait for the user from *inside*
`LoopTool::execute` — inside the window being bounded. As first written the
watchdog would have cut a `bash` call after 150s because the user was still
reading the command they were asked to approve, which is worse than the stall
it exists to catch. `src/human_wait.rs` marks those stretches and the watchdog
re-arms rather than firing while any are open. The count is process-wide, so
one prompt holds off every in-flight watchdog — erring toward not cutting,
which is the direction to err in.

The known limitation is the one §2 describes: this bounds *async* stalls only.
A tool that blocks the runtime thread never lets the timer be polled.

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

On a build carrying the §1 fix the first row cannot hang any more — it surfaces
as `error: the agent run crashed: …` and the prompt comes back — so a hang on a
current build is §2, §3 or §4, and the same table still tells them apart.
