# lev Runbook

Running lev as a sidecar and wiring it to dirge's vigil rite gate.

## 1. Run lev

lev is a Clojure HTTP service run with jolt. From the lev checkout:

```bash
jolt -M:serve
```

Defaults (no config): binds `127.0.0.1:8080`, requires the API key `s3cret`.

To change the host, port, or key, pass CLI flags or set the config file. lev
resolves configuration in this order (first match wins):

1. CLI flags
2. environment variables
3. `config.edn`
4. built-in defaults

The config dir is looked up at `$LEV_CONFIG_DIR`, then
`$XDG_CONFIG_HOME/lev`, then `~/.config/lev`.

Sanity-check the server:

```bash
curl -s http://127.0.0.1:8080/v1/models \
  -H 'Authorization: Bearer s3cret'
```

The endpoint dirge uses is `POST /v1/systemone`:

```bash
curl -s http://127.0.0.1:8080/v1/systemone \
  -H 'Authorization: Bearer s3cret' \
  -H 'Content-Type: application/json' \
  -d '{"state":{"job":"ci-watch"},"questions":{"judgment":{"type":"noul","instructions":"The current state requires agent intervention."}}}'
```

A successful response includes `answers.judgment.noul` — a probability between
0 and 1. The lev plugin reads exactly that field.

## 2. Wire it to dirge

Add the plugin to your dirge plugin config (or drop it in the plugins dir), and
set the environment variables:

```bash
export LEV_URL="http://127.0.0.1:8080"
export LEV_API_KEY="s3cret"
export LEV_THRESHOLD="0.8"   # block when lev's noul is below this
dirge --vigil
```

No vigil config change is required: `on-vigil-rite` fires for every vigil whose
plugin manager has the hook registered. If you want the shell `rite` gate to run
first, keep it configured — the lev gate runs after it, independently.

## 3. Verify the gate

With lev running and the plugin loaded, start a vigil and trigger it:

```bash
dirge --vigil
# in the TUI: /vigil status
```

Watch the log for the gate verdict:

- `on-vigil-rite gate blocked, skipping observance` with a reason — lev judged
  intervention unnecessary, so no agent turn fired.
- no gate line, but the observance fires — lev passed the event through.

If lev is unreachable, the gate fails open: dirge logs a warning and proceeds
with the observance rather than stalling the vigil.

## 4. Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| No gate line at all, observances still fire | Plugin not loaded, or `on-vigil-rite` not registered | Check the plugin loads and declares `(def hooks ["on-vigil-rite"])` |
| `on-vigil-rite gate timed out` | lev is slow or hung | Confirm lev responds to `curl`; raise lev resources |
| `responder dropped` warning | Hook drained but the oneshot sender was dropped | Check the drainer task is running (it is spawned in `main.rs` whenever the hook channel exists) |
| lev returns non-2xx | Wrong API key or bad URL | Verify `LEV_API_KEY` and `LEV_URL`; test with the `curl` above |
| `http-post` returns `nil` | Non-2xx, timeout, or network error | See above; confirm lev binds the address `LEV_URL` points at |

## 5. Testing the plugin without lev

`plugins/vigil_lev.janet`'s `lev-verdict` accepts an optional config table, so
the gate logic is testable against a mock server with no lev running:

```janet
(lev-verdict
  {:vigil "v" :trigger :toll :event_count 1 :payload "{}"}
  {:endpoint "http://127.0.0.1:PORT" :threshold 0.8})
```

The Rust test suite exercises this path (`lev_rite_gate_blocks_below_threshold`,
`lev_rite_gate_passes_above_threshold`) using a local mock HTTP server.
