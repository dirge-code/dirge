# `Reporting.dfy` — run reporting (module `Reporting`)

Color-aware, verbosity-gated print helpers used by the run methods in
[`DafnyCheck.dfy`](DafnyCheck.md) and [`Stateful.dfy`](Stateful.md). You normally don't call these
directly — they're the shared output layer behind `RunTest*`/`RunModelTest*` — but they're public
if you build a custom runner. Each takes `useColor`/`verbosity` (see [`RunConfig.md`](RunConfig.md))
rather than the generic config so reporting is independent of the input type.

```dafny
function Colorize(text: string, color: string, useColor: bool): string   // wrap in ANSI color when enabled

method ReportSuccess(name: string, valid: nat, useColor: bool, verbosity: Verbosity)
method ReportFailure(name: string, useColor: bool, verbosity: Verbosity)
method ReportUnsatisfiable(name: string, useColor: bool, verbosity: Verbosity)
method ReportCounterExample<T>(value: T, choices: seq<bv64>, useColor: bool, verbosity: Verbosity)
method ReportFailingExample<T>(value: T, useColor: bool, verbosity: Verbosity)
method ReportShrink(before: seq<bv64>, after: seq<bv64>, useColor: bool, verbosity: Verbosity)
method ReportStatistics(name: string, stats: map<string, nat>, useColor: bool, verbosity: Verbosity)
```

Verbosity gating: nothing prints at `Off`; pass/fail/counterexample print from `Low`; statistics
print from `Medium`; shrink steps print at `High`. Colors come from
[`colors.dfy`](colors.md) (`ConsoleColors`).
