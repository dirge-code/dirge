use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    /// dirge-69oe.4: optional `anchor:` frontmatter — the heading of the one
    /// section that must survive a compaction. `None` for the great majority
    /// of skills, which are read once and do not govern later turns.
    pub anchor: Option<String>,
    #[allow(dead_code)]
    pub location: PathBuf,
}

pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut map: HashMap<String, Skill> = HashMap::new();

    let global_dirs = dirs::home_dir().into_iter().flat_map(|home| {
        [
            home.join(".claude").join("skills"),
            home.join(".opencode").join("skills"),
            home.join(".agents").join("skills"),
            home.join(".dirge").join("skills"),
        ]
    });

    // `find_project_ancestor_dirs` returns cwd first, then parents
    // (inner → outer). The map insert below is last-write-wins, so
    // iterating in that natural order makes OUTER repos overwrite
    // INNER — the opposite of opencode's "more specific wins". For
    // a monorepo where both `monorepo/.dirge/skills/foo` and
    // `monorepo/svc/.dirge/skills/foo` exist, the svc-level skill
    // is the one a developer working in svc would expect. Reverse
    // so OUTER repos are visited first (and overwritten by INNER).
    // Audit H13.
    let mut project_ancestors = find_project_ancestor_dirs(cwd);
    project_ancestors.reverse();
    let project_dirs = project_ancestors.into_iter().flat_map(|ancestor| {
        [
            ancestor.join(".claude").join("skills"),
            ancestor.join(".opencode").join("skills"),
            ancestor.join(".agents").join("skills"),
            ancestor.join(".dirge").join("skills"),
        ]
    });

    for dir in global_dirs.chain(project_dirs) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                // UI-5: refuse to load skills whose directory or
                // SKILL.md is a symlink. Symlinks would let a
                // repo plant `.dirge/skills/innocent -> /etc/...`
                // and silently load whatever the link target
                // contains. `std::fs::metadata` follows links;
                // `symlink_metadata` does not.
                let lmeta = match std::fs::symlink_metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if lmeta.file_type().is_symlink() {
                    eprintln!("warning: skipping symlinked skill dir {:?}", path);
                    continue;
                }
                if !lmeta.is_dir() {
                    continue;
                }
                let skill_md = path.join("SKILL.md");
                let skill_lmeta = match std::fs::symlink_metadata(&skill_md) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if skill_lmeta.file_type().is_symlink() {
                    eprintln!("warning: skipping symlinked SKILL.md at {:?}", skill_md);
                    continue;
                }
                if !skill_lmeta.is_file() {
                    continue;
                }
                // Cap skill content at 1 MB. A skill is meant to be a
                // short markdown instructions file; multi-MB skills
                // would blow up LLM context. If users have legitimate
                // need for larger skills, they should compress and
                // bump this cap deliberately.
                const SKILL_MAX_BYTES: u64 = 1024 * 1024;
                if let Ok(meta) = std::fs::metadata(&skill_md)
                    && meta.len() > SKILL_MAX_BYTES
                {
                    eprintln!(
                        "warning: skipping skill {:?} ({} bytes > 1 MB cap)",
                        skill_md,
                        meta.len(),
                    );
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&skill_md)
                    && let Some(skill) = parse_skill(&content, &path)
                {
                    // README contract: "Project skills override
                    // global skills by name." Globals are iterated
                    // first (line 34), so use `insert` (last-write-
                    // wins) — `or_insert` kept the global value
                    // and silently dropped the project override.
                    if !skill.name.is_empty() {
                        map.insert(skill.name.clone(), skill);
                    }
                }
            }
        }
    }

    let mut skills: Vec<Skill> = map.into_values().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

pub fn find_project_ancestor_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = cwd.to_path_buf();
    dirs.push(current.clone());
    loop {
        if current.join(".git").is_dir() && !dirs.contains(&current) {
            dirs.push(current.clone());
        }
        if !current.pop() {
            break;
        }
    }
    dirs
}

