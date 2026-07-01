//! `/learn` prompt builder (dirge-s99m).
//!
//! Port of Hermes's `agent/learn_prompt.py`. `/learn` turns source
//! material — a directory, a URL, pasted notes, or "what we just did" —
//! into a reusable skill, WITHOUT a separate distillation engine: it
//! builds one standards-guided instruction that the live agent runs as a
//! normal turn, gathering sources with its existing tools (`read`,
//! `grep`/`find_files`, `webfetch`) and saving the result through the
//! `skill` tool. Because the agent does the sourcing, `/learn` works the
//! same on every backend.
//!
//! The authoring standards are HARDLINE house style so learned skills
//! are consistent and — per the verification gate (dirge-pb1p) — carry a
//! `## Verification` command that proves the skill works and seeds its
//! effectiveness record.

/// The empty-`/learn` fallback: distill the current conversation.
const CONVERSATION_FALLBACK: &str = "the workflow we just went through in \
this conversation — review the steps taken and distill them into a \
reusable skill";

/// House-style authoring rules embedded in every `/learn` turn.
const AUTHORING_STANDARDS: &str = r###"AUTHORING STANDARDS (follow exactly):

Frontmatter (YAML between --- fences):
  - name: lowercase-hyphenated, <=64 chars, matches the skill's purpose
  - description: ONE sentence, <=80 chars, no trailing period needed

Body sections (Markdown; omit a section only if it genuinely has no content):
  1. A short title line + 1-2 sentence intro
  2. "## When to Use" — bullet triggers for reaching this skill
  3. "## Prerequisites" — env vars, installs, credentials (skip if none)
  4. "## Procedure" — numbered steps with copy-paste commands
  5. "## Pitfalls" — known limits, gotchas (skip if none)
  6. "## Verification" — a SINGLE command that proves the skill worked.
     This is REQUIRED: it is what lets the skill's effectiveness be
     checked and tracked over time.

Quality bar:
  - Use ONLY commands, paths, flags, and APIs that appear VERBATIM in the
    source material. Never invent them.
  - Distill — do not paste the source docs back wholesale.
  - Frame actions through dirge's tools where natural (`read`, `grep`,
    `find_files`, `bash`, `webfetch`), not raw shell utility names.
  - Aim for ~100 lines for a simple skill, ~200 for a complex one.
  - Do NOT author a router/index skill that only points at other skills."###;

/// Build the agent instruction for a `/learn` request. `user_request` is
/// the free text after `/learn` (paths, URLs, notes, requirements, in any
/// mix); empty falls back to distilling the conversation.
pub fn build_learn_prompt(user_request: &str) -> String {
    let request = user_request.trim();
    let request = if request.is_empty() {
        CONVERSATION_FALLBACK
    } else {
        request
    };

    format!(
        "[/learn] Turn the request below into a reusable skill, and save it.

THE REQUEST:
{request}

Do this:
1. Gather every source the request names — use `read` for local files,
   `find_files`/`grep` to explore a directory, and `webfetch` for URLs.
   If the request is about work we just did, review this conversation.
2. Apply every requirement, focus, and constraint in the request — prose
   next to a source is authoring guidance, not noise. Cover what it asks
   for and skip what it says to skip.
3. Author ONE skill following the standards below, then save it by
   calling the `skill` tool with action='create', a sensible `name`, and
   the full SKILL.md text (frontmatter + body) as `content`.

{AUTHORING_STANDARDS}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_the_request_verbatim() {
        let p = build_learn_prompt("~/proj/sdk focus on auth, skip deprecated");
        assert!(p.contains("~/proj/sdk focus on auth, skip deprecated"));
    }

    #[test]
    fn always_includes_authoring_standards() {
        let p = build_learn_prompt("anything");
        assert!(p.contains("AUTHORING STANDARDS"));
        // The verification requirement is the hook for the effectiveness
        // gate (dirge-pb1p) — it must always be present.
        assert!(p.contains("## Verification"));
    }

    #[test]
    fn instructs_saving_via_the_skill_tool() {
        let p = build_learn_prompt("x");
        assert!(p.contains("skill") && p.contains("action='create'"));
    }

    #[test]
    fn references_the_gather_tools() {
        let p = build_learn_prompt("x");
        for tool in ["read", "find_files", "grep", "webfetch"] {
            assert!(p.contains(tool), "prompt should mention `{tool}`");
        }
    }

    #[test]
    fn empty_request_falls_back_to_the_conversation() {
        let p = build_learn_prompt("   ");
        assert!(p.contains(CONVERSATION_FALLBACK));
        assert!(!p.contains("THE REQUEST:\n\n"));
    }
}
