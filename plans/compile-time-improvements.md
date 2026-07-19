# Compile-time improvements

Notes on what we've done to keep dirge's build fast, plus measured data and
the options we've evaluated but not (yet) adopted. Referenced from `Cargo.toml`.

## Baseline (measured)

Machine: Apple M1, 10 cores, 64 GB RAM. `cargo build --bin dirge`, default
features, from `cargo clean`, after the `ring` switch (PR #690).

- Cold wall time: **~85s** (~275s user CPU, ~3.6× parallelism).
- Slowest compile units (from `cargo build --timings`):

  | Unit | Time |
  |------|-----:|
  | dirge-agent (our crate) | 28.6s |
  | rig-core | 21.0s |
  | objc2-foundation | 15.7s |
  | rmcp | 12.2s |
  | lsp-types | 7.2s |
  | tokio | 7.0s |
  | syn | 6.6s |
  | bindgen | 6.1s |
  | rustls | 5.6s |
  | ring | 4.9s |

The tail is now our own crate's codegen (28.6s) plus a handful of large
third-party crates that are cached after the first build. The old dominant
cost (`aws-lc-sys` at ~49s) is gone.

The number that actually governs day-to-day iteration is the **incremental
rebuild** after a one-file edit — everything is cached, only `dirge-agent`
recompiles and relinks. Measured here (touch `src/main.rs`, plugin-free):
**~5.5s**. That's already fast, and it's mostly link + a small codegen delta,
not something the full-build levers move.

## What's already applied

- **rustls on `ring`, not `aws-lc-rs`** (PR #690). aws-lc-sys compiled the full
  BoringSSL C tree (~49s); ring is ~4.9s. Cold-build CPU down ~59%.
- **Dropped `[profile.dev.build-override] opt-level = 3`** (PR #690). Building
  proc-macro/build deps at `-O3` made `bindgen` ~41s instead of ~6s; those
  crates are cached and rarely rebuilt, so the cost was never recouped.
- **Default-feature pruning**: `rig-core`, `reqwest`, `jsonschema` all pinned to
  the minimal feature set (see `Cargo.toml`). Dropped ~370 transitive crates.
- **mold linker** on Linux (`.cargo/config.toml`); macOS keeps the fast Xcode
  `ld` (mold's macOS port is discontinued).
- **`split-debuginfo = "unpacked"`** + **`debug = 0`** in `profile.dev` — skips
  `.dSYM` bundling and debuginfo generation on the dev iterate loop.
- **`cargo-machete` / `cargo-shear`** config to keep unused deps out.
- **CI dependency caching** via `actions-rust-lang/setup-rust-toolchain` (bundles
  `Swatinem/rust-cache`).

## Evaluated

Each subsection carries its own verdict (adopted in CI / opt-in / rejected).
The one opt-in accelerator we kept, sccache, is wrapped by
`scripts/fast-build.sh`. It is not the committed default — a plain `cargo build`,
the release build, and CI all stay on stable LLVM with no wrapper — and it only
helps *clean* builds, not the ~5.5s incremental edit loop.

### sccache — conditional, opt-in only (`scripts/fast-build.sh`)

Measured on this repo (M1): a clean rebuild with a **warm** sccache and
*unchanged source* drops from **85s → 52s (~39%)**, 100% Rust-crate hit rate.

But that is the clean-rebuild case. sccache does **not** help the normal
edit-one-file-and-rebuild loop — that's cargo's incremental compilation, and
sccache can't cache the incrementally-built local crate. So it only pays off
when `target/` is thrown away or unshared:

- frequent `cargo clean` / benchmarking from clean,
- switching branches that change `Cargo.lock` (invalidates dep artifacts),
- multiple checkouts/worktrees sharing dependency versions,
- CI runs whose `target/` cache missed.

Verdict: **don't** make it the committed default (`RUSTC_WRAPPER` in
`.cargo/config.toml`) — it adds a daemon and doesn't help the common loop. Use
`scripts/fast-build.sh` when the clean-rebuild cases above are frequent
(needs `brew install sccache`). Leave `CARGO_INCREMENTAL` alone: registry deps
are non-incremental and get cached by sccache, while our crate stays
incremental.

Matches corrode.dev's own conclusion: negligible for a single project's steady
state, worthwhile for build servers / multiple projects sharing deps.

### Cranelift + parallel frontend — evaluated, rejected

The nightly **Cranelift backend** + **parallel frontend** (`-Zthreads`) do faster
debug *codegen* than LLVM, so a full plugin-free build sped up (~59s vs LLVM's
~65-85s). But the number that matters — the incremental edit loop — barely moved:

| Plugin-free, touch `src/main.rs` | Cranelift | LLVM (stable) |
|----------------------------------|----------:|--------------:|
| Incremental rebuild              |    ~4.9s  |        ~5.5s  |

A one-file edit only re-codegens a small delta and relinks, so both are ~5s and
Cranelift's codegen speed has little to bite on. On top of that:

- **It can't link the `plugin` (Janet FFI) feature on macOS arm64** — the final
  link fails with a missing `computer_use_exec_body` symbol. It only builds
  plugin-free (e.g. `--no-default-features --features windows-default`), which
  excludes a real chunk of the codebase.
- Nightly-only, and the minimal nightly lacks `llvm-tools`, so `rust-objcopy`
  (our `split-debuginfo = "unpacked"`) prints "libLLVM.dylib not found" noise.
- The Homebrew `rust` formula on `PATH` shadows rustup's proxy, so
  `cargo +nightly` / `rustup run nightly` leave *rustc* on stable and the `-Z`
  flags get rejected; you have to point cargo at nightly's real rustc via
  `rustup which --toolchain nightly rustc`.

Verdict: **not adopted.** No gain on the loop that matters, can't build the
plugin feature, and drags in a nightly toolchain. If you ever want it for a
one-off big rebuild off the plugin code:

```sh
NIGHTLY_RUSTC=$(rustup which --toolchain nightly rustc)
env RUSTC="$NIGHTLY_RUSTC" RUSTFLAGS="-Zcodegen-backend=cranelift -Zthreads=8" \
  "$(rustup which --toolchain nightly cargo)" build --bin dirge \
  --no-default-features --features windows-default
```

### cargo-nextest — faster test runs (adopted in CI)

The suite is ~4400 tests. `cargo-nextest` runs them in parallel per-process,
faster than `cargo test` with clearer failures. Adopted in `ci.yml`'s test jobs
(`taiki-e/install-action@nextest` + `cargo nextest run --bin dirge`). dirge is a
bin-only crate (no lib), so `cargo test --bin` never ran doctests and the swap
drops no coverage. Locally: `cargo nextest run --bin dirge`.

### Not applicable / not worth it

- **Workspace split / cargo-hakari** — dirge is a single crate; high effort,
  architectural. Revisit only if the crate keeps growing.
- **RAM-disk `target/`** — 64 GB RAM already keeps `target/` hot in page cache;
  marginal and risky.
- **Adding back proc-macro `-O3`** — measured net-negative here (see above).