fn parse_skill(content: &str, dir_path: &Path) -> Option<Skill> {
    let dir_name = dir_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let (frontmatter, body) = split_frontmatter(content);
    let body = body.trim();
    if body.is_empty() {
        return None;
    }

    let (name, description) = if frontmatter.is_empty() {
        (dir_name.to_string(), String::new())
    } else {
        parse_frontmatter(&frontmatter, dir_name)
    };

    // A frontmatter `name:` with an empty value would parse to "" and
    // then any subsequent `skill <empty>` call would silently match
    // the first such entry. Fall back to the directory name in that
    // case so every skill has a usable handle.
    let name = if name.trim().is_empty() {
        dir_name.to_string()
    } else {
        name
    };

    let anchor = if frontmatter.is_empty() {
        None
    } else {
        // Only honour an anchor that actually resolves. A heading that is not
        // in the body would otherwise be emitted as a marker the fold can
        // never satisfy -- a preservation guarantee that silently keeps
        // nothing, which is worse than not claiming one.
        parse_anchor(&frontmatter).filter(|h| extract_section(body, h).is_some())
    };

    Some(Skill {
        name,
        description,
        anchor,
        content: body.to_string(),
        location: dir_path.to_path_buf(),
    })
}

pub(crate) fn split_frontmatter(content: &str) -> (String, String) {
    let content = if let Some(c) = content.strip_prefix("---\n") {
        c
    } else if let Some(c) = content.strip_prefix("---\r\n") {
        c
    } else {
        return (String::new(), content.to_string());
    };

    if let Some(pos) = content.find("\r\n---") {
        let frontmatter = &content[..pos];
        let body = &content[pos + 5..];
        (frontmatter.to_string(), body.to_string())
    } else if let Some(pos) = content.find("\n---") {
        let frontmatter = &content[..pos];
        let body = &content[pos + 4..];
        (frontmatter.to_string(), body.to_string())
    } else {
        (String::new(), content.to_string())
    }
}

pub(crate) fn parse_frontmatter(frontmatter: &str, default_name: &str) -> (String, String) {
    let mut name = default_name.to_string();
    let mut description = String::new();

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("name:") {
            name = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("description:") {
            description = value.trim().to_string();
        }
    }

    (name, description)
}

/// dirge-69oe.4 — marker the skill tool emits so a compaction can find the
/// anchor heading without knowing anything about skills.
///
/// The fold sees `Turn { role, text, calls }` and NOTHING else — no tool name,
/// no skill registry — so the only way it can tell a skill body from any other
/// tool result is a marker in the text itself. Naming the heading rather than
/// repeating the section keeps the body verbatim and costs one line.
pub(crate) const SKILL_ANCHOR_OPEN: &str = "<!-- dirge:skill-anchor ";
pub(crate) const SKILL_ANCHOR_CLOSE: &str = " -->";

/// True when this text is a skill body emitted by the skill tool. Distinct
/// from [`anchor_marker_heading`]: a skill with no `anchor:` is still MARKED
/// (so it can get the head fallback) but has no heading.
pub(crate) fn is_skill_body(text: &str) -> bool {
    text.contains(SKILL_ANCHOR_OPEN)
}

/// Read the anchor heading back out of a marked message, if it carries one.
pub(crate) fn anchor_marker_heading(text: &str) -> Option<&str> {
    let start = text.find(SKILL_ANCHOR_OPEN)? + SKILL_ANCHOR_OPEN.len();
    let rest = &text[start..];
    let end = rest.find(SKILL_ANCHOR_CLOSE)?;
    let heading = rest[..end].trim();
    if heading.is_empty() {
        None
    } else {
        Some(heading)
    }
}

