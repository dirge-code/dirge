# Prompts system

Prompts change the agent's behavior and tone, and can declare tool
restrictions enforced at the permission layer. Switch at runtime with
`/prompt [name]`.

## Built-in prompts

| Prompt | Description |
|--------|-------------|
| **`code`** (default) | Coding mode with full tool access, TDD workflow |
| **`plan`** | Planning-only mode — `edit`/`write`/`apply_patch`/`bash`/`webfetch` are denied at the permission layer (via `deny_tools` frontmatter). Plan is delivered as the chat reply; the user saves it to disk if desired. |
| **`review`** | Code review mode — same deny list as plan; findings delivered in chat |
| **`debug`** | Debug mode — finds root cause before proposing fixes |
| **`ask`** | Read-only mode — `edit`/`write`/`apply_patch`/`bash`/`webfetch` denied via deny_tools |
| **`brainstorm`** | Design-only mode — explores ideas and presents designs without code |
| **`frontend-design`** | Frontend design mode — distinctive, production-grade UI |
| **`review-security`** | Security review mode — same deny list as plan/review; finds exploitable vulnerabilities |
| **`simplify`** | Code simplification mode — refines for clarity without changing behavior |
| **`write-prompt`** | Prompt writing mode — creates and optimizes agent prompts |
| **`default`** | Default system prompt — the base built-in prompt |

## Per-prompt tool restrictions

Each prompt is a markdown file with optional YAML frontmatter declaring its
tool restrictions:

```markdown
---
deny_tools: [edit, write, apply_patch, bash, webfetch]
description: Read-only planning mode
---
You are dirge in plan mode. …
```

The permission checker refuses any denied tool BEFORE the call leaves dirge
— even under `--yolo` mode. Applies symmetrically to MCP tools: an entry
in `deny_tools` matches an MCP-exported tool when the entry equals
**any** of the following:

- the bare tool name as the MCP server registered it (e.g. `edit` matches
  an MCP `edit` tool from any server — convenient blanket deny, but be
  aware that `deny_tools: [edit]` intended for the built-in editor will
  also block an MCP server's `edit` tool)
- the qualified `mcp_tool:<server>:<name>` form (for narrowly denying a
  specific server's tool)
- the umbrella `mcp_tool` (denies every MCP tool from every server)

For surgical control over one MCP tool without affecting the built-in, use
the qualified form.

## Critic control (frontmatter)

Two optional keys steer the F6 in-loop critic (see [`config.md`](config.md),
`critic_provider`) **per prompt**, without touching config:

- `critic: false` — suppress the critic **and the diff-aware code reviewer**
  for this prompt only (both share the `critic_provider` judge, so the flag
  gates both; read-only/exploratory modes leave no diff to review anyway). The
  **goal gate** (`--goal`) is unaffected: it has its own judge under its own
  fixed preamble. `critic: true`, or omitting the key, inherits the global
  behavior.
- `critic_preamble:` — override the critic's system preamble for this
  prompt. Wins over `critic_preamble` in config and the built-in. Inline
  string or a YAML block scalar (`|`) for multi-line:

  ```markdown
  ---
  critic: false
  ---
  ```

  ```markdown
  ---
  critic_preamble: |
    You are a security-focused reviewer.
    Block only on concrete, in-scope gaps.
  ---
  ```

An empty `critic_preamble` is treated as unset (inherits). Block-scalar
indentation is stripped; folding (`>`) is not supported.

## Custom prompts

Custom prompts can be placed in `$XDG_CONFIG_HOME/dirge/prompts/` as `.md` files
(available to every project), and/or in `<project>/.dirge/prompts/` for
project-local prompts. A project-local prompt with the same stem as a global or
built-in prompt overrides it.

## Model-aware steering

Separate from the selectable prompt modes above, the harness appends a
model-specific guidance fragment to the preamble based on the **active model
family** (resolved from the provider + model id). This is automatic and has no
config key.

Today only DeepSeek **chat** models (v3/v4) receive a fragment,
`prompts/steering/deepseek.md`, embedded in the binary via `include_str!`. It
encodes a Plan-Execute-Verify working method, structural-constraint framing, an
explicit success/never contract, and an anti tool-call-repetition rule — the
dominant failure modes for DeepSeek in agentic loops. It is appended **last** in
the preamble so it sits closest to the conversation / action boundary, where
rules best resist "prompt-distance drift" in long tool-calling sequences.

The DeepSeek **reasoner** (R1) is deliberately excluded — it ignores the system
prompt — and every non-DeepSeek model is unaffected. Steering fragments live
under `prompts/steering/` rather than `prompts/`, so they are **not** selectable
`/prompt` modes and never appear in the `/prompt` list. To edit the guidance,
change the markdown file and rebuild.

## Context files

The agent automatically loads `AGENTS.md` or `CLAUDE.md` from the project root,
ancestor directories, and `~/.config/dirge/agent/AGENTS.md` as a global
fallback. Use `-n` / `--no-context-files` to disable.
