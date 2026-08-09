"""Per-language specifics for the Aider Polyglot exercises.

Everything that differs between the six languages lives here, so the driver in
`polyglot.py` stays language-agnostic. Three things vary:

  * which files are the *solution* (the agent edits these) versus the *tests*
    (it must not);
  * how to run the test suite, and what a pass looks like;
  * which skip markers the exercise ships with. Exercism templates disable all
    but the first test so a human can work through them one at a time. Left in
    place, an agent "passes" by satisfying one assertion, so they are stripped
    before scoring. This mirrors the transforms the upstream Aider harness
    applies (`xit` -> `it`, `@Disabled`, `--include-ignored`).

Pure and dependency-free by design: the transforms and command construction are
unit-tested in `test_polyglot.py` without a model, a network, or a toolchain.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field


@dataclass(frozen=True)
class Language:
    name: str
    # Glob patterns, relative to the exercise root.
    solution_globs: tuple[str, ...]
    test_globs: tuple[str, ...]
    # argv for the test command, run with cwd = exercise root.
    test_command: tuple[str, ...]
    # Extra environment for the test run.
    test_env: dict[str, str] = field(default_factory=dict)


LANGUAGES: dict[str, Language] = {
    "python": Language(
        name="python",
        solution_globs=("*.py",),
        test_globs=("*_test.py",),
        # `python3`, not `python`: on a stock macOS / many Linux distros
        # there is no bare `python`, and the resulting FileNotFoundError
        # would score every exercise as a miss.
        test_command=("python3", "-m", "pytest", "-x", "-q"),
    ),
    "go": Language(
        name="go",
        solution_globs=("*.go",),
        test_globs=("*_test.go",),
        test_command=("go", "test", "./..."),
    ),
    "rust": Language(
        name="rust",
        solution_globs=("src/*.rs",),
        test_globs=("tests/*.rs",),
        # Exercism marks all but the first test `#[ignore]`; running them is
        # the Rust equivalent of stripping a skip marker.
        test_command=("cargo", "test", "--", "--include-ignored"),
    ),
    "javascript": Language(
        name="javascript",
        solution_globs=("*.js",),
        test_globs=("*.spec.js",),
        test_command=("npm", "test", "--", "--no-color"),
    ),
    "cpp": Language(
        name="cpp",
        solution_globs=("*.cpp", "*.h"),
        test_globs=("*_test.cpp",),
        test_command=("bash", "-lc", "cmake -S . -B build && cmake --build build"),
        # Exercism's CMakeLists gates the full suite behind this.
        test_env={"EXERCISM_RUN_ALL_TESTS": "1"},
    ),
    "java": Language(
        name="java",
        solution_globs=("src/main/java/*.java",),
        test_globs=("src/test/java/*.java",),
        test_command=("./gradlew", "test", "--no-daemon", "--console=plain"),
    ),
}


# Per-language skip-marker transforms, applied to TEST files only.
#
# Each entry is (pattern, replacement) applied with `re.sub`. Kept as data so
# `test_polyglot.py` can assert each one against a real template snippet.
SKIP_TRANSFORMS: dict[str, tuple[tuple[str, str], ...]] = {
    "python": (
        # `@pytest.mark.skip(...)` / `@unittest.skip("...")`, whole line.
        (r"(?m)^[ \t]*@(?:pytest\.mark\.skip|unittest\.skip)\b.*\n", ""),
    ),
    "javascript": (
        # `xit(` -> `it(`, `xdescribe(` -> `describe(`, `.skip(` -> `(`.
        (r"\bxit\s*\(", "it("),
        (r"\bxdescribe\s*\(", "describe("),
        (r"\b(it|describe|test)\.skip\s*\(", r"\1("),
    ),
    "java": (
        (r"(?m)^[ \t]*@Disabled\b.*\n", ""),
        (r"(?m)^[ \t]*@Ignore\b.*\n", ""),
    ),
    "rust": (
        # Handled by `--include-ignored` on the command line; stripping the
        # attribute too would diverge from the upstream harness for no gain.
    ),
    "go": (),
    "cpp": (),
}


def strip_skip_markers(language: str, source: str) -> str:
    """Return `source` with the language's skip markers removed.

    Unknown languages and languages with no markers return the input
    unchanged — this must never be a silent partial transform.
    """
    out = source
    for pattern, replacement in SKIP_TRANSFORMS.get(language, ()):
        out = re.sub(pattern, replacement, out)
    return out


def suite_passed(language: str, returncode: int, stdout: str, stderr: str) -> bool:
    """Whether the suite passed.

    Exit status is the signal for every language here. The output is still
    taken because two runners lie with a zero exit:

      * `npm test` exits 0 when the *script* is missing, and
      * a gradle build that fails to compile can exit 0 under some wrappers.

    Both print an unmistakable marker, so they are checked explicitly rather
    than trusted.
    """
    if returncode != 0:
        return False
    combined = f"{stdout}\n{stderr}"
    if language == "javascript" and "missing script" in combined:
        return False
    if language == "java" and "BUILD FAILED" in combined:
        return False
    return True


def failure_excerpt(stdout: str, stderr: str, limit: int = 6000) -> str:
    """The tail of a failing test run, for the second attempt's prompt.

    The tail, not the head: compilers and test runners put the summary and the
    first real failure at the end, while the head is setup noise.
    """
    combined = (stdout + "\n" + stderr).strip()
    if len(combined) <= limit:
        return combined
    return "…(truncated)…\n" + combined[-limit:]