/// dirge-69oe.4 — read the optional `anchor:` frontmatter key.
///
/// A skill's body is an ordinary tool result: it rides in history and is
/// truncated or pruned at the first compaction like anything else. For most
/// skills that is fine — they were read, they did their job. For a skill that
/// governs HOW the model operates for the rest of the run, losing the body
/// silently disables it while the run carries on looking healthy.
///
/// `anchor:` names the one section that has to survive that fold. Quotes are
/// optional; an empty or missing value means no anchor, never an anchor that
/// cannot match.
pub(crate) fn parse_anchor(frontmatter: &str) -> Option<String> {
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("anchor:") {
            let v = value.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .unwrap_or(v)
                .trim();
            if v.is_empty() {
                return None;
            }
            return Some(v.to_string());
        }
    }
    None
}

/// Extract a markdown section by its heading, up to the next heading of the
/// same or shallower depth.
///
/// Depth-awareness is the whole point: a section must keep its own
/// subsections and stop at its sibling. Both failure modes are silent — too
/// greedy carries the rest of the file through every fold, too eager cuts the
/// section at its first `###` and preserves a fragment that reads complete.
///
/// Returns `None` when the heading is absent, so a typo'd `anchor:` degrades
/// to "no anchor" rather than "keep everything".
pub(crate) fn extract_section(body: &str, heading: &str) -> Option<String> {
    let heading = heading.trim();
    let depth = heading.chars().take_while(|c| *c == '#').count();
    if depth == 0 {
        return None;
    }
    let mut out: Vec<&str> = Vec::new();
    let mut in_section = false;
    for line in body.lines() {
        let trimmed = line.trim_end();
        if in_section {
            let d = trimmed.chars().take_while(|c| *c == '#').count();
            // A heading at the same or shallower depth ends the section. Deeper
            // ones are subsections and stay.
            if d > 0 && d <= depth && trimmed[d..].starts_with(' ') {
                break;
            }
            out.push(trimmed);
        } else if trimmed == heading {
            in_section = true;
            out.push(trimmed);
        }
    }
    if !in_section {
        return None;
    }
    let joined = out.join("\n");
    let joined = joined.trim_end();
    if joined.is_empty() {
        None
    } else {
        Some(joined.to_string())
    }
}

