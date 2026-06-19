include "./colors.dfy"
include "./RunConfig.dfy"
include "./utils.dfy"
include "./RandomGenerator.dfy"

// Unified, color-aware, verbosity-gated reporting for test runs. Every run
// method routes its output through these helpers so formatting stays
// consistent. Color is applied here (gated on `useColor`) because the
// ConsoleColors constants are unconditional. Helpers take `useColor` and
// `verbosity` directly (rather than the generic RunConfig<T>) so reporting is
// independent of the input type.
module Reporting {
  import opened ConsoleColors
  import opened RunConfigs
  import opened LTLUtils
  import opened RandomGenerator
  import opened Std.Wrappers

  // Wrap `text` in an ANSI color when enabled, otherwise return it unchanged.
  function Colorize(text: string, color: string, useColor: bool): string {
    if useColor then color + text + NOCOLOR else text
  }

  // Integer percentage count/total, guarding against division by zero.
  function Percent(count: nat, total: nat): nat {
    if total == 0 then 0 else (count * 100) / total
  }

  method ReportSuccess(name: string, valid: nat, useColor: bool, verbosity: Verbosity)
  {
    if verbosity != Off {
      print Colorize("[" + name + "] PASS", GREEN, useColor),
            " (", valid, " valid examples)\n";
    }
  }

  method ReportFailure(name: string, useColor: bool, verbosity: Verbosity)
  {
    if verbosity != Off {
      print Colorize("[" + name + "] FAIL", RED, useColor), "\n";
    }
  }

  method ReportUnsatisfiable(name: string, useColor: bool, verbosity: Verbosity)
  {
    if verbosity != Off {
      print Colorize("[" + name + "] UNSATISFIABLE", YELLOW, useColor),
            " - no valid test cases generated\n";
    }
  }

  // Report a generation counterexample: the failing input plus the minimised
  // choice sequence that reproduces it.
  method ReportCounterExample<T>(value: T, choices: seq<Choice>, useColor: bool, verbosity: Verbosity)
  {
    if verbosity != Off {
      print "  ", Colorize("counterexample", YELLOW, useColor), ": ", value, "\n";
      print "  minimised choices: ", choices, "\n";
    }
  }

  // Report a failing always-tested example (no shrinking — user supplied it).
  method ReportFailingExample<T>(value: T, useColor: bool, verbosity: Verbosity)
  {
    if verbosity != Off {
      print "  ", Colorize("failing example", YELLOW, useColor), ": ", value, "\n";
    }
  }

  // Trace one generated value and its choice sequence (High verbosity only).
  method ReportGenerated<T>(value: Option<T>, choices: seq<Choice>, valid: bool, useColor: bool, verbosity: Verbosity)
  {
    if AtLeast(verbosity, High) {
      print "  ", Colorize("gen", CYAN, useColor), if valid then " ok   " else " skip ", "choices=", choices;
      if value.Some? {
        print " value=", value.value;
      }
      print "\n";
    }
  }

  // Report a single accepted shrink step (High verbosity only).
  method ReportShrink(before: seq<Choice>, after: seq<Choice>, useColor: bool, verbosity: Verbosity)
  {
    if AtLeast(verbosity, High) {
      print "  ", Colorize("shrink", CYAN, useColor), ": ", before, " -> ", after, "\n";
    }
  }

  // Sum of all bucket counts in a classification map.
  function TotalCount(stats: map<string, nat>): nat {
    SumCounts(stats, SetToSequence(stats.Keys))
  }

  function SumCounts(stats: map<string, nat>, keys: seq<string>): nat {
    if |keys| == 0 then 0
    else (if keys[0] in stats then stats[keys[0]] else 0) + SumCounts(stats, keys[1..])
  }

  // Print classification statistics: one line per bucket as `label: P% (count)`,
  // ordered by label. Buckets come from a classifier applied to generated inputs.
  method ReportStatistics(name: string, stats: map<string, nat>, useColor: bool, verbosity: Verbosity)
  {
    if |stats| > 0 {  // log whenever a classifier produced buckets, at any verbosity
      var total := TotalCount(stats);
      print Colorize("[" + name + "] statistics", CYAN, useColor),
            " (", total, " classified):\n";
      var labels := SetToSequence(stats.Keys);
      var i := 0;
      while i < |labels|
        invariant 0 <= i <= |labels|
      {
        var lbl := labels[i];
        var count := if lbl in stats then stats[lbl] else 0;
        print "  ", lbl, ": ", Percent(count, total), "% (", count, ")\n";
        i := i + 1;
      }
    }
  }
}
