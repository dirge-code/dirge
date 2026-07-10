# Agent profiles

An **agent profile** is a named, reusable persona: a bundle of *system prompt*,
*model*, and *tool policy* you can switch to at runtime. Where a prompt
(`/prompt`) only changes the system prompt + tool restrictions, an agent profile
also routes the loop to a different **model** — so you can keep the right
model-for-the-job one keystroke away (a cheap fast model for review, a stronger
one for hard implementation) without editing config.

The feature is **fully opt-in**: with no profiles defined, dirge behaves exactly
as before. Defining a profile changes nothing until you `/agent <name>` into it.

> Agent profiles drive the **main loop's active persona**. They are separate
> from — and do not change — the built-in role routing (critic / review /
> escalation / summarization / subagent), which is still configured via the
> `*_provider` keys in `config.json`. `/agents` shows both side by side.

## Defining profiles

Profiles come from three sources, layered so a more specific one overrides a
more general one of the same name:

| Precedence | Source | Location |
|---|---|---|
| highest | project file | `.dirge/agents/<name>.md` |
| middle | global file | `~/.config/dirge/agents/<name>.md` |
| lowest | config | `config.json` `"agents": { "<name>": { … } }` |

### File form (`.dirge/agents/<name>.md`)

The filename stem is the profile name. The file is markdown with optional
YAML-ish frontmatter (the same tiny format as prompts/skills — no nested
objects); the body is the system prompt.

```markdown
---
model: haiku
deny_tools: [bash, write, edit, apply_patch]
reasoning: high
temperature: 0.2
description: read-only reviewer on a cheap fast model
---
You are a meticulous code reviewer. Read the diff and the surrounding code,
then report concrete findings ordered by severity. Do not modify files.
```

All frontmatter keys are optional:

| Key | Meaning |
|---|---|
| `model` | A `providers` alias **or** a model name. Resolved to a model string for the current client (see *Model routing* below). Omit to keep the current model. |
| `deny_tools` | Tools to deny while this profile is active (e.g. `[bash, write, edit, apply_patch]`). |
| `allow_tools` | The complement: deny every built-in **not** listed. `deny_tools` wins if both are given. |
| `reasoning` | Reasoning-effort hint (`low` / `medium` / `high`). |
| `temperature` | Sampling temperature. |
| `description` | One-line summary shown in `/agents`. |
| `subagent_tools` | Opt this profile's `task(agent=…)` subagent into tools: `readonly` (read-only tool universe), `readwrite` (readonly + write/edit/bash — can edit the repo), or `toolless` (the default one-shot). See *Tooled subagents* below. |
| `subagent_max_turns` | Cap the tooled subagent's loop (default `25`). |
| `subagent_deny` | Narrow the tool set further within the tier (e.g. `[webfetch]`). |

A frontmatter-less file is treated as a body-only profile (just a system
prompt).

### Config form (`config.json`)

The same shape as a JSON object, for profiles you'd rather keep in config:

```json
{
  "agents": {
    "reviewer": {
      "model": "haiku",
      "deny_tools": ["bash", "write", "edit", "apply_patch"],
      "description": "read-only reviewer on a cheap fast model"
    },
    "researcher": {
      "model": "haiku",
      "subagent": {
        "tools": "readonly",
        "max_turns": 15,
        "deny": ["webfetch"]
      },
      "description": "tooled subagent that reads the repo directly"
    },
    "architect": {
      "model": "opus",
      "prompt": "You are a senior architect. Think in trade-offs; propose a plan before code."
    }
  }
}
```

## Using profiles

| Command | Effect |
|---|---|
| `/agents` (or `/agent`) | List defined profiles (active one marked `*`) **and** the built-in role routing. |
| `/agent <name>` | Activate a profile: apply its system prompt, tool policy (at the permission layer), and model (rebuilds the agent). |
| `/agent off` | Deactivate the profile and restore the underlying state: the active `/prompt`'s prompt + denies come back, and the model is restored to whatever it was **before** the profile was activated. |

The `/prompt` and `/agent` selections are **independent composing layers**, not
one shared slot. Activating a profile no longer wipes the active prompt's
restrictions, and deactivating it no longer wipes the prompt:

- **Prompt** — if the profile defines a body, it overrides the active system
  prompt while the profile is on; otherwise your `/prompt` body is kept. The
  "mode" (which drives plan/review reminders) stays owned by `/prompt`.
