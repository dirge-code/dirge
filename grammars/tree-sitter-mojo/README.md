# tree-sitter-mojo (vendored)

Generated parser for the Mojo grammar, compiled into `dirge-agent` by the
crate's `build.rs` under the `semantic-mojo` feature.

## Why it's vendored

`tree-sitter-mojo` is not published to crates.io under any name, so the only
way to depend on it is a git dependency — and cargo refuses to publish a crate
that has one ("all dependencies must have a version requirement specified when
publishing"). `dirge-agent` ships to crates.io on every release, so a git dep
would break the release job. Vendoring the generated parser keeps the crate
self-contained and publishable.

## Provenance

- Upstream: https://github.com/lsh/tree-sitter-mojo
- Taken from https://github.com/chriselrod/tree-sitter-mojo at `75a6faf`
  ("Remove dangling references to deleted queries/tags.scm"), which is upstream
  HEAD (`33193a9`) plus a one-line build fix — upstream's Rust binding has
  failed to compile since Dec 2025 because `lib.rs` `include_str!`s a
  `queries/tags.scm` that was deleted. Submitted upstream as
  lsh/tree-sitter-mojo#12.
- Grammar version 0.25.0, MIT licensed (see `LICENSE`).

## What's here

`src/parser.c`, `src/scanner.c` and `src/tree_sitter/*.h` are the build inputs.
`grammar.js` and `src/node-types.json` are carried for provenance and to make
the next regeneration diffable; nothing compiles them.

## Updating

Regenerate upstream with `tree-sitter generate`, then copy `src/parser.c`,
`src/scanner.c`, `src/tree_sitter/`, `grammar.js` and `src/node-types.json`
here and update the rev above. Re-run the grammar tests in
`src/semantic/syntax_validator.rs`
(`the_mojo_grammar_accepts_core_mojo_1_0`, `the_mojo_grammar_rejects_real_world_constructs`)
— the second one is the signal that the grammar has learned constructs it used
to reject, and that `.mojo`/`.🔥` may be ready to move off `GATE_EXCLUSIONS`.

Prefer switching back to a crates.io dependency if the grammar is ever
published there.
