# Cutting a release

Six distribution channels. Pushing a `v*` tag drives all of them; nothing else
is manual.

| Channel | Job in `release.yml` | Secret |
|---|---|---|
| GitHub release + binaries (5 targets, SBOM, attestations) | `sbom`, `build` | — |
| `nix/bin.nix` on `main` | `nix-binary-package` | — |
| Homebrew tap (`dirge-code/homebrew-dirge`) | `homebrew` | `HOMEBREW_TAP_TOKEN` |
| crates.io (`dirge-agent`) | `crates-io` | `CARGO_REGISTRY_TOKEN` |
| Site version (`dirge-code.github.io`) | `site` | `SITE_REPO_TOKEN` |
| All of the above, read back | `verify-channels` | — |

## Releasing

1. Bump `version` in `Cargo.toml`, commit, push to `main`.
2. Tag and push:

   ```
   git tag v0.21.2
   git push origin v0.21.2
   ```

3. Watch the run. `verify-channels` is the one that matters — it reads every
   channel back from its published source and fails if any of them disagrees
   with the tag.

That's it. The tag must match `Cargo.toml`; `crates-io` checks this before
publishing anything, because a mismatch would put the wrong version on the
registry permanently.

## Why verify-channels exists

v0.20.0 shipped to 4 of 6 channels and nobody noticed until v0.21.0 was cut.
`cargo install dirge-agent` went straight from 0.19.29 to 0.21.0, and the site
still read v0.19.29 — it had missed 0.20.0 as well.

The split was clean: every automated channel fired, both manual channels were
skipped. So the fix was to automate the remaining two, not to write a longer
checklist — the checklist existing is not what failed.

But automating them only converts "someone forgot" into "a job silently
no-opped": both the homebrew and site steps rewrite markup with `sed`, and an
unmatched `sed` exits 0. `verify-channels` closes that by checking published
state rather than job status — the registry index, the tap's formula, the site's
`brand-ver` span, `nix/bin.nix` on `main`, and every expected release asset with
its checksum. A channel that didn't ship is a red X on the release run.

(dirge-a9pc)

## Secrets

Repo secrets on `dirge-code/dirge`:

- `CARGO_REGISTRY_TOKEN` — crates.io API token scoped to publish-update on
  `dirge-agent`. From crates.io → Account Settings → API Tokens.
- `HOMEBREW_TAP_TOKEN` — PAT with `contents:write` on
  `dirge-code/homebrew-dirge`. The default `GITHUB_TOKEN` can't push to
  another repository.
- `SITE_REPO_TOKEN` — same, for `dirge-code/dirge-code.github.io`.

A missing secret fails its job loudly; the binary release is unaffected.

## Notes

- The crate is `dirge-agent` (the short `dirge` was taken); the installed
  binary is still `dirge`.
- Publishing is permanent. A version can be yanked but never replaced or
  reused, which is why `crates-io` gates on `build` — a tag that doesn't
  compile on all five targets shouldn't become an immovable registry entry.
- `crates-io` is idempotent: re-running a completed release sees the version
  already on the registry and exits clean.
- crates.io installs get default features only (`loop`, `git-worktree`, `mcp`,
  `lsp`). The published crate carries every feature definition regardless —
  `cargo install dirge-agent --features "semantic,semantic-ts"` works; the
  README lists the combos.

### Publishing by hand

Only needed if the workflow is broken.

```
git switch main && git pull
cargo publish --dry-run    # packages + builds in isolation
cargo publish
```

`--dry-run` complaining about a dirty tree from `.dirge/skills/.usage.json` is
harmless local state — stash it or pass `--allow-dirty`; the file isn't in the
package. To keep the token out of shell history:
`read -s CARGO_REGISTRY_TOKEN && export CARGO_REGISTRY_TOKEN && cargo publish`.
