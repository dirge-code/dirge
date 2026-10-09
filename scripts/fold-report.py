#!/usr/bin/env python3
"""Pass-rate and cost-per-pass report over kept loop-ab.sh runs.

Reads one or more loop-ab work directories (run the harness with
LOOP_AB_KEEP=1) and reports, per arm:

  n, runs that folded (the mechanism gate), passes with a Wilson 95% CI,
  mean input / cached / output tokens with a 95% CI, token-priced cost per
  run, cost per pass, turns, folds per run, and re-reads (read calls on a
  path the run had already read: the visible cost of a fold that lost
  something the model then went back for).

With --compare A B it also reports the paired comparison: runs are paired by
(work dir, repeat index), an exact two-sided sign test over the discordant
pass/fail pairs, and a paired 95% CI on the input-token delta.

Prices are per million tokens and must be given explicitly: a default price
would quietly misreport any other model.

Usage:
  scripts/fold-report.py WORKDIR [WORKDIR...] --price-in P --price-cached P
      --price-out P [--pool NAME] [--compare ARM_A ARM_B] [--json OUT]

--pool NAME relabels every arm as NAME (an A/A run of one configuration is a
single arm of 2n runs; the split halves are still reported with --compare).
"""

import argparse
import json
import math
import os
import statistics
import sys
from collections import defaultdict

# results.tsv columns (1-based in loop-ab.sh, 0-based here).
C_TAG, C_MODEL, C_REPEAT, C_TURNS = 0, 1, 2, 3
C_CORRECT, C_IN, C_CACHED, C_SESS, C_COMPACT, C_OUT = 13, 20, 21, 23, 25, 31


def wilson(k, n, z=1.96):
    if n == 0:
        return (float("nan"), float("nan"))
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return (max(0.0, c - h), min(1.0, c + h))


# Two-sided 97.5% t quantiles for small samples; normal beyond the table.
T975 = {1: 12.706, 2: 4.303, 3: 3.182, 4: 2.776, 5: 2.571, 6: 2.447, 7: 2.365,
        8: 2.306, 9: 2.262, 10: 2.228, 11: 2.201, 12: 2.179, 13: 2.160,
        14: 2.145, 15: 2.131, 16: 2.120, 17: 2.110, 18: 2.101, 19: 2.093,
        20: 2.086, 25: 2.060, 30: 2.042, 40: 2.021, 60: 2.000}


def t975(df):
    if df in T975:
        return T975[df]
    keys = sorted(k for k in T975 if k <= df)
    return T975[keys[-1]] if keys and df < 60 else 1.96


def mean_ci(xs):
    if not xs:
        return (float("nan"), float("nan"), float("nan"))
    m = statistics.fmean(xs)
    if len(xs) < 2:
        return (m, float("nan"), float("nan"))
    h = t975(len(xs) - 1) * statistics.stdev(xs) / math.sqrt(len(xs))
    return (m, m - h, m + h)


def sign_test(b, c):
    """Exact two-sided binomial test on b vs c discordant pairs."""
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    tail = sum(math.comb(n, i) for i in range(k + 1)) / 2 ** n
    return min(1.0, 2 * tail)


BYPASS_TOOLS = {"bash", "grep", "write", "edit", "apply_patch", "find_files", "glob"}


def scan_tools(jsonl_path):
    """(re-reads, used a bypass tool) for one run's stream-json output.

    A re-read is a `read` of a path the run had already read. A bypass tool is
    any tool that can extract the answer without holding it in context (a
    shell loop, a grep, a scratch file); a run that used one did not rely on
    the fold alone, whether or not it passed.
    """
    seen, dup, bypass = set(), 0, False
    try:
        with open(jsonl_path) as fh:
            for line in fh:
                try:
                    ev = json.loads(line)
                except ValueError:
                    continue
                if ev.get("type") != "assistant":
                    continue
                for blk in ev.get("message", {}).get("content", []) or []:
                    if blk.get("type") != "tool_use":
                        continue
                    if blk.get("name") in BYPASS_TOOLS:
                        bypass = True
                    if blk.get("name") != "read":
                        continue
                    inp = blk.get("input") or {}
                    p = os.path.basename(str(inp.get("path") or inp.get("file_path") or ""))
                    if p in seen:
                        dup += 1
                    seen.add(p)
    except OSError:
        return (None, None)
    return (dup, bypass)


def halted(trace_path):
    """True when the loop itself stopped the run on context pressure."""
    try:
        with open(trace_path) as fh:
            for line in fh:
                if '"system_notice"' in line and "Run stopped: the context" in line:
                    return True
    except OSError:
        return None
    return False


def load(workdirs, pool):
    runs = []
    for w in workdirs:
        path = os.path.join(w, "results.tsv")
        with open(path) as fh:
            for line in fh:
                f = line.rstrip("\n").split("\t")
                if len(f) < 32:
                    sys.exit(f"{path}: row has {len(f)} columns, need 32 "
                             "(written by a loop-ab.sh without output tokens)")
                tag, model, rep = f[C_TAG], f[C_MODEL], int(f[C_REPEAT])
                runs.append({
                    "work": w, "arm": pool or tag, "split": tag, "model": model,
                    "repeat": rep,
                    "turns": int(f[C_TURNS] or 0),
                    "correct": int(f[C_CORRECT] or 0),
                    "in": int(f[C_IN] or 0), "cached": int(f[C_CACHED] or 0),
                    "out": int(f[C_OUT] or 0),
                    "session": int(f[C_SESS] or 0),
                    "folds": int(f[C_COMPACT] or 0),
                })
                runs[-1]["halted"] = halted(
                    os.path.join(w, f"{tag}-{model}-{rep}.trace.jsonl"))
                runs[-1]["rereads"], runs[-1]["bypass"] = scan_tools(
                    os.path.join(w, f"{tag}-{model}-{rep}.jsonl"))
    return runs