- **Tools** — the profile's `deny_tools` / `allow_tools` are enforced at the
  **permission layer** (the same path that backs per-prompt restrictions), not
  just as prose. They **compose** with the active prompt's `deny_tools` as a
  union: a profile can only **add** restrictions to a prompt's, never weaken
  them (so `/prompt review` + a permissive profile keeps `review`'s denies). To
  drop a prompt's restrictions, switch the prompt (`/prompt default`), not the
  agent. `allow_tools` is best-effort over built-in tools; for a hard cap prefer
  `deny_tools`.
- **Revert** — `/agent off` pops only the profile layer: the `/prompt` layer's
  body + denies and the pre-profile model are restored in one step.
- **Model** — see below.

## Model routing

A profile's `model` is resolved to a model string for the **current** client:
if it names a `providers` alias that carries a `model`, that model is used;
otherwise the value is used verbatim as the model name. This covers the common
case (e.g. everyone on one OpenRouter/Anthropic account switching between
models).

> Cross-provider switching — a profile model that points at a *different*
> backend (its own `provider_type` / `base_url` / API key) — is not yet wired
> for `/agent`; only the model string is taken. The built-in roles
> (critic/escalation) already build full per-role clients and are unaffected.

## Running a subagent under a profile

The `task` tool — which spawns a one-shot subagent for an independent subtask —
can run that subagent under a profile. When any profiles are defined, the tool
advertises an `agent` parameter (an enum of your profile names); the model calls
it like:

```
task(prompt="Review the auth changes for security issues", agent="reviewer")
```

The subagent then runs on the profile's **model** and **system prompt**, so you
can fan work out to specialized personas (a cheap fast reviewer, a stronger
planner) from a single session. Omitting `agent` uses the default subagent,
exactly as before — the parameter only appears when profiles exist, and naming a
profile that isn't defined is a hard error (no silent fallback). The default
subagent (no `agent=`) runs on `subagent_provider`'s model when that role key is
configured, otherwise the main model.

Profiles are resolved into subagent routes once at startup. By default
subagents are **tool-less** (a one-shot query — a profile's
`deny_tools`/`allow_tools` doesn't apply, since the subagent has no tools),
and the profile's `reasoning`/`temperature` aren't applied on the subagent
path — only the model and system prompt are. Routing `/plan` phases to named
profiles, and cross-provider client switching, remain follow-ups.

### Tooled subagents (opt-in)

A profile can opt its `task(agent=…)` subagent into a **real filtered agent
loop** with tools, instead of the tool-less one-shot. Set `subagent.tools`
(frontmatter `subagent_tools`, or the `subagent` block in `config.json`):

- **`toolless`** (default) — unchanged one-shot, no tools.
- **`readonly`** — the subagent runs a real loop with the read-only tool
  universe: `read`, `read_minified`, `grep`, `find_files`, `glob`,
  `list_dir`, `repo_overview`, `websearch`, `webfetch` (web tools only when
  enabled in config). It can investigate the repo directly and report back —
  ideal for research/exploration subtasks.
- **`readwrite`** — the subagent runs a real loop with the read-only universe
  PLUS the write family: `write`, `edit`, `edit_lines`, `edit_minified`,
  `apply_patch`, `bash`, `bash_output`, `kill_shell`. It can edit the code
  tree and run builds/tests directly — ideal for delegating implementation
  subtasks to a subagent.

The tool set is **intersected with the tier's universe**, so `allow` can
never escalate past the tier (a readonly profile can't `allow` its way to
`edit`) and `deny` only narrows. A mandatory floor is then stripped from
every tooled subagent regardless of tier or `allow`: recursion (`task`,
`task_status`), durable writes (`memory`, `skill`, `spec`),
session-attribution tools (`session_search`, `issue`, `write_todo_list`,
`graph`), and interactive tools (`question`, `plan_enter`, `plan_exit`).
So even a `readwrite` subagent can edit the repo but can't write durable
agent state or attribute to a session. The subagent runs under a fresh
child session id and a bounded turn cap (`subagent.max_turns`, default
`25`), so it can't recurse, write side effects out of band, or loop
forever.

Permissions inherit the parent agent: in-cwd reads/edits are auto-allowed,
and a path outside the cwd surfaces a permission prompt through the parent
UI. If a profile pinned a model, the tooled subagent runs on that model;
otherwise it uses the live agent's.

## Relationship to the built-in critic and roles

Defining profiles never disables or changes the built-in critic or any role
routing. The critic is still opt-in via `critic_provider`; review / escalation /
summarization / subagent still resolve through their `*_provider` keys.
`/agents` surfaces both the user profiles and the configured role routing so the
whole picture is visible in one place.
