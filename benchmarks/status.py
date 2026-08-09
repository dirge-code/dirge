#!/usr/bin/env python3
"""Read a Polyglot results file while the run is still going.

`polyglot.py` rewrites its output atomically after every exercise, so this is
safe to call at any point — including against a run that was killed, which is
when the per-language breakdown is most worth having.

    python3 benchmarks/status.py polyglot-results.json
"""

from __future__ import annotations

import argparse
import json
from collections import defaultdict
from pathlib import Path


def bar(fraction: float, width: int = 24) -> str:
    filled = round(fraction * width)
    return "#" * filled + "." * (width - filled)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("results", type=Path, nargs="?", default=Path("polyglot-results.json"))
    args = parser.parse_args()

    if not args.results.is_file():
        print(f"no results file at {args.results}")
        return 1
    payload = json.loads(args.results.read_text(encoding="utf-8"))
    results = payload.get("results", [])
    if not results:
        print("no exercises completed yet")
        return 0

    per_language: dict[str, list[dict]] = defaultdict(list)
    for row in results:
        per_language[row["language"]].append(row)

    print(f"model: {payload.get('model') or '(config default)'}")
    completed, total = payload.get("completed", len(results)), payload.get("total", len(results))
    elapsed = payload.get("elapsed_s", 0.0)
    print(f"progress: {completed}/{total}  elapsed {elapsed / 60:.1f}m")
    if completed and completed < total:
        eta = elapsed / completed * (total - completed)
        print(f"eta: {eta / 60:.1f}m at the current rate")
    print()

    for language in sorted(per_language):
        rows = per_language[language]
        passed = sum(1 for r in rows if r["passed"])
        rate = passed / len(rows)
        print(f"  {language:<11} {bar(rate)} {passed:>3}/{len(rows):<3} {rate:6.1%}")

    passed = sum(1 for r in results if r["passed"])
    print(f"\n  {'overall':<11} {bar(passed / len(results))} {passed:>3}/{len(results):<3} "
          f"{passed / len(results):6.1%}")

    # First-attempt rate is the more honest signal for scaffold work: the retry
    # hands the model its own test output, which papers over a bad first move.
    first_try = sum(1 for r in results if r["passed"] and r["attempts"] == 1)
    print(f"  {'first try':<11} {bar(first_try / len(results))} {first_try:>3}/{len(results):<3} "
          f"{first_try / len(results):6.1%}")

    cost = sum(r.get("cost_usd") or 0.0 for r in results)
    turns = [t for r in results for t in r.get("turns", [])]
    if turns:
        print(f"\n  turns/attempt: mean {sum(turns) / len(turns):.1f}  max {max(turns)}")
    if cost:
        print(f"  cost: ${cost:.2f}")

    errored = [r for r in results if r.get("error")]
    if errored:
        print(f"\n  {len(errored)} harness error(s) — not agent failures:")
        for row in errored[:10]:
            print(f"    {row['language']}/{row['slug']}: {row['error']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
