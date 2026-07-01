# Claude-compatible skills

Skills are on-demand instruction bundles the agent can load mid-session via the
`skill` tool. dirge reads the same format as Claude and opencode.

Place skill directories in `.claude/skills/`, `.opencode/skills/`,
`.agents/skills/`, or `.dirge/skills/` in your project or home directory.
Each skill is a directory
containing `SKILL.md` with optional YAML frontmatter:

```markdown
---
name: my-skill
description: A helpful skill
---
# Instructions
Detailed skill content here.
```

Skills are auto-discovered at agent startup and listed in the system prompt,
**ordered by salience** (see below) so the most useful skills surface first. The
agent calls `skill "my-skill"` to load the full content on demand. Project skills
override global skills by name.

## Learning a skill: `/learn`

`/learn` turns source material into a reusable skill without hand-authoring it:

```
/learn ~/projects/acme-sdk, focus on auth + pagination
/learn https://docs.example.com/api/quickstart
/learn filing an expense: open the portal, New > Expense, attach receipt, submit
/learn            # bare: distill the workflow from the current conversation
```

There's no separate distillation engine — `/learn` builds one standards-guided
instruction that the agent runs with its normal tools (`read`, `grep`,
`find_files`, `webfetch`), then saves the result through the `skill` tool. It
gathers every source named, applies any focus/constraints in the request, and
writes one SKILL.md.

Every created skill must include a `## Verification` section — a single command
that proves the skill works. Creation is rejected without it, and a freshly
learned skill starts with one recorded success (it was validated in the session
that produced it) so its salience begins grounded.

## Salience and curation

Skill telemetry lives in the project's SQLite database
(`.dirge/sessions/state.db`), reusing the same salience engine as memory:

- **Reinforce on use** — loading a skill nudges its salience up.
- **Disuse decay** — the background curator decays skills nobody consults.
- **Effectiveness** — a skill's success/failure record folds into its ranking, so
  one that keeps working outranks one that has failed in practice.
- **Confidence** — a tiebreak among equally-salient skills.

The curator archives an agent-created skill once its *effective* salience falls to
the archival threshold — i.e. it's gone unused long enough that decay overtakes any
track record. A skill that keeps proving useful survives on its effectiveness even
if it hasn't been touched recently; **pinned** skills and skills you didn't author
(bundled/manual) are never auto-archived. Archived skills move to
`.dirge/skills/.archive/` and are recoverable. This replaces the earlier age-only
staleness rule so the library self-prunes on *usefulness*, not just recency.

## Bundled starter skills

The repo ships a small pack of general-purpose workflow skills under
[`skills/`](../skills/) — `systematic-debugging`, `code-review-feedback`, and
`writing-skills`. They are **not** installed automatically; copy the ones you
want into a discovered location, e.g. `cp -r skills/systematic-debugging
.dirge/skills/` (per project) or `~/.dirge/skills/` (global). See
[skills/README.md](../skills/README.md).
