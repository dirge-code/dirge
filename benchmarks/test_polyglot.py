"""Tests for the parts of the Polyglot harness that can be wrong silently.

The scoring logic is where a benchmark harness fails dangerously: a skip-marker
transform that stops matching, or a runner that reads a zero exit as a pass,
produces a *number* rather than an error, and a wrong number is worse than none.
Those paths are pinned here so they can be run without a model or a toolchain:

    python3 -m pytest benchmarks/test_polyglot.py
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from polyglot import EXCLUDED_DIRS, find_exercises, read_instructions, stage_exercise
from runners import LANGUAGES, failure_excerpt, strip_skip_markers, suite_passed


# ── skip-marker transforms ──────────────────────────────────────────────
# Each case is a real snippet shape from the Exercism templates. If a
# template changes and a pattern stops matching, the harness would silently
# score a one-assertion pass as a solved exercise.


def test_python_skip_markers_are_stripped():
    src = (
        "def test_one():\n    assert f(1) == 1\n\n"
        "@pytest.mark.skip('remove this to run')\n"
        "def test_two():\n    assert f(2) == 2\n"
    )
    out = strip_skip_markers("python", src)
    assert "@pytest.mark.skip" not in out
    assert "def test_two():" in out, "the test itself must survive"


def test_python_unittest_skip_is_stripped():
    src = "    @unittest.skip('later')\n    def test_two(self):\n        pass\n"
    assert "@unittest.skip" not in strip_skip_markers("python", src)


def test_javascript_skip_forms_are_all_enabled():
    src = "xit('adds', () => {});\nit.skip('subs', () => {});\nxdescribe('g', () => {});\n"
    out = strip_skip_markers("javascript", src)
    assert "xit(" not in out and "it.skip(" not in out and "xdescribe(" not in out
    assert out.count("it(") == 2 and "describe(" in out


def test_java_disabled_markers_are_stripped():
    src = "    @Disabled(\"Remove to run\")\n    @Test\n    public void two() {}\n"
    out = strip_skip_markers("java", src)
    assert "@Disabled" not in out
    assert "@Test" in out and "public void two()" in out


def test_go_and_cpp_pass_through_unchanged():
    src = "func TestOne(t *testing.T) {}\n"
    assert strip_skip_markers("go", src) == src
    assert strip_skip_markers("cpp", src) == src


def test_unknown_language_is_a_no_op_not_a_crash():
    assert strip_skip_markers("haskell", "anything") == "anything"


def test_rust_relies_on_include_ignored_rather_than_stripping():
    # Deliberate: `--include-ignored` is what upstream does, and rewriting the
    # attribute instead would make the two harnesses disagree.
    assert strip_skip_markers("rust", "#[ignore]\nfn t() {}") == "#[ignore]\nfn t() {}"
    assert "--include-ignored" in LANGUAGES["rust"].test_command


# ── pass/fail classification ────────────────────────────────────────────


def test_nonzero_exit_is_always_a_failure():
    assert not suite_passed("python", 1, "", "")
    assert not suite_passed("go", 2, "ok", "")


def test_zero_exit_is_a_pass():
    for language in LANGUAGES:
        assert suite_passed(language, 0, "all good", "")


def test_npm_missing_script_is_not_a_pass():
    # `npm test` exits 0 when the script is absent — every JS exercise would
    # score as solved without this.
    assert not suite_passed("javascript", 0, "npm ERR! missing script: test", "")


def test_gradle_build_failed_is_not_a_pass():
    assert not suite_passed("java", 0, "BUILD FAILED in 2s", "")


def test_the_lying_exit_checks_are_language_scoped():
    # The same text under another language must not be misread.
    assert suite_passed("python", 0, "the string BUILD FAILED appears in output", "")


# ── failure excerpt ─────────────────────────────────────────────────────


def test_excerpt_keeps_the_tail_where_the_failure_is():
    stdout = "setup noise\n" * 5000 + "AssertionError: expected 3 got 4"
    excerpt = failure_excerpt(stdout, "", limit=200)
    assert "AssertionError: expected 3 got 4" in excerpt
    assert len(excerpt) < 400
    assert "truncated" in excerpt


def test_short_output_is_returned_whole_without_a_marker():
    assert failure_excerpt("boom", "") == "boom"


# ── staging ─────────────────────────────────────────────────────────────


def _make_exercise(root: Path) -> Path:
    exercise = root / "python" / "exercises" / "practice" / "bob"
    (exercise / ".docs").mkdir(parents=True)
    (exercise / ".meta").mkdir()
    (exercise / ".docs" / "instructions.md").write_text("Answer Bob.")
    (exercise / ".docs" / "instructions.append.md").write_text("Edge case: silence.")
    (exercise / "bob.py").write_text("def hey(x):\n    pass\n")
    (exercise / "bob_test.py").write_text(
        "def test_one():\n    pass\n\n@pytest.mark.skip('x')\ndef test_two():\n    pass\n"
    )
    (exercise / ".meta" / "example.py").write_text("# the answer")
    return exercise


def test_staging_never_copies_the_reference_solution(tmp_path):
    """The single worst bug this harness could have."""
    exercise = _make_exercise(tmp_path / "src")
    dest = tmp_path / "work"
    stage_exercise(exercise, dest, "python")
    for excluded in EXCLUDED_DIRS:
        assert not (dest / excluded).exists(), f"{excluded} leaked into the workdir"
    assert not list(dest.rglob("example.py"))


def test_staging_strips_skip_markers_in_the_copy_only(tmp_path):
    exercise = _make_exercise(tmp_path / "src")
    dest = tmp_path / "work"
    stage_exercise(exercise, dest, "python")
    assert "@pytest.mark.skip" not in (dest / "bob_test.py").read_text()
    assert "@pytest.mark.skip" in (exercise / "bob_test.py").read_text(), (
        "the source tree must never be mutated — a rerun would score differently"
    )


def test_instructions_include_the_append_file(tmp_path):
    """The append file carries the edge cases the tests assert."""
    exercise = _make_exercise(tmp_path / "src")
    text = read_instructions(exercise)
    assert "Answer Bob." in text
    assert "Edge case: silence." in text


def test_find_exercises_walks_the_upstream_layout(tmp_path):
    _make_exercise(tmp_path)
    found = find_exercises(tmp_path, ["python", "go"])
    assert [(lang, path.name) for lang, path in found] == [("python", "bob")]


def test_every_language_has_a_complete_spec():
    for name, spec in LANGUAGES.items():
        assert spec.name == name
        assert spec.solution_globs and spec.test_globs and spec.test_command


# ── the driver, end to end ──────────────────────────────────────────────
# Driven against a stub `dirge` so the whole loop — argv construction,
# envelope parsing, test restore, retry, scoring — is exercised without a
# model or a network. A harness bug here would silently mis-score a real run.


def _stub_dirge(path: Path, body: str) -> Path:
    """A fake dirge that writes a solution and emits a result envelope."""
    script = path / "fake-dirge"
    script.write_text(
        "#!/usr/bin/env python3\n"
        "import json, pathlib, sys\n"
        f"{body}\n"
        "print(json.dumps({'type': 'result', 'subtype': 'success',\n"
        "                  'is_error': False, 'num_turns': 3,\n"
        "                  'total_cost_usd': 0.01, 'result': 'done'}))\n"
    )
    script.chmod(0o755)
    return script


def _args(tmp_path: Path, dirge_bin: Path, attempts: int = 2):
    import argparse

    return argparse.Namespace(
        dirge_bin=str(dirge_bin), model=None, attempts=attempts,
        max_turns=10, agent_timeout=60, test_timeout=60,
    )


def test_driver_scores_a_solved_exercise(tmp_path):
    from polyglot import run_exercise

    exercise = _make_exercise(tmp_path / "src")
    (exercise / "bob_test.py").write_text(
        "from bob import hey\n\ndef test_one():\n    assert hey('hi') == 'Whatever.'\n"
    )
    dirge = _stub_dirge(
        tmp_path,
        "pathlib.Path('bob.py').write_text(\"def hey(x):\\n    return 'Whatever.'\\n\")",
    )
    result = run_exercise("python", exercise, _args(tmp_path, dirge))
    assert result.passed and result.attempts == 1
    assert result.turns == [3] and result.cost_usd == 0.01


def test_driver_scores_an_unsolved_exercise_after_every_attempt(tmp_path):
    from polyglot import run_exercise

    exercise = _make_exercise(tmp_path / "src")
    (exercise / "bob_test.py").write_text(
        "from bob import hey\n\ndef test_one():\n    assert hey('hi') == 'Whatever.'\n"
    )
    dirge = _stub_dirge(tmp_path, "pathlib.Path('bob.py').write_text('def hey(x):\\n    return 1\\n')")
    result = run_exercise("python", exercise, _args(tmp_path, dirge, attempts=2))
    assert not result.passed
    assert result.attempts == 2, "both attempts must be spent before scoring a miss"
    assert result.turns == [3, 3]


def test_an_agent_that_edits_the_tests_cannot_score_a_pass(tmp_path):
    """The prompt forbids it; forbidding is not preventing."""
    from polyglot import run_exercise

    exercise = _make_exercise(tmp_path / "src")
    (exercise / "bob_test.py").write_text(
        "from bob import hey\n\ndef test_one():\n    assert hey('hi') == 'Whatever.'\n"
    )
    dirge = _stub_dirge(
        tmp_path,
        "pathlib.Path('bob_test.py').write_text('def test_one():\\n    pass\\n')",
    )
    result = run_exercise("python", exercise, _args(tmp_path, dirge, attempts=1))
    assert not result.passed, "the original tests must be restored before scoring"


def test_a_crashed_agent_is_a_miss_not_a_harness_failure(tmp_path):
    from polyglot import run_exercise

    exercise = _make_exercise(tmp_path / "src")
    (exercise / "bob_test.py").write_text("def test_one():\n    assert False\n")
    dirge = tmp_path / "broken-dirge"
    dirge.write_text("#!/usr/bin/env python3\nimport sys\nsys.exit(3)\n")
    dirge.chmod(0o755)
    result = run_exercise("python", exercise, _args(tmp_path, dirge, attempts=1))
    assert not result.passed
    assert result.turns == [0], "a crash contributes no turns"


def test_a_missing_toolchain_is_reported_not_scored_as_a_miss(tmp_path):
    """A benchmark that reports a plausible wrong number is worse than one
    that reports nothing. `go` absent must not read as 'the model failed'."""
    from polyglot import run_tests

    outcome = run_tests(tmp_path, "go", timeout=10)
    if outcome.harness_error is None:
        import shutil as _shutil

        assert _shutil.which("go"), "go resolved but no harness error was raised"
        return
    assert not outcome.passed
    assert "toolchain missing" in outcome.harness_error


def test_a_harness_error_stops_the_retry_loop(tmp_path):
    """Retrying can't fix a suite that never ran, and spending a second model
    call on it would burn budget and inflate the turn stats."""
    from polyglot import run_exercise

    exercise = _make_exercise(tmp_path / "src")
    dirge = _stub_dirge(tmp_path, "pass")
    args = _args(tmp_path, dirge, attempts=2)
    args.test_timeout = 10

    import polyglot

    original = polyglot.run_tests
    polyglot.run_tests = lambda *a, **k: polyglot.TestOutcome(False, "", "toolchain missing: x")
    try:
        result = run_exercise("python", exercise, args)
    finally:
        polyglot.run_tests = original

    assert not result.passed
    assert result.attempts == 1, "the second attempt must not be spent"
    assert result.error == "toolchain missing: x"


def test_a_relative_dirge_bin_is_pinned_before_the_run(tmp_path, monkeypatch):
    """The agent runs with cwd set to the exercise workdir, so a relative
    `--dirge-bin` — the form the README suggests — would resolve elsewhere."""
    import polyglot

    exercises = tmp_path / "poly"
    _make_exercise(exercises)
    binary = tmp_path / "rel" / "dirge"
    binary.parent.mkdir()
    binary.write_text("#!/bin/sh\nexit 0\n")
    binary.chmod(0o755)

    seen = {}

    def fake_run_exercise(language, exercise, args):
        seen["bin"] = args.dirge_bin
        return polyglot.ExerciseResult(language, exercise.name, True, 1, 0.1, [1], 0.0)

    monkeypatch.setattr(polyglot, "run_exercise", fake_run_exercise)
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(
        "sys.argv",
        ["polyglot.py", "--exercises", str(exercises), "--dirge-bin", "rel/dirge",
         "--out", str(tmp_path / "out.json")],
    )
    polyglot.main()
    assert seen["bin"] == str(binary), "relative binary path was not pinned"


def test_an_unwritable_out_path_fails_before_any_model_call(tmp_path, monkeypatch):
    """A run is hours long and writes only to --out. Finding out it's
    unwritable after the first exercise loses everything after it too."""
    import polyglot
    import pytest

    exercises = tmp_path / "poly"
    _make_exercise(exercises)
    called = []
    monkeypatch.setattr(polyglot, "run_exercise", lambda *a: called.append(1))
    monkeypatch.setattr(
        "sys.argv",
        ["polyglot.py", "--exercises", str(exercises),
         "--out", str(tmp_path / "no" / "such" / "dir" / "out.json")],
    )
    with pytest.raises(SystemExit):
        polyglot.main()
    assert not called, "the run started despite an unwritable --out"
