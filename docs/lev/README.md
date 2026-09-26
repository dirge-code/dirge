# lev — System One Rite Gate

`lev` ([jlt-commons/lev](https://github.com/jlt-commons/lev), Apache-2.0) is an
external decision engine that runs as a **sidecar** next to dirge. dirge talks to
it over HTTP; lev never links into the binary.

In the vigil feature line, lev plugs into the **rite gate**: before the reaper
dispatches an agent turn (an *observance*), a Janet plugin asks lev one yes/no
question — "does the current event state require agent intervention?" — and
blocks the observance when lev's confidence is low.

## Why a sidecar instead of a built-in gate?

- dirge's Rust core stays small: no decision-engine runtime is vendored.
- lev-specific logic (request shape, threshold, verdict) lives in a Janet
  plugin, which can be edited or replaced without recompiling dirge.
- lev runs out-of-process under the project owner's control, with its own
  config, auth, and lifecycle.

## Two layers

| Layer | Where | What |
|---|---|---|
| A — mechanism | `src/extras/vigil/` + `src/plugin/` | The `on-vigil-rite` hook, a synchronous oneshot channel, a dedicated hook drainer, and a fail-open gate |
| B — adapter | `plugins/vigil_lev.janet` | The lev plugin: builds the request, POSTs to lev, reads the `noul` probability, blocks or passes |

Layer A is generic — any plugin can register `on-vigil-rite` and gate an
observance, not just lev. Layer B is the lev-specific adapter.

## The `on-vigil-rite` hook

`on-vigil-rite` fires **after** the optional shell `rite` command (if one is
configured) and **before** the observance is dispatched. It is the System-1
decision step.

| Hook | When fired | Janet context | Return value |
|---|---|---|---|
| `on-vigil-rite` | Reaper drains a vigil, after shell rite, pre-observance | `{:vigil "<name>" :trigger :toll\|:watcher\|:harbinger :event_count N :payload "<coalesced JSON>"}` | `nil` to pass; `harness/block "reason"` to block |

The `:payload` is the same coalesced event state the observance would carry —
a single event's context, or `{"events": [...], "files": [...], "event_count": N}`
for a multi-event batch — serialized as a JSON string.

### Gate semantics (fail-open)

The gate only blocks on an explicit `harness/block` verdict. Everything else
proceeds:

- `nil` from the hook → pass
- hook responder dropped → warn + pass
- hook exceeds the 10-second timeout → warn + pass

The gate is also **off by default**: dirge enables it only when a loaded plugin
has registered `on-vigil-rite`. With no lev plugin, vigil behavior is unchanged.

### Registration

Plugins register `on-vigil-rite` the same way as any vigil hook:

```janet
(def hooks ["on-vigil-rite"])

(defn on-vigil-rite [ctx]
  (when-let [reason (lev-verdict ctx)]
    (harness/block reason)))
```

The plugin loader promotes the bare `defn` to `{plugin-stem}-on-vigil-rite`, so
no other registry change is needed.

## The `harness/http-post` bridge

dirge exposes a minimal HTTP bridge to the Janet plugin VM so plugins can call
sidecar services without shelling out to `curl` (which would reintroduce shell
injection):

```janet
(harness/http-post url body &opt headers)
```

- `url` — absolute URL string
- `body` — request body string (sent with `Content-Type: application/json`)
- `headers` — optional JSON object of string→string pairs (e.g.
  `"{\"Authorization\":\"Bearer s3cret\"}"`)
- returns the raw response body string on a 2xx status, `nil` on any
  error / non-2xx / timeout

The bridge uses reqwest's blocking client with a 10-second timeout and a
panic-safe FFI boundary; a hung sidecar cannot pin the Janet worker thread.
It is registered only on the plugin VM, not the notebook VM.

## The lev plugin

`plugins/vigil_lev.janet` posts the vigil state to `POST /v1/systemone`, asks a
single `noul` (yes/no) question, and blocks when the returned probability is
below the configured threshold.

Configuration is read from the environment at call time:

| Variable | Default | Meaning |
|---|---|---|
| `LEV_URL` | `http://127.0.0.1:8080` | lev base URL |
| `LEV_API_KEY` | unset | sent as `Authorization: Bearer <key>` when set |
| `LEV_THRESHOLD` | `0.8` | block when the `noul` probability is below this |

The plugin's `lev-verdict` function is the testable seam — it accepts an
optional config table (`:endpoint` / `:api-key` / `:threshold`) so tests can
point it at a mock lev without touching process env vars.

## More docs

- [Runbook](runbook.md) — run lev, wire it to dirge, and verify the gate
