#!/usr/bin/env python3
"""Aider Polyglot driver for dirge (dirge-4ga8).

dirge ships roughly fifty behavioural guards — the storm breaker, the failure
tracker, the progress monitor, the capability tier, the claim / source /
completeness gates, safe-state, reflexion, the verifier — every one of them
tuned by hand against reasoning and anecdote. Nothing measures whether they
help, and nothing notices when one regresses. This is the smallest harness that
changes that.

Aider Polyglot is the cheapest useful benchmark to start from: 225 self-
contained exercises across six languages, each with a spec, a stub, and a real
test suite, and no infrastructure beyond the language toolchains.

    git clone https://github.com/Aider-AI/polyglot-benchmark /tmp/polyglot
    python3 benchmarks/polyglot.py --exercises /tmp/polyglot --out results.json

Protocol, matching upstream so numbers are comparable:

  1. copy the exercise to a scratch directory (the source tree is never
     mutated, so a run is repeatable and a crashed run leaves nothing behind);
  2. strip the skip markers from the test files (see `runners.py`);
  3. run the agent with the instructions and the list of files it may edit;
  4. run the suite. If it passes, done;
  5. otherwise run the agent a second time with the failure output appended,
     and score that.

The agent is driven through `--print --output-format stream-json`, so this
measures the same headless path `--loop` and the MCP server use.

Results are written atomically after every exercise, so `status.py` can watch a
run in progress and a killed run keeps everything it finished.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from runners import LANGUAGES, failure_excerpt, strip_skip_markers, suite_passed  # noqa: E402

# Exercism ships reference solutions next to the exercise. They must never be
# copied into the agent's working directory.
EXCLUDED_DIRS = {".meta", ".approaches", ".articles", ".git"}

FIRST_PROMPT = """\
Implement the exercise described in the instructions below.

Edit ONLY these files: {files}
Do not modify the test files — they are the specification and will be restored \
before scoring. Do not create new top-level files unless the instructions ask for them.

When you are done, run the tests yourself and fix anything that fails.

--- INSTRUCTIONS ---
{instructions}
"""

RETRY_PROMPT = """\
Your implementation does not pass the tests. Here is the output:

--- TEST OUTPUT ---
{failure}

