# Janet Compression Plugin

**Token reduction for LLM tool output — 75% fewer chars across 10 command families.**

## What it does

Every time dirge runs a shell command (`bash`, `bash_output`), the output goes through this plugin before reaching the LLM. The plugin strips ANSI escape codes, removes progress bars and spinner noise, and applies per-command compressors that restructure verbose CLI output into dense, information-preserving summaries.

The result: the model sees the same semantic information in a fraction of the tokens, leaving more context window for reasoning.

## Numbers

| Family | Sample | Before | After | Reduction |
|--------|--------|--------|-------|-----------|
| git | status (dirty) | 536 | 91 | 83% |
| cargo | build | 116 | 26 | 78% |
| cargo | clippy (warnings) | 418 | 10 | 98% |
| docker | ps (3 containers) | 466 | 108 | 77% |
| kubectl | describe pod | 832 | 244 | 71% |
| npm | install | 172 | 13 | 92% |
| pip | install | 1,328 | 106 | 92% |
| grep | 40 matches | 1,590 | 825 | 48% |
| find | 30 results | 680 | 370 | 46% |
| ls | 50 entries | 2,592 | 29 | 99% |

**17 realistic samples: 10,361 → 2,627 chars (75% overall).**

See [`tests/bench.janet`](tests/bench.janet) for methodology and thresholds.

## Why this matters

LLMs charge per token and have finite context windows. CLI tools produce notoriously verbose output — progress bars, ASCII tables padded to 80 columns, git status boilerplate, download spinners. None of this helps the model reason about what happened.

The insight is straightforward: **compress the tool result, not the prompt.** The model doesn't need to see `" total"` on every line of `ls -la` or the full `CONTAINER ID   IMAGE   COMMAND   CREATED   STATUS   PORTS   NAMES` header on every `docker ps`. It needs to know what ran, what changed, and whether it succeeded.

This approach is lossy in surface text but lossless in *information*. A compressed `git status` that reads `main↑3 staged: ~src/main.rs untracked: TODO.txt` tells the model everything the full 20-line output would, in 15% of the characters.

## Inspirations

The idea of compressing tool output for LLM context has been explored in a few projects we've learned from:

