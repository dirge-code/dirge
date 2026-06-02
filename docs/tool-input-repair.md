# Tool-Input Repair

The tool-input repair layer catches common malformed tool-call arguments emitted by LLMs and rewrites them into shapes the tool's schema accepts. Without it, recoverable formatting mistakes — null-valued optionals, JSON-strings where arrays are expected, markdown-wrapped paths — surface as opaque deserialization errors that the model cannot fix from its side.

The problem is most visible with open models (DeepSeek, GLM, Qwen) whose post-training distributions occasionally produce structurally-near-correct tool calls. Repair runs as a validate-then-repair pass: valid inputs are never touched; only inputs that fail schema validation are walked, and only at the exact paths the schema disagreed at.

## How it works

1. Try the input as-is. If the tool's schema accepts it, the call ships unchanged.
2. On validation failure, walk the schema and the input together, applying targeted repairs at each failing field.
3. Retry validation once. On success, log a `tool_input_repaired` event tagged with the kinds of repair applied. On failure, log `tool_input_invalid` and return a model-readable retry message.

Schema-driven annotations (`dirge-hints`) let tools opt fields into semantic-specific repairs (path-shaped fields get markdown-link unwrapping; relational defaults fill paired arguments when one is provided alone).

## Repairs applied

```rust
pub enum RepairKind {
    NullStripped,
    JsonStringToArray,
    ObjectToArray,
    BareStringToArray,
    MdLinkUnwrapped,
}
```

| Kind | Detects | Action |
|---|---|---|
| `NullStripped` | Top-level keys whose value is `null` where the schema marks the field optional | Drop the key |
| `JsonStringToArray` | A `string` value at a path the schema declares `array`, matching `^\s*\[.*\]\s*$` | Parse the string as JSON and substitute if the parse yields an array |
| `ObjectToArray` | An empty `{}` at a path the schema declares `array` | Substitute `[]` |
| `BareStringToArray` | A bare `string` at a path the schema declares `array` | Wrap in a singleton array `[input]` |
| `MdLinkUnwrapped` | A markdown auto-link in a path-shaped field, of the form `[text](http(s)://text)` where the link text equals the URL stripped of protocol | Unwrap to the link text |

Order matters: `JsonStringToArray` runs before `BareStringToArray`, otherwise `"[\"a\",\"b\"]"` would wrap into `["[\"a\",\"b\"]"]` instead of parsing to `["a","b"]`.

Real markdown links where text and URL are semantically different pass through `MdLinkUnwrapped` untouched. The repair only fires on the degenerate auto-link case.

### Relational defaults

A separate pass before the shape repairs reads `dirge-hints.relational` from the schema and fills missing companion fields when their counterpart is present. Example: `read({path, limit:30})` gets `offset: 0` injected. The defaulted value is recorded as a `Note:` line prepended to the tool result so the model sees the chosen default.

## `dirge-hints` schema annotations

Tools declare repair contracts directly in their JSON Schema under the `dirge-hints` extension namespace.

### `semantic` — per-property tags

Used by `MdLinkUnwrapped` to identify path-shaped fields:

```json
{
  "type": "object",
  "properties": {
    "path": {
      "type": "string",
      "dirge-hints": { "semantic": "absolute_path" }
    }
  }
}
```

Tagged fields receive the markdown-link unwrap; untagged string fields do not, even if they happen to contain a markdown link (so `content` / `body` text passes through verbatim).

### `relational` — paired-field defaults

Declared at the top level of the schema:

```json
{
  "type": "object",
  "dirge-hints": {
    "relational": [
      { "fields": ["offset", "limit"], "default": { "offset": 0, "limit": 2000 } }
    ]
  }
}
```

When some but not all named fields are present, the missing ones are filled from `default`, and a note describing the fill is added to the tool result.

### `contract_hint_for`

Tools may register a free-text contract hint via `contract_hint_for(tool_name)`. The hint is appended to the tool's description (`with_contract_hint`) and shown to the model in the tool catalog, biasing the model away from the malformations the repair layer catches.

## Telemetry

Two `tracing` events fire from the repair path:

| Event | Target | Fields | Fires when |
|---|---|---|---|
| `tool_input_repaired` | `tool_repair` | `tool`, `model`, `repair_kind` | Repair walk succeeded and retry validation passed |
| `tool_input_invalid` | `tool_input_invalid` | `tool`, `model` | Repair walk could not produce a valid input |

`RepairStats` carries per-kind atomic counters plus an `invalid` counter for the lifetime of an agent run. The snapshot is emitted as a `LoopEvent::RepairStats` at `AgentEnd` and bridged to `AgentEvent::RepairStats` for the UI to display at session close.

```rust
pub struct RepairStatsSnapshot {
    pub null_stripped: u64,
    pub json_string_to_array: u64,
    pub object_to_array: u64,
    pub bare_string_to_array: u64,
    pub md_link_unwrapped: u64,
    pub invalid: u64,
}
```

Enable verbose logs with `RUST_LOG=tool_repair=info,tool_input_invalid=info`.

## Error formatting

When all repairs fail, the raw `serde_json::Error` is not surfaced to the model. `format_tool_error` (in `rig_tool.rs`) translates the failure into a structured retry message naming the rejected field, the expected shape from the schema, and a concrete hint when one is available.

## Where it lives

The repair layer lives in `src/agent/agent_loop/tool_input_repair/`:

| Symbol | Role |
|---|---|
| `RepairKind` | Enum of the five repair operations |
| `validate_and_repair` | Entry point — runs relational defaults, then shape repairs, returns the rewritten args plus the list of applied kinds and any notes |
| `apply_relational_defaults` | Reads `dirge-hints.relational` and fills missing paired fields |
| `unwrap_md_links_in_args` | Walks args + schema, unwrapping markdown auto-links at fields tagged with a path-shaped semantic |
| `strip_null_recursive` | First-pass null-strip pass |
| `try_repairs_at_value` | Per-field shape repair dispatch (`JsonStringToArray`, `ObjectToArray`, `BareStringToArray`) |
| `RepairStats` / `RepairStatsSnapshot` | Per-run atomic counters and the value carried in `LoopEvent::RepairStats` |
| `contract_hint_for` / `with_contract_hint` | Free-text contract hints injected into tool descriptions |
| `SemanticTag` | Enum of recognised `dirge-hints.semantic` values |

Call sites: `tools.rs::prepare_tool_call` invokes `validate_and_repair` when initial deserialization fails. `stream.rs` and `steering.rs` construct the shared `RepairStats` instance threaded through the run.