Fix the implementation in {files}. Do not modify the test files.
"""


@dataclass
class ExerciseResult:
    language: str
    slug: str
    passed: bool
    attempts: int
    duration_s: float
    # Per-attempt agent telemetry, straight from the result envelope.
    turns: list[int]
    cost_usd: float
    error: str | None = None


def find_exercises(root: Path, languages: list[str]) -> list[tuple[str, Path]]:
    """Every `<lang>/exercises/practice/<slug>` directory, sorted."""
    found: list[tuple[str, Path]] = []
    for language in languages:
        practice = root / language / "exercises" / "practice"
        if not practice.is_dir():
            continue
        for slug_dir in sorted(p for p in practice.iterdir() if p.is_dir()):
            found.append((language, slug_dir))
    return found


def read_instructions(exercise: Path) -> str:
    """The exercise spec.

    `.docs/instructions.md` plus the optional `.append.md`, which carries the
    edge cases and exact output formats the tests assert — dropping it makes
    several exercises unsolvable-by-reading.
    """
    parts = []
    for name in ("instructions.md", "instructions.append.md"):
        path = exercise / ".docs" / name
        if path.is_file():
            parts.append(path.read_text(encoding="utf-8", errors="replace"))
    return "\n\n".join(parts).strip()


def matching_files(root: Path, globs: tuple[str, ...]) -> list[Path]:
    out: list[Path] = []
    for pattern in globs:
        out.extend(sorted(root.glob(pattern)))
    return [p for p in out if p.is_file()]


def stage_exercise(exercise: Path, dest: Path, language: str) -> None:
    """Copy the exercise to `dest` and strip skip markers from its tests."""
    shutil.copytree(
        exercise,
        dest,
        ignore=shutil.ignore_patterns(*EXCLUDED_DIRS),
        dirs_exist_ok=True,
    )
    spec = LANGUAGES[language]
    for test_file in matching_files(dest, spec.test_globs):
        original = test_file.read_text(encoding="utf-8", errors="replace")
        stripped = strip_skip_markers(language, original)
        if stripped != original:
            test_file.write_text(stripped, encoding="utf-8")


@dataclass
class TestOutcome:
    """Result of running an exercise's suite.

    `harness_error` is the distinction that matters: a suite that RAN and
    failed is a legitimate agent miss, while a missing toolchain or a wedged
    runner is the harness's problem. Collapsing the two would quietly report
    "the model failed 40 exercises" when the truth was "go isn't installed" —
    a benchmark that produces a plausible wrong number is worse than one that
    refuses to produce a number at all.
    """

    passed: bool
    output: str
    harness_error: str | None = None


def run_tests(workdir: Path, language: str, timeout: int) -> TestOutcome:
    spec = LANGUAGES[language]
    env = {**os.environ, **spec.test_env}
    try:
        proc = subprocess.run(
            spec.test_command,
            cwd=workdir,
            env=env,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return TestOutcome(False, "", f"test run exceeded {timeout}s")
    except (FileNotFoundError, PermissionError) as exc:
        return TestOutcome(False, "", f"toolchain missing: {exc}")
    ok = suite_passed(language, proc.returncode, proc.stdout, proc.stderr)
    return TestOutcome(ok, failure_excerpt(proc.stdout, proc.stderr))


def run_agent(
    workdir: Path,
    prompt: str,
    dirge_bin: str,
    model: str | None,
    max_turns: int,
    timeout: int,
) -> dict:
    """One headless dirge run. Returns the parsed `result` envelope.

    A crash, a timeout, or unparseable output all come back as a synthetic
    envelope with `is_error`, so one bad exercise never takes down the run.
    """
    argv = [
        dirge_bin,
        "--print",
        "--output-format",
        "stream-json",
        "--accept-all",
        "--max-agent-turns",
        str(max_turns),
    ]
    if model:
        argv += ["--model", model]
    argv.append(prompt)

    try:
        proc = subprocess.run(
            argv,
            cwd=workdir,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return {"is_error": True, "subtype": "harness_timeout", "num_turns": 0}
    except FileNotFoundError:
        raise SystemExit(f"dirge binary not found: {dirge_bin!r} — pass --dirge-bin")

    # The result envelope is the last `{"type": "result"}` line. Scanning for
    # it rather than taking the last line tolerates a trailing newline or a
    # stray write from a subprocess the agent spawned.
    envelope: dict | None = None
    for line in proc.stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "result":
            envelope = event
    if envelope is None:
        return {
            "is_error": True,
            "subtype": "no_result_envelope",
            "num_turns": 0,
            "stderr_tail": proc.stderr[-2000:],
        }
    return envelope


def run_exercise(
    language: str,
    exercise: Path,
    args: argparse.Namespace,
) -> ExerciseResult:
    slug = exercise.name
    started = time.monotonic()
    turns: list[int] = []
    cost = 0.0

    with tempfile.TemporaryDirectory(prefix=f"polyglot-{language}-") as tmp:
        workdir = Path(tmp) / slug
        try:
            stage_exercise(exercise, workdir, language)
        except OSError as exc:
            return ExerciseResult(
                language, slug, False, 0, time.monotonic() - started, turns, cost,
                error=f"staging failed: {exc}",
            )

        instructions = read_instructions(workdir)
        if not instructions:
            return ExerciseResult(
                language, slug, False, 0, time.monotonic() - started, turns, cost,
                error="no instructions found",
            )

        spec = LANGUAGES[language]
        editable = [
            str(p.relative_to(workdir))
            for p in matching_files(workdir, spec.solution_globs)
            if p not in set(matching_files(workdir, spec.test_globs))
        ]
        if not editable:
            return ExerciseResult(
                language, slug, False, 0, time.monotonic() - started, turns, cost,
                error="no editable solution files matched",
            )
        files = ", ".join(editable)

        # Snapshot the test files so an agent that edits them (which the prompt
        # forbids, but forbidding is not preventing) can't score itself a pass.
        test_files = {
            p: p.read_bytes() for p in matching_files(workdir, spec.test_globs)
        }

        prompt = FIRST_PROMPT.format(files=files, instructions=instructions)
        for attempt in range(1, args.attempts + 1):
            envelope = run_agent(
                workdir, prompt, args.dirge_bin, args.model,
                args.max_turns, args.agent_timeout,
            )
            turns.append(int(envelope.get("num_turns") or 0))
            cost += float(envelope.get("total_cost_usd") or 0.0)

            for path, content in test_files.items():
                path.write_bytes(content)

            outcome = run_tests(workdir, language, args.test_timeout)
            if outcome.passed:
                return ExerciseResult(
                    language, slug, True, attempt,
                    time.monotonic() - started, turns, cost,
                )
            if outcome.harness_error:
                # Retrying can't help — the suite never ran. Stop and record
                # why, so `status.py` reports it apart from the miss count.
                return ExerciseResult(
                    language, slug, False, attempt,
                    time.monotonic() - started, turns, cost,
                    error=outcome.harness_error,
                )
            prompt = RETRY_PROMPT.format(failure=outcome.output, files=files)

        return ExerciseResult(
            language, slug, False, args.attempts,
            time.monotonic() - started, turns, cost,
        )


def write_results(path: Path, payload: dict) -> None:
    """Write via a temp file + rename, so `status.py` never reads a half file."""
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    tmp.replace(path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--exercises", required=True, type=Path,
        help="clone of Aider-AI/polyglot-benchmark",
    )
    parser.add_argument("--out", type=Path, default=Path("polyglot-results.json"))
    parser.add_argument(
        "--languages", default=",".join(LANGUAGES),
        help="comma-separated subset to run",
    )
    parser.add_argument("--dirge-bin", default="dirge")
    parser.add_argument("--model", default=None)
    parser.add_argument("--attempts", type=int, default=2)
    parser.add_argument("--max-turns", type=int, default=40)
    parser.add_argument("--agent-timeout", type=int, default=900)
    parser.add_argument("--test-timeout", type=int, default=300)
    parser.add_argument(
        "--limit", type=int, default=0,
        help="stop after N exercises (a smoke run; 0 = all)",
    )
    args = parser.parse_args()

    # The agent runs with `cwd` set to the exercise workdir, so a relative
    # `--dirge-bin` (like the `target/release/dirge` the README suggests) would
    # resolve against the wrong directory. Anything with a separator is pinned
    # now; a bare name is left alone so PATH lookup still works.
    if os.sep in args.dirge_bin or (os.altsep and os.altsep in args.dirge_bin):
        args.dirge_bin = str(Path(args.dirge_bin).resolve())

    languages = [x.strip() for x in args.languages.split(",") if x.strip()]
    unknown = [x for x in languages if x not in LANGUAGES]
    if unknown:
        parser.error(f"unknown language(s): {', '.join(unknown)}")

    exercises = find_exercises(args.exercises, languages)
    if not exercises:
        parser.error(f"no exercises under {args.exercises} for {languages}")
    if args.limit:
        exercises = exercises[: args.limit]

    # Report the languages that actually yielded exercises, not the ones asked
    # for: "225 across 6" when five toolchains resolved nothing is how a run
    # silently measures a sixth of what you thought it did.
    found_languages = sorted({lang for lang, _ in exercises})
    print(
        f"{len(exercises)} exercises across {len(found_languages)} language(s): "
        f"{', '.join(found_languages)}",
        flush=True,
    )
    missing = [x for x in languages if x not in found_languages]
    if missing:
        print(f"warning: no exercises found for {', '.join(missing)}", flush=True)
    results: list[ExerciseResult] = []
    started = time.monotonic()

    for index, (language, exercise) in enumerate(exercises, start=1):
        result = run_exercise(language, exercise, args)
        results.append(result)
        passed = sum(1 for r in results if r.passed)
        mark = "PASS" if result.passed else "FAIL"
        note = f" ({result.error})" if result.error else ""
        print(
            f"[{index}/{len(exercises)}] {mark} {language}/{result.slug} "
            f"— {passed}/{len(results)} = {passed / len(results):.1%}{note}",
            flush=True,
        )
        write_results(
            args.out,
            {
                "model": args.model,
                "languages": languages,
                "attempts": args.attempts,
                "total": len(exercises),
                "completed": len(results),
                "passed": passed,
                "elapsed_s": round(time.monotonic() - started, 1),
                "results": [asdict(r) for r in results],
            },
        )

    passed = sum(1 for r in results if r.passed)
    print(f"\n{passed}/{len(results)} = {passed / len(results):.2%}")
    print(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