def cost(r, a):
    uncached = max(0, r["in"] - r["cached"])
    return (uncached * a.price_in + r["cached"] * a.price_cached
            + r["out"] * a.price_out) / 1e6


def summarize(runs, a):
    n = len(runs)
    k = sum(r["correct"] for r in runs)
    lo, hi = wilson(k, n)
    costs = [cost(r, a) for r in runs]
    total = sum(costs)
    rr = [r["rereads"] for r in runs if r["rereads"] is not None]
    return {
        "n": n,
        "folded_runs": sum(1 for r in runs if r["folds"] > 0),
        "missing_session": sum(1 for r in runs if not r["session"]),
        "halted_runs": sum(1 for r in runs if r["halted"]),
        "fail_not_halted": sum(1 for r in runs if not r["correct"] and not r["halted"]),
        "bypass_runs": sum(1 for r in runs if r["bypass"]),
        "passes_without_bypass": sum(1 for r in runs if r["correct"] and not r["bypass"]),
        "passes": k, "pass_rate": k / n if n else float("nan"),
        "pass_ci95": [lo, hi],
        "input_tokens": mean_ci([r["in"] for r in runs]),
        "cached_tokens": mean_ci([r["cached"] for r in runs]),
        "output_tokens": mean_ci([r["out"] for r in runs]),
        "turns": mean_ci([r["turns"] for r in runs]),
        "folds_per_run": mean_ci([r["folds"] for r in runs]),
        "rereads_per_run": mean_ci(rr),
        "cost_per_run_usd": mean_ci(costs),
        "total_cost_usd": total,
        "cost_per_pass_usd": (total / k) if k else None,
    }


def fmt_ci(t, nd=0):
    m, lo, hi = t
    return f"{m:.{nd}f} [{lo:.{nd}f}, {hi:.{nd}f}]"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("workdirs", nargs="+")
    ap.add_argument("--price-in", type=float, required=True)
    ap.add_argument("--price-cached", type=float, required=True)
    ap.add_argument("--price-out", type=float, required=True)
    ap.add_argument("--pool")
    ap.add_argument("--compare", nargs=2, metavar=("ARM_A", "ARM_B"))
    ap.add_argument("--json")
    a = ap.parse_args()

    runs = load(a.workdirs, a.pool)
    by_arm = defaultdict(list)
    for r in runs:
        by_arm[r["arm"]].append(r)
    report = {"arms": {}}
    for arm, rs in by_arm.items():
        s = summarize(rs, a)
        report["arms"][arm] = s
        lo, hi = s["pass_ci95"]
        cpp = f"${s['cost_per_pass_usd']:.4f}" if s["cost_per_pass_usd"] else "n/a (0 passes)"
        print(f"== arm {arm}: n={s['n']} folded={s['folded_runs']}/{s['n']}"
              f" missing_session={s['missing_session']}")
        print(f"  pass           {s['passes']}/{s['n']} = {s['pass_rate']:.2f}"
              f"  Wilson95 [{lo:.2f}, {hi:.2f}]")
        print(f"  halted by ladder {s['halted_runs']}/{s['n']}"
              f"  (other failures {s['fail_not_halted']})")
        print(f"  bypass runs    {s['bypass_runs']}/{s['n']}"
              f"  (passes without bypass {s['passes_without_bypass']})")
        print(f"  input tokens   {fmt_ci(s['input_tokens'])}")
        print(f"  cached tokens  {fmt_ci(s['cached_tokens'])}")
        print(f"  output tokens  {fmt_ci(s['output_tokens'])}")
        print(f"  turns          {fmt_ci(s['turns'], 1)}")
        print(f"  folds/run      {fmt_ci(s['folds_per_run'], 2)}")
        print(f"  re-reads/run   {fmt_ci(s['rereads_per_run'], 2)}")
        print(f"  cost/run       {fmt_ci(s['cost_per_run_usd'], 4)} USD (token-priced)")
        print(f"  total          ${s['total_cost_usd']:.4f}   cost/pass {cpp}")

    if a.compare:
        key = "split" if a.pool else "arm"
        pa = {(r["work"], r["repeat"]): r for r in runs if r[key] == a.compare[0]}
        pb = {(r["work"], r["repeat"]): r for r in runs if r[key] == a.compare[1]}
        pairs = [(pa[k], pb[k]) for k in sorted(pa) if k in pb]
        b = sum(1 for x, y in pairs if x["correct"] and not y["correct"])
        c = sum(1 for x, y in pairs if y["correct"] and not x["correct"])
        dtok = mean_ci([y["in"] - x["in"] for x, y in pairs])
        p = sign_test(b, c)
        report["compare"] = {"a": a.compare[0], "b": a.compare[1], "pairs": len(pairs),
                             "a_only_pass": b, "b_only_pass": c, "sign_test_p": p,
                             "input_token_delta_b_minus_a": dtok}
        print(f"== paired {a.compare[1]} vs {a.compare[0]}: {len(pairs)} pairs")
        print(f"  discordant: {a.compare[0]}-only pass {b}, {a.compare[1]}-only pass {c}"
              f"  exact sign test p={p:.3f}")
        print(f"  input-token delta ({a.compare[1]} - {a.compare[0]}) {fmt_ci(dtok)}")

    if a.json:
        with open(a.json, "w") as fh:
            json.dump(report, fh, indent=2)


if __name__ == "__main__":
    main()
