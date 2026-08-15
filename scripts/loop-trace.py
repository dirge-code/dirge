#!/usr/bin/env python3
"""loop-trace.py — render a `dirge --trace` JSONL file as a readable timeline.

The trace is one JSON object per loop decision (see
`src/agent/agent_loop/trace.rs`). This turns it into something you can read top
to bottom, and prints a summary that answers the questions a harness review
actually asks: how many turns, what did each cost, which guards fired, and
whether anything the model reached for was not there.

    scripts/loop-trace.py run.jsonl              # timeline + summary
    scripts/loop-trace.py run.jsonl --summary    # summary only
    scripts/loop-trace.py run.jsonl --kind tool_start,tool_end

Reads stdin when no path is given.
"""

import argparse
import json
import sys
from collections import Counter

# Timeline glyphs. A harness intervention gets the loud one — it is the event
# the whole trace exists to make visible.
GLYPH = {
    "run_start": "▶",
    "agent_start": "▶",
    "agent_end": "■",
    "turn_start": "·",
    "turn_end": "·",
    "message": " ",
    "tool_start": "→",
    "tool_end": "←",
    "usage": "$",
    "context": "◇",
    "compaction_start": "◆",
    "compacted": "◆",
    "checkpoint": "◇",
    "retry": "↻",
    "system_notice": "!",
    "repairs": "⊕",
    "escalation": "↑",
}


def parse(stream):
    """Records, skipping anything unparseable — and saying how many.

    A trace is written by a process that may have been killed mid-line, so a
    truncated tail is normal. Silently dropping it is not: a summary computed
    over half a run reads exactly like a summary of a short run.
    """
    records, bad = [], 0
    for line in stream:
        line = line.strip()
        if not line:
            continue
        try:
            records.append(json.loads(line))
        except json.JSONDecodeError:
            bad += 1
    return records, bad


def render_line(rec):
    kind = rec.get("kind", "?")
    glyph = GLYPH.get(kind, "?")
    ms = rec.get("ms", 0)
    stamp = f"{ms / 1000:7.2f}s"

    if kind == "run_start":
        tools = rec.get("tools") or []
        body = (
            f"run start — model={rec.get('model')} ctx_max={rec.get('ctx_max')} "
            f"max_turns={rec.get('max_turns')}\n"
            f"          {len(tools)} tools: {', '.join(tools)}"
        )
    elif kind == "message":
        role = rec.get("role")
        if role == "intervention":
            # The guard that steered the model, and its own account of why.
            body = (
                f"\033[1mINTERVENTION {rec.get('guard')}\033[0m — {rec.get('why')}\n"
                f"          {rec.get('text', '')}"
            )
        elif role == "assistant":
            calls = rec.get("tool_calls", 0)
            suffix = f" [{calls} tool call(s)]" if calls else ""
            body = f"assistant{suffix}: {rec.get('text', '')}"
        elif role == "user":
            body = f"user: {rec.get('text', '')}"
        elif role == "tool_result":
            body = f"tool_result {rec.get('tool')}{' ERROR' if rec.get('error') else ''}"
        else:
            body = f"{role}: {rec.get('text', '')}"
    elif kind == "tool_start":
        body = f"{rec.get('tool')}({rec.get('args', '')})"
    elif kind == "tool_end":
        body = f"{rec.get('tool')} {'ERROR ' if rec.get('error') else ''}→ {rec.get('output', '')}"
    elif kind == "usage":
        body = (
            f"tokens in={rec.get('input')} out={rec.get('output')} "
            f"cached={rec.get('cached')}"
        )
    elif kind == "context":
        ratio = rec.get("ratio", 0)
        verdict = rec.get("verdict")
        note = ""
        if verdict == "ExitWithSummary":
            note = "  ← turn force-ended"
        elif verdict and verdict != "None":
            note = f"  ← {verdict}"
        body = (
            f"context {rec.get('prompt_tokens')}/{rec.get('ctx_max')} "
            f"= {ratio:.1%} {verdict}{note}"
        )
    elif kind == "compacted":
        body = (
            f"compacted {rec.get('tokens_before')} → {rec.get('tokens_after')} "
            f"tokens ({rec.get('how')})"
        )
    elif kind == "turn_end":
        body = f"turn end ({rec.get('stop_reason')}, {rec.get('tool_results')} results)"
    elif kind == "system_notice":
        body = f"\033[1mNOTICE\033[0m {rec.get('text', '')}"
    elif kind == "retry":
        body = f"retry #{rec.get('attempt')} after {rec.get('delay_ms')}ms: {rec.get('error')}"
    elif kind == "repairs":
        body = f"repaired {rec.get('repaired')} input(s), {rec.get('invalid')} invalid"
    else:
        body = json.dumps({k: v for k, v in rec.items() if k not in ("ms", "seq", "kind")})

    return f"{stamp} {glyph} {body}"


