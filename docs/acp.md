# ACP (Agent Client Protocol)

Built with the `acp` feature, `dirge --acp` runs as an ACP agent over stdio,
so an editor can spawn it as a subprocess. Configuration is in
[config.md](config.md#acp-agent-communication-protocol-configuration).

## Usage and cost of a prompt

The response to `session/prompt` reports what the prompt used in
`_meta.usage`:

```json
{
  "stopReason": "end_turn",
  "_meta": {
    "usage": {
      "inputTokens": 9500,
      "outputTokens": 30,
      "totalTokens": 9530,
      "cachedReadTokens": 9000,
      "cachedWriteTokens": 400,
      "costUsd": 0.0041
    }
  }
}
```

- The counts are summed over every model call the prompt made. A prompt
  whose reply calls tools makes more than one.
- `inputTokens` is the whole prompt sent to the model. `cachedReadTokens`
  (served from the provider's prompt cache) and `cachedWriteTokens`
  (written to it) are parts of it, so do not add them to it. Providers count
  cached tokens in two ways, and dirge converts both to this one.
- `totalTokens` is `inputTokens + outputTokens`.
- `costUsd` is dirge's price for those tokens. It is `0` for a model whose
  price dirge does not know.
- `_meta` is absent when the provider reported no usage, and for the slash
  commands dirge answers without a model call.

ACP's own `usage` field on the prompt response is not stable yet. The ACP
Rust schema has it only behind the `unstable_end_turn_token_usage` feature.
dirge uses `_meta` instead, with that field's names, so a client can read
either one.
