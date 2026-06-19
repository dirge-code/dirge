# `colors.dfy` — ANSI colors (module `ConsoleColors`)

ANSI color escape constants and helpers used by [`Reporting.dfy`](Reporting.md). Reporting decides
*whether* to apply color (per `RunConfig.useColor`); this module just provides the codes.

```dafny
function makeColor(code: string): string                 // prefix a code with the ESC byte
function combineColors(fgCode: string, bgCode: string): string
function FgOnBg(fgCode: string, bgCode: string): string  // foreground-on-background combination
```

Foreground constants (bright): `BLACK`, `RED`, `GREEN`, `YELLOW`, `BLUE`, `MAGENTA`, `CYAN`,
`WHITE`; the same with a `_NORMAL` suffix for the non-bold variants. Background constants:
`BG_BLACK` … `BG_WHITE`. Reset: `NOCOLOR`.

```dafny
print GREEN, "PASS", NOCOLOR, "\n";
```