def summarize(records):
    out = []
    kinds = Counter(r.get("kind") for r in records)

    start = next((r for r in records if r.get("kind") == "run_start"), None)
    if start:
        out.append(f"model        {start.get('model')}")
        out.append(f"window       {start.get('ctx_max')} tokens")
        out.append(f"tools        {len(start.get('tools') or [])}")

    # Turns: count assistant messages, which is one per model call. `turn_start`
    # is not emitted for the first turn, so counting those undercounts by one.
    turns = sum(
        1 for r in records if r.get("kind") == "message" and r.get("role") == "assistant"
    )
    out.append(f"turns        {turns}")

    tool_ends = [r for r in records if r.get("kind") == "tool_end"]
    errored = [r for r in tool_ends if r.get("error")]
    out.append(f"tool calls   {len(tool_ends)} ({len(errored)} errored)")
    if tool_ends:
        by_tool = Counter(r.get("tool") for r in tool_ends)
        out.append(
            "  by tool    "
            + ", ".join(f"{name}×{n}" for name, n in by_tool.most_common())
        )
    if errored:
        by_err = Counter(r.get("tool") for r in errored)
        out.append(
            "  errored    "
            + ", ".join(f"{name}×{n}" for name, n in by_err.most_common())
        )

    # The headline: which guards steered the model, and how often.
    steers = [r for r in records if r.get("kind") == "message" and r.get("role") == "intervention"]
    if steers:
        by_guard = Counter(r.get("guard") for r in steers)
        out.append(f"interventions {len(steers)}")
        for guard, n in by_guard.most_common():
            why = next(r["why"] for r in steers if r.get("guard") == guard)
            out.append(f"  {guard} ×{n} — {why}")
    else:
        out.append("interventions 0")

    # Context health. A run that force-ends turns is a run being truncated, and
    # it is invisible in every other number here.
    ctx = [r for r in records if r.get("kind") == "context"]
    if ctx:
        peak = max(r.get("ratio", 0) for r in ctx)
        forced = sum(1 for r in ctx if r.get("verdict") == "ExitWithSummary")
        folds = sum(1 for r in ctx if r.get("verdict") == "Fold")
        out.append(f"context peak {peak:.1%} of window")
        if forced:
            out.append(f"  FORCE-ENDED {forced} turn(s) — context over the window")
        if folds:
            out.append(f"  folded      {folds} time(s)")

    usage = [r for r in records if r.get("kind") == "usage"]
    if usage:
        out.append(
            f"tokens       {sum(r.get('input', 0) for r in usage)} in / "
            f"{sum(r.get('output', 0) for r in usage)} out / "
            f"{sum(r.get('cached', 0) for r in usage)} cached"
        )

    if kinds.get("retry"):
        out.append(f"retries      {kinds['retry']}")
    if kinds.get("compacted"):
        out.append(f"compactions  {kinds['compacted']}")

    if records:
        out.append(f"wall clock   {records[-1].get('ms', 0) / 1000:.1f}s")
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("path", nargs="?", help="trace file (default: stdin)")
    ap.add_argument("--summary", action="store_true", help="summary only")
    ap.add_argument("--kind", help="comma-separated kinds to show")
    args = ap.parse_args()

    stream = open(args.path) if args.path else sys.stdin
    records, bad = parse(stream)
    if args.path:
        stream.close()

    if not records:
        print("empty trace", file=sys.stderr)
        return 1

    if not args.summary:
        wanted = set(args.kind.split(",")) if args.kind else None
        for rec in records:
            if wanted and rec.get("kind") not in wanted:
                continue
            print(render_line(rec))
        print()

    print("── summary " + "─" * 50)
    for line in summarize(records):
        print(line)
    if bad:
        print(f"\n{bad} unparseable line(s) skipped — the trace may be truncated")
    return 0


if __name__ == "__main__":
    sys.exit(main())
