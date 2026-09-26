# dirge wasm demo

`index.html` loads the `dirge-agent` wasm bundle in the browser and drives it the
same way the library path does:

- `token_count(text)` — llmtrim token estimator
- `chat(api_key, prompt)` — BYOK DeepSeek chat (key only goes to DeepSeek)
- `AgentHandle` — tool-calling agent loop with JS-registered tools
- `SessionStore` — pared-down in-memory session persistence
- Janet VM — a standalone, pared-down Janet interpreter (`wasm-janet/dist/`)

## Run it

The web bundle (`../pkg-web/`) is committed, so a checkout works as-is. Serve the
repo root and open `/demo/`:

```bash
python3 -m http.server 8000
# then open http://localhost:8000/demo/
```

A static file server is required: the browser loads the wasm via `fetch`, which
does not work over the `file://` scheme.

## Rebuild the web bundle

The demo imports `../pkg-web/dirge_agent.js`, the `wasm-pack --target web` output.
Regenerate it after changing the wasm surface:

```bash
wasm-pack build --target web --dev --no-opt --out-dir pkg-web -- --no-default-features --features wasm
```

## Rebuild the Janet bundle

The Janet VM is a standalone Emscripten build, not part of `dirge-agent`. See
`../wasm-janet/README.md` for the pinned-recipe rebuild.