pub fn find_skill<'a>(name: &str, skills: &'a [Skill]) -> Option<&'a Skill> {
    skills.iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dirge-69oe.4 — an optional `anchor:` frontmatter key names the section
    /// a skill needs kept when its body is folded away. Quotes are optional
    /// and stripped, because both forms are natural to write and a stray pair
    /// of quotes would silently stop the section from ever matching.
    #[test]
    fn frontmatter_anchor_is_parsed_and_unquoted() {
        assert_eq!(
            parse_anchor("name: j\nanchor: \"## The J-Space Premise\""),
            Some("## The J-Space Premise".to_string())
        );
        assert_eq!(
            parse_anchor("anchor: ## Plain Unquoted"),
            Some("## Plain Unquoted".to_string())
        );
        // Absent, blank, and quotes-around-nothing all mean "no anchor" rather
        // than an anchor that can never match.
        assert_eq!(parse_anchor("name: j\ndescription: d"), None);
        assert_eq!(parse_anchor("anchor:"), None);
        assert_eq!(parse_anchor("anchor: \"\""), None);
    }

    /// Extraction runs to the next heading of the SAME OR SHALLOWER depth, so
    /// a section keeps its own subsections and stops at its sibling. Getting
    /// this wrong in either direction is silent: too greedy swallows the rest
    /// of the file, too eager truncates the section at its first `###`.
    #[test]
    fn anchor_section_extraction_respects_heading_depth() {
        let body = "# Title\nintro\n\n## Premise\nline one\n\n### Sub\nnested stays\n\n## Next\nexcluded\n";
        let got = extract_section(body, "## Premise").expect("section found");
        assert!(got.contains("line one"), "got: {got}");
        assert!(got.contains("nested stays"), "subsection must stay: {got}");
        assert!(!got.contains("excluded"), "must stop at the sibling: {got}");

        // A heading that is not there yields None rather than the whole body —
        // the must-not-fire half. A typo'd anchor must degrade to "no anchor",
        // never to "keep everything".
        assert_eq!(extract_section(body, "## Nope"), None);
    }

    /// The marker has to survive the round trip through a tool result, since
    /// that is the only channel between the loader and the fold.
    #[test]
    fn anchor_marker_round_trips() {
        let text = format!(
            "# j-space\n{SKILL_ANCHOR_OPEN}## The J-Space Premise{SKILL_ANCHOR_CLOSE}\n\nbody"
        );
        assert_eq!(anchor_marker_heading(&text), Some("## The J-Space Premise"));
        // Unmarked text yields None rather than a bogus heading -- otherwise
        // every tool result in the fold would look like a skill.
        assert_eq!(anchor_marker_heading("# j-space\n\nordinary body"), None);
        assert_eq!(
            anchor_marker_heading(&format!("{SKILL_ANCHOR_OPEN}{SKILL_ANCHOR_CLOSE}")),
            None
        );
    }

    #[test]
    fn test_split_frontmatter() {
        let (fm, body) = split_frontmatter("---\nname: test\ndescription: desc\n---\nbody here");
        assert_eq!(fm, "name: test\ndescription: desc");
        assert_eq!(body.trim(), "body here");
    }

    #[test]
    fn test_split_frontmatter_no_fm() {
        let (fm, body) = split_frontmatter("just body");
        assert!(fm.is_empty());
        assert_eq!(body, "just body");
    }

    #[test]
    fn test_split_frontmatter_crlf() {
        let (fm, body) = split_frontmatter("---\r\nname: test\r\n---\r\nbody");
        assert_eq!(fm, "name: test");
        assert_eq!(body.trim(), "body");
    }

    #[test]
    fn test_parse_frontmatter() {
        let (name, desc) = parse_frontmatter("name: my-skill\ndescription: Does stuff", "default");
        assert_eq!(name, "my-skill");
        assert_eq!(desc, "Does stuff");
    }

    #[test]
    fn test_parse_frontmatter_falls_back_to_default_name() {
        let (name, desc) = parse_frontmatter("description: Does stuff", "dir-name");
        assert_eq!(name, "dir-name");
        assert_eq!(desc, "Does stuff");
    }

    #[test]
    fn discover_finds_skills_under_dot_agents() {
        // A project-scoped `.agents/skills/<name>/SKILL.md` must be picked
        // up (cwd is always a project ancestor, no .git needed).
        let dir = std::env::temp_dir().join(format!("dirge-agents-skill-{}", std::process::id()));
        // dirge-m1ni: clear first. The name is keyed on the process id and
        // nothing removes it, so a run whose pid the OS recycled would
        // otherwise inherit the previous run's files — the shape that made
        // a spec_db test fail on impossible counts.
        let _ = std::fs::remove_dir_all(&dir);
        let skill_dir = dir.join(".agents").join("skills").join("agents-dir-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: agents-dir-skill\ndescription: from .agents\n---\nDo the agents thing.\n",
        )
        .unwrap();

        let skills = discover_skills(&dir);
        assert!(
            find_skill("agents-dir-skill", &skills).is_some(),
            "a skill under .agents/skills must be discovered: {:?}",
            skills.iter().map(|s| &s.name).collect::<Vec<_>>(),
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_skill_rejects_empty_body() {
        let skill = parse_skill("---\nname: test\n---\n   \n", Path::new("/tmp/test-skill"));
        assert!(skill.is_none());
    }

    #[test]
    fn test_find_skill() {
        let skills = vec![Skill {
            name: "test".into(),
            description: "desc".into(),
            content: "body".into(),
            anchor: None,
            location: PathBuf::from("/tmp"),
        }];
        assert!(find_skill("test", &skills).is_some());
        assert!(find_skill("missing", &skills).is_none());
    }
}
