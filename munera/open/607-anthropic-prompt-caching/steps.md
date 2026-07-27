# 607 — Steps

- [ ] Verify `with_automatic_caching_1h` exists on rig 0.39.0 CompletionModel
- [ ] Add `.with_automatic_caching_1h()` to `AnthropicOauth` arm in `stream_dispatch.rs`
- [ ] Add `.with_automatic_caching_1h()` to `Anthropic` arm in `stream_dispatch.rs`
- [ ] Add `prompt-caching-scope-2026-01-05` to `ANTHROPIC_OAUTH_BETA` in `anthropic_http.rs`
- [ ] `cargo build` — confirm compiles clean
- [ ] `cargo test -p dirge` — confirm existing tests pass
- [ ] Commit
