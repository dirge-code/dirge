# Agent Loop

The agent loop drives a multi-turn conversation between an LLM and a set of tools. It streams assistant responses, dispatches the tool calls they request, injects steering / follow-up messages from the host application, and gives plugins and the embedder structured hook points to observe and shape every turn.

The loop solves the problem of running an LLM agent as a long-lived, cancellable, hook-extensible process — instead of one prompt-and-reply, the loop owns turn boundaries, tool dispatch ordering, cancellation propagation, and between-turn context manipulation (compaction, model swap, steering injection).

## Turn structure

### Outer loop

A single user input opens one outer iteration. The outer iteration runs inner turns until the assistant produces a turn with no tool calls AND no pending messages remain. After that it polls `get_followup_messages`; if any are produced they become the next iteration's pending messages and the outer loop continues. When follow-ups are empty the loop emits `AgentEnd` and exits.

### Finalization gates

When the assistant tries to stop, `poll_finalization_follow_up` (in `run.rs`) consults a fixed priority order and returns **at most one** gate's follow-up per finalization:

1. **Hook** — caller-supplied `get_followup_messages` (e.g. the `/plan` reviewer loop).
2. **Verifier** — cheap, signal-based: code was edited but no build/test ran, or the last build/test failed. One nudge per run.
3. **Critic** — bounded LLM judgment of *completeness* from the transcript. Once per run, opt-in via `critic_provider`.
4. **Code reviewer** — diff-aware LLM review of *correctness* (dirge-iyf5). On a run that left uncommitted changes it captures the diff, runs a two-pass review (review → verify/dedupe), and splits findings by severity: **high/critical** re-enter the loop as a blocking `[code-review]` nudge (fix or justify), **medium/low** surface as a non-blocking `<system>` advisory. Reuses the `critic_provider` judge — same opt-in as the critic, off with no cost otherwise. Persists across finalizations bounded by `MAX_REVIEW_REACT`. Prompt craft and the finding/verdict model are ported from [roborev](https://go.kenn.io/roborev). Run it manually any time with `/code-review`.
5. **Goal gate** — user-defined `--goal` stop condition, judged by the same provider, bounded by `MAX_GOAL_REACT`.
6. **Todo nudge** — unfinished todos, bounded by `MAX_TODO_NUDGES`.

Because only one gate fires per finalization, lower-priority gates are deferred a turn (intentional: a red build surfaces the verifier nudge now; the critic and reviewer run at the *next* finalization once it's green). All gates fail **open** — an errored judge never blocks finalization.

### Inner loop

Each inner iteration is one assistant turn:

1. Drain pending steering messages into the context, emitting `MessageStart` / `MessageEnd` for each.
2. Call `stream_assistant_response` to produce the next assistant message via the configured `StreamFn`.
3. If the stop reason is `Error` or `Aborted`, emit `TurnEnd` + `AgentEnd` and exit.
4. Collect tool-call blocks from the assistant message. If any exist, dispatch them via `execute_tool_calls` and append the results to the context.
5. Emit `TurnEnd` carrying the assistant message and the batch of tool results.
6. Run between-turn hooks: `prepare_next_turn` (optional context snapshot), `should_stop_after_turn` (early exit), `get_steering_messages` (queue messages for the next inner turn).
7. Continue while either the last turn had tool calls or pending messages exist.

## Hooks

Hooks live on `LoopConfig` and fire at fixed points in the loop. Each is an optional async closure; the loop runs the default behaviour when a hook is `None`.

| Hook | Fires | Returns |
|---|---|---|
| `convert_to_llm` | Before each stream call, on `context.messages` | Filtered `Vec<Value>` of messages the LLM should see (drops custom roles) |
| `transform_context` | Before `convert_to_llm`, once per turn | Mutated `Context` (e.g. compaction, system-prompt rewrite) |
| `before_tool_call` | Per tool call, before dispatch | `BeforeToolCallResult` — pass, mutate args, or block with synthetic result |
| `after_tool_call` | Per tool call, after dispatch | `AfterToolCallResult` — pass, replace result, mark batch terminating |
| `prepare_next_turn` | After each turn (post-tools, pre-stop-check) | Optional snapshot: new `Context`, new model, new thinking level |
| `should_stop_after_turn` | After `prepare_next_turn`, every turn | `bool`; `true` exits the inner loop after the current turn |
| `get_steering_messages` | End of every inner turn | `Vec<LoopMessage>` queued into pending for the next inner turn |
| `get_followup_messages` | End of every outer iteration | `Vec<LoopMessage>` that reopens the outer loop if non-empty |
| `get_api_key` | Per stream call, given provider name | `Option<String>` override for the request |

Plugins reach these hooks through `plugin_hooks.rs` factories that bridge Janet plugin slots (`on-tool-start`, `on-tool-end`, etc.) into the corresponding hook closure.

## Stream pipeline

`stream_assistant_response` is the single path from context to assistant message:

1. Apply `transform_context` if set, producing the context the LLM will see this turn.
2. Apply `convert_to_llm` to filter `context.messages` to LLM-visible roles (`user`, `assistant`, `toolResult`, `system`).
3. Build `StreamOptions` with per-call API key, thinking level, headers, metadata, request timeout, and the shared `AbortSignal`.
4. Call the configured `StreamFn` to obtain an async stream of `StreamEvent`s.
5. Consume the stream: text deltas accumulate into a text block, tool-call deltas accumulate into tool-call blocks, reasoning deltas into thinking blocks. Each is committed on the matching `*End` event.
6. Emit `MessageStart` at the start and `MessageEnd` at the close, carrying the fully-assembled `AssistantMessage` with its `stop_reason`.

The rig adapter (`rig_stream.rs` + `rig_stream_factory.rs`) supplies a `StreamFn` that wraps a `rig::CompletionModel`. Per-provider parameter shapes (Anthropic `thinking`, OpenAI-family `reasoning.effort`, Gemini `thinking_config`, generic `reasoning_level` fallback) are packed in `rig_stream_factory::build_provider_additional_params`.

`retry.rs::retrying_stream_fn` wraps a `StreamFn` to auto-retry transient network and rate-limit errors. Retry only fires before any text or tool-call delta commits; once content has streamed, an error passes through and the loop exits.

## Tool execution

`execute_tool_calls` dispatches the batch of tool calls in a single assistant message. The batch runs sequentially if either:

- `LoopConfig.tool_execution` is `ToolExecutionMode::Sequential`, or
- Any tool in the batch declares `execution_mode() == Some(ToolExecutionMode::Sequential)`.

Otherwise the batch runs in parallel via `tokio::join_all`. Read-only tools (`read`, `grep`, `list_dir`, `find_files`) leave the default `Parallel`; mutating tools (`write`, `edit`, `bash`, `apply_patch`) declare `Sequential` so a batch containing any of them serializes.

In parallel mode, `tool_execution_end` events emit in completion order but the resulting `ToolResultMessage` items appear in source order in the context. Each tool dispatch threads through:

1. `prepare_tool_call` — argument schema validation, applies `tool_input_repair` if validation fails.
2. `before_tool_call` hook — may mutate args, block with a synthetic result, or pass.
3. `execute_prepared_tool_call` — runs the tool future inside a `tokio::select!` against the abort signal; a cancel returns an `aborted` error result within ~50ms.
4. `after_tool_call` hook — may replace the result or set the batch's `terminate` flag.
5. `finalize_executed_tool_call` — emits `ToolExecutionEnd` and builds the `ToolResultMessage`.

If every result in the batch is marked terminating, the inner loop exits after the current turn.

## Repeat-loop guard (reflect-then-pivot)

A `StormBreaker` (`src/agent/agent_loop/storm.rs`) tracks recent `(tool_name, args)` pairs in a sliding window and suppresses a call once it has been issued identically too many times (default: the 3rd identical call). This catches non-progressing loops — an agent re-reading the same file or re-running the same failing command — without relying on the model to notice it's stuck.

The intervention is deliberately *not* a bare "don't repeat yourself". Research on agent loops (and dirge's own experience) shows that simply telling a model to try again tends to **reinforce the same failing chain of reasoning** — the *degeneration-of-thought* / *mental-set* problem. So on the first all-suppressed turn the loop fabricates a tool result carrying a **reflect-then-pivot** prompt (`run.rs`, the `guard_text` in the storm-suppression branch) that forces genuine divergence:

1. State what the call was trying to achieve and why it isn't working.
2. Name the assumption that might be wrong, and what the earlier results actually show.
3. Propose **2–3 fundamentally different approaches** — a different tool, entry point, or interpretation — and pick one.
4. Proceed with that approach; or, if nothing can work with the available tools, say so plainly instead of retrying.

This gives the model one structured shot to self-correct (`turn_self_corrected`). If it keeps producing only suppressed calls afterward, the inner loop exits rather than spinning. The outermost backstop is the `max_turns` cap (see [config.md](config.md)), which stops the run and surfaces a `<system>` notice.

## Phased plan workflow (`/plan`)

An opt-in, per-task workflow (ported from [vix](https://github.com/kirby88/vix)) that splits a complex request into separate, context-isolated phases instead of one long single-agent run. It is an **explicit command**, not a forced mode — regular chat is untouched, and the user decides which tasks warrant it. Gated by `phased_workflow_enabled` (default off; see [config.md](config.md)). The logic lives under `src/agent/plan/`: `workflow.rs` (phase prompts + verdict parsing + the shared `next_review_step` policy) and `runtime.rs` (the runner-drain glue + reviewer fork); the entry is `src/ui/slash/cmd_plan.rs` and the reviewer loop runs in `src/ui/run_handlers/plan_review.rs`.

`/plan <request>` runs four phases:

1. **Explore** — a read-only fork (`READONLY_PHASE_TOOLS`: read/grep/glob/lsp/semantic navigation, no mutation) that builds a frugal understanding of the codebase and emits a structured findings report. A genuine context reset: it starts from a frozen transcript snapshot.
2. **Plan** — a second read-only fork. The **only** thing carried over from Explore is its findings report (true context reset between phases, vix's `fork_from` discipline). It produces a specificity-forced implementation plan against a quality rubric.
3. **Implement** — the plan seeds a normal **streamed** agent turn through the main UI loop, so edits, build/test runs, permission prompts, and interjection all behave exactly as in a regular run. You watch it work.
4. **Review** — after the implement turn completes, a **write-disabled reviewer** fork (`REVIEWER_TOOLS`: read/grep + `bash`, but no `write`/`edit`/`apply_patch`) independently *runs the code* and emits a machine-parsed JSON verdict. A `DONE` verdict ends the workflow; `NEEDS_FIX` feeds the reviewer's punch-list back into another implement turn. Bounded by `phased_workflow_max_review_cycles` (default 2).

The review gate is **asymmetrically cautious**: anything that isn't a parseable `DONE` is treated as not-done, so an ambiguous or malformed verdict triggers another fix cycle rather than shipping — a false `DONE` ships a broken result, a false `NEEDS_FIX` only costs one retry. The reviewer loop is driven event-by-event across `Done` events (in `src/ui/run_handlers/done.rs`) because the implement turn streams through the UI loop and can't be awaited inline; the same per-step policy (`next_review_step`) backs the headless `run_review_loop` so both paths stay in lockstep.

## Cancellation

A single `AbortSignal` is shared end-to-end:

- The outer loop checks the signal between turns via the stream's stop-reason path.
- The stream wrapper polls the signal between chunks and emits an `Error` event mid-stream when triggered.
- Tool execution races the tool future against `wait_for_cancel` and returns an aborted result on signal.

In-flight HTTP requests are not cancelled (rig configures the HTTP client at construction); the loop simply stops reading. The server-side stream is dropped when the connection closes.

## Where it lives

The agent loop lives in `src/agent/agent_loop/`:

| File | Role |
|---|---|
| `run.rs` | `run_loop` / `run_agent_loop` / `run_agent_loop_continue` — the outer/inner loop |
| `stream.rs` | `stream_assistant_response` and the `StreamFn` trait |
| `tools.rs` | `execute_tool_calls` dispatcher; sequential and parallel paths; `prepare_tool_call`, `execute_prepared_tool_call`, `finalize_executed_tool_call` |
| `hooks.rs` | All hook function-type aliases and `TurnHookContext` |
| `types.rs` | `Context`, `LoopConfig`, `TurnUpdate`, `ThinkingLevel`, `ThinkingBudgets`, `ToolExecutionMode`, `QueueMode` |
| `message.rs` | `LoopMessage`, `AssistantMessage`, `ToolResultMessage`, `UserMessage`, `ContentBlock`, `StreamEvent`, `LoopEvent` |
| `result.rs` | `LoopToolResult`, `BeforeToolCallResult`, `AfterToolCallResult` |
| `tool.rs` | `LoopTool` trait, `AbortSignal`, `LoopToolUpdate` |
| `bridge.rs` | `LoopEvent` → `AgentEvent` translation for UI / ACP consumers |
| `integration.rs` | `spawn_loop_runner` composition into a `LoopRunner` |
| `rig_stream.rs` | `wrap_rig_stream` adapter from `rig::StreamingCompletionResponse` to `StreamEvent` |
| `rig_stream_factory.rs` | `rig_stream_fn_from_model_with_provider` and per-provider reasoning mapping |
| `rig_tool.rs` | `RigToolAdapter` — wraps `rig::ToolDyn` as `LoopTool` |
| `retry.rs` | `retrying_stream_fn` — transient-error recovery around a `StreamFn` |
| `steering.rs` | `steering_from_queue` — shared queue → `GetSteeringMessagesFn` |
| `plugin_hooks.rs` | Factories that adapt Janet plugin hooks to the loop hook surface |
| `tool_input_repair/` | Validate-then-repair pass for malformed tool-call arguments |
| `context_manager.rs` | Compaction policy and dispatch for `transform_context` |

## Production wiring

Every `provider::AnyAgent::spawn_runner` call composes the loop the same way:

```
spawn_runner(prompt, history) -> AgentRunner
  tool_defs       = loop_tools → rig tool definitions
  inner_stream_fn = build_stream_fn(tool_defs)
  stream_fn       = retrying_stream_fn(inner_stream_fn, RecoveryPolicy::default())
  cfg             = LoopSpawnConfig { stream_fn, system_prompt, history, prompt, tools, plugin_mgr, provider_name, ... }
  spawn_loop_runner(cfg).into_agent_runner()
```

The headless non-streaming path (`runner::run_print`) and history conversion (`runner::convert_history`) bypass the loop and stay in `runner.rs`.