- **[lean-ctx](https://github.com/yvgude/lean-ctx)** — The direct ancestor. lean-ctx implemented the original Rust-based compressor library that this plugin ports. The per-family routing table, the "first non-nil wins" dispatch pattern, and most of the regex patterns are adapted from lean-ctx's `mod.rs` and family modules. The key difference: lean-ctx ran inline in the Rust agent loop; dirge moves it into a hot-swappable Janet plugin.

- **[headroom](https://github.com/yvgude/headroom)** — A focused experiment in stripping ANSI codes, progress bars, and spinner noise from any CLI output. headroom's noise-removal pipeline (carriage-return detection, download-line filtering, blank-run collapsing) directly informed `compress-generic` in `init.janet`. The insight that "you can safely delete a *lot* of terminal output without losing meaning" came from watching headroom's pass-through on real build logs.

- **Prompt caching / context distillation** — Anthropic's prompt caching and various academic papers on context distillation (e.g. "LLMLingua", "Selective Context") ask the question "how do you fit more reasoning into a fixed context window?" from the other direction — they compress *prompts*, we compress *tool results*. The general principle is the same: the model's context window is precious real estate, and anything that doesn't carry signal is rent you're paying for nothing.

We're not the first to notice that CLI tools are extremely verbose, but we *are* the first to ship it as a Janet plugin that works across ten command families without touching a single line of the agent loop. If you have a tool whose output you'd like compressed, the pattern is easy to copy — see Contributing below.

## How it works

### Architecture

```
Rust (dirge)                    Janet (this plugin)
────────────────────────────    ──────────────────────────
on-tool-end hook           →    strip-ansi
    passes {:tool "bash"        compress-generic (spin bars, blank runs)
             :output "..."      per-command compressor (git/cargo/docker/...)
             :command "..."}    dispatch via command routing table
                            →  harness/replace-result with compressed output
```

The plugin loads as a single shared Janet environment. Files are eval'd in alphabetical order, so symbols from `00-regex.janet` are available in `10-git.janet`, `init.janet`, etc.

### Pipeline

1. **ANSI stripping** — CSI escape sequences (`\x1b[31m`, `\x1b[0J`) are removed with a PEG pattern.
2. **Generic noise removal** — progress bars (lines containing `\r`), download spinners (`Downloading ...`, `Collecting ...`), and runs of >3 blank lines are collapsed.
3. **Per-command compression** — each compressor inspects the command string and either handles it (returning a compressed string) or returns `nil` (pass, next compressor tries). The first non-`nil` result wins.

### Compressor design patterns

- **Table summarization** — `kubectl get pods` counts by STATUS column; `docker ps` extracts `name (image): status`. Drops headers, keeps each row as one line.
- **Structural extraction** — `git status` pulls branch name, ahead/behind count, and changed file lists into `branch↑N staged: ... unstaged: ...` format.
- **Deduplication** — `kubectl logs` and `docker logs` collapse repeated lines with `(xN)` suffixes.
- **Stat extraction** — `git merge`/`pull`/`cherry-pick` extract `N files, +M/-K` from the trailing summary line.
- **Passthrough** — commands with ≤10 lines of output or that don't match any pattern pass through unchanged. Already-terse output (like `git diff --stat` or small `curl` responses) is left alone.

## Files

```
.dirge/plugins/compression/
├── 00-regex.janet       Vendored spork/regex (MIT) — regex→PEG compiler
├── 10-git.janet         git (19 subcommands: status, log, diff, branch, ...)
├── 20-cargo.janet       cargo (10 subcommands: build, test, clippy, ...)
├── 30-docker.janet      docker + compose (12 subcommands: ps, images, build, ...)
├── 40-kubectl.janet     kubectl (9 subcommands: get, logs, describe, apply, ...)
├── 45-npm.janet         npm/yarn/pnpm (6 subcommands: install, test, audit, ...)
├── 50-misc.janet        pip, grep/rg, find/fd, ls, curl
├── init.janet           ANSI strip, generic noise removal, on-tool-end hook,
│                        main dispatch routing
└── tests/
    ├── bench.janet      A/B compression benchmark with realistic samples
    └── test.janet       Unit tests for each compressor + tool-gate tests
```

## Running the tests

```bash
# Unit tests (23 tests, all compressors + tool gate)
janet .dirge/plugins/compression/tests/test.janet

# A/B benchmark (17 realistic samples, threshold checks)
janet .dirge/plugins/compression/tests/bench.janet
```

## Porting from lean-ctx

This plugin ports the compression patterns from [lean-ctx](https://github.com/yvgude/lean-ctx) (originally in Rust) into pure Janet. The pattern logic is equivalent; the implementation differs in:

- **Regex engine** — lean-ctx uses Rust's `regex` crate (native, backtracking). dirge uses vendored spork/regex compiled to Janet PEGs (non-backtracking, anchored). Some patterns needed restructuring (e.g., `finished-match` uses `string/find` instead of a lazy regex to avoid PEG greediness).
- **Dispatch** — lean-ctx routes by command prefix in Rust; dirge routes by a priority-ordered `or` chain in Janet. Same semantics, different mechanism.
- **Shared environment** — Janet's single shared namespace across eval'd files means cross-file symbol references work naturally without imports. `init.janet` can call `git-compress` defined in `10-git.janet` because the files are loaded in order.

## Security

- **Command injection**: The `:command` field is user-controlled (it's the command the model asked dirge to run). It flows through `escape_janet_string` in Rust before being embedded in a Janet struct literal, preventing escape of the string context. Inside Janet, command strings are only used for `string/find` routing — never passed to `os/shell`, `os/execute`, or any evaluator.
- **Output sanitization**: ANSI stripping and noise removal run before any per-command compressor, so non-printable content in tool output is neutralized early.

## License

The compression patterns and plugin integration are part of dirge.

`00-regex.janet` is vendored from [janet-lang/spork](https://github.com/janet-lang/spork) under the MIT license. The full license text is included at the top of that file.

## Contributing

Compressors follow a consistent pattern:

```janet
(defn- my-compress-specific [output]
  # Per-subcommand logic: extract, dedup, summarize
  ...)

(defn my-compress [command output]
  (if (not (string/find "mycommand" command)) (break nil))
  (cond
    (string/find "sub1" command) (my-compress-specific output)
    nil))
```

- Public dispatch functions accept `[command output]` and return a compressed string or `nil`.
- Private helpers accept `[output]` and return a string.
- `(break nil)` is the standard way to early-exit a `defn` with `nil` in Janet.
- Use `cond` for subcommand routing, `case` where the subcommand is already extracted.
- Add a `test-compressor` entry in `tests/test.janet` for each family (at minimum: one smoke test, one "non-X command returns nil" test).
