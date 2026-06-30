//! Skill format validation and frontmatter parsing.
//!
//! Skills are stored as directories under `.dirge/skills/` with a
//! `SKILL.md` file. The file starts with YAML frontmatter (between
//! `---` delimiters) followed by Markdown body content.

/// A parsed skill specification — the in-memory representation of
/// a `SKILL.md` file with its frontmatter metadata extracted.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillSpec {
    /// Skill name (lowercase, hyphens, max 64 chars). From
    /// frontmatter `name:` field or the directory name.
    pub name: String,
    /// Human-readable description from frontmatter `description:`.
    pub description: String,
    /// The full file content (frontmatter + body).
    pub content: String,
    /// Tags extracted from `tags:` in frontmatter dirge metadata.
    pub tags: Vec<String>,
    /// Related skill names from `related_skills:` in metadata.
    pub related: Vec<String>,
    /// The body content (everything after the closing `---`).
    pub body: String,
}

// ── Validation constants ───────────────────────────────

/// Maximum length of a skill name in bytes. 256 bytes is plenty
/// for UTF-8 identifiers (e.g. ~85 CJK code points) while still
/// being a sane upper bound that bounds memory and prevents abuse.
const MAX_NAME_LEN: usize = 256;

/// Maximum total content size (100K chars ≈ 36K tokens).
const MAX_CONTENT_LEN: usize = 100_000;

// ── Public API ─────────────────────────────────────────

/// Parse a `SKILL.md` file's content into a [`SkillSpec`]. Uses
/// `dir_name` as the fallback name when frontmatter omits it.
pub fn parse_skill_spec(content: &str, dir_name: &str) -> Option<SkillSpec> {
    let (frontmatter, body) = split_frontmatter(content)?;
    let body = body.trim().to_string();
    if body.is_empty() {
        return None;
    }

    let yaml = parse_yaml_frontmatter(&frontmatter)?;

    let name = yaml_scalar(&yaml, "name")
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| dir_name.to_string());

    // Block scalars (`|` / `>`) in YAML 1.2 keep a single trailing
    // newline by default (clip chomping). Trim it so callers see a
    // clean string — matches the previous hand-rolled behavior.
    let description = yaml_scalar(&yaml, "description")
        .map(|s| s.trim_end().to_string())
        .unwrap_or_default();

    // Tags / related can live either at the top level or nested
    // under `metadata.dirge.*`. Try both — top level wins.
    let tags = yaml_list(&yaml["tags"])
        .or_else(|| yaml_list(&yaml["metadata"]["dirge"]["tags"]))
        .unwrap_or_default();
    let related = yaml_list(&yaml["related_skills"])
        .or_else(|| yaml_list(&yaml["metadata"]["dirge"]["related_skills"]))
        .unwrap_or_default();

    Some(SkillSpec {
        name,
        description,
        content: content.to_string(),
        tags,
        related,
        body,
    })
}

/// Validate a skill name. Returns `Ok(())` if the name is valid,
/// `Err(reason)` otherwise.
///
/// Rules (loosened to support real-world names):
/// - non-empty
/// - ≤ MAX_NAME_LEN bytes
/// - no path separators (`/`, `\`)
/// - no null bytes or other control chars (via `char::is_control`)
/// - must not start with `.` (would conflict with dotfiles)
///
/// Otherwise any Unicode letters, digits, hyphens, dots, etc.
/// are accepted — `kebab-case`, `skill.v2`, `日本語スキル` all OK.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Skill name must not be empty".to_string());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!(
            "Skill name too long ({} bytes, max {})",
            name.len(),
            MAX_NAME_LEN
        ));
    }
    if name.starts_with('.') {
        return Err("Skill name must not start with '.'".to_string());
    }
    for c in name.chars() {
        if c == '/' || c == '\\' {
            return Err("Skill name must not contain path separators".to_string());
        }
        if c.is_control() {
            return Err("Skill name must not contain control characters".to_string());
        }
    }
    Ok(())
}

/// Validate total content size. Returns error if over the limit.
pub fn validate_content_size(content: &str) -> Result<(), String> {
    if content.len() > MAX_CONTENT_LEN {
        return Err(format!(
            "Skill content too large ({} chars, max {})",
            content.len(),
            MAX_CONTENT_LEN
        ));
    }
    Ok(())
}

/// Build the frontmatter header for a skill.
#[cfg_attr(not(test), allow(dead_code))]
pub fn build_frontmatter(name: &str, description: &str, tags: &[String]) -> String {
    let mut fm = String::from("---\n");
    fm.push_str(&format!("name: {}\n", name));
    if !description.is_empty() {
        fm.push_str(&format!("description: {}\n", description));
    }
    if !tags.is_empty() {
        fm.push_str("metadata:\n");
        fm.push_str("  dirge:\n");
        fm.push_str("    tags: [");
        fm.push_str(
            &tags
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
        fm.push_str("]\n");
    }
    fm.push_str("---\n\n");
    fm
}

// ── Internal helpers ───────────────────────────────────

/// Split frontmatter from body. Returns `None` if there's no
/// frontmatter or it's malformed. Returns `(frontmatter_text, body_text)`.
fn split_frontmatter(content: &str) -> Option<(String, String)> {
    let content = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;

    let (fm, body) = if let Some(pos) = content.find("\n---") {
        let (a, b) = content.split_at(pos);
        (a.to_string(), b[4..].to_string())
    } else {
        let pos = content.find("\r\n---")?;
        let (a, b) = content.split_at(pos);
        (a.to_string(), b[5..].to_string())
    };

    Some((fm, body))
}

// ── YAML frontmatter parser (yaml-rust2 adapter) ───────
//
// Thin adapter over `yaml_rust2::YamlLoader`. The crate is the
// maintained fork of the abandoned `yaml-rust`, fully YAML 1.2
// compliant — handles block scalars (`|` / `>` with `-`/`+` chomp
// indicators), flow arrays, flow maps, nested mappings, quoted
// strings, etc., natively.

use yaml_rust2::{Yaml, YamlLoader};

/// Parse frontmatter body (between the `---` markers) and return
/// the first document. Returns `None` if the YAML is malformed —
/// callers treat that as "ignore frontmatter".
fn parse_yaml_frontmatter(frontmatter: &str) -> Option<Yaml> {
    let mut docs = YamlLoader::load_from_str(frontmatter).ok()?;
    if docs.is_empty() {
        // Empty frontmatter → behave as an empty mapping so all
        // lookups produce `BadValue` and resolve to defaults.
        return Some(Yaml::Hash(Default::default()));
    }
    Some(docs.remove(0))
}

/// Get a scalar string at the top-level key. Returns `None` if
/// missing or non-scalar.
fn yaml_scalar(yaml: &Yaml, key: &str) -> Option<String> {
    yaml[key].as_str().map(|s| s.to_string())
}

/// Coerce a YAML node to a list-of-strings. Returns:
///   - `None` if the node is missing or null
///   - `Some(vec![])` if the node is an empty sequence
///   - `Some(vec![s])` if the node is a bare scalar (promoted)
///   - `Some(vec)` for sequences (non-string items filtered out)
fn yaml_list(node: &Yaml) -> Option<Vec<String>> {
    match node {
        Yaml::BadValue | Yaml::Null => None,
        Yaml::Array(items) => Some(
            items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
        ),
        Yaml::String(s) if !s.is_empty() => Some(vec![s.clone()]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_name ─────────────────────────────────

    #[test]
    fn valid_name_passes() {
        assert!(validate_name("project-build").is_ok());
        assert!(validate_name("rust-best-practices").is_ok());
        assert!(validate_name("a").is_ok());
    }

    #[test]
    fn empty_name_rejected() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn uppercase_name_accepted() {
        // Loosened validation: mixed case is allowed.
        assert!(validate_name("Project-Build").is_ok());
    }

    #[test]
    fn underscore_and_space_accepted() {
        // Loosened: underscores and spaces are both legal Unicode
        // identifier-ish characters and not banned by the new
        // rules. Path separators and control chars are still
        // rejected (see dedicated tests below).
        assert!(validate_name("project_build").is_ok());
        assert!(validate_name("project build").is_ok());
    }

    #[test]
    fn leading_trailing_hyphen_accepted() {
        // Loosened: only `.` at the start is forbidden.
        assert!(validate_name("-project").is_ok());
        assert!(validate_name("project-").is_ok());
    }

    #[test]
    fn too_long_name_rejected() {
        // The cap is in bytes — 257 ASCII bytes > 256.
        let long = "a".repeat(MAX_NAME_LEN + 1);
        assert!(validate_name(&long).is_err());
    }

    #[test]
    fn skill_name_accepts_unicode() {
        assert!(validate_name("日本語スキル").is_ok());
        assert!(validate_name("café-skill").is_ok());
    }

    #[test]
    fn skill_name_accepts_dots_after_first_char() {
        assert!(validate_name("skill.v2").is_ok());
        assert!(validate_name("a.b.c").is_ok());
    }

    #[test]
    fn skill_name_rejects_path_separator() {
        assert!(validate_name("foo/bar").is_err());
        assert!(validate_name("foo\\bar").is_err());
    }

    #[test]
    fn skill_name_rejects_control_chars() {
        assert!(validate_name("foo\x01bar").is_err());
        assert!(validate_name("foo\0bar").is_err());
        assert!(validate_name("foo\nbar").is_err());
    }

    #[test]
    fn skill_name_rejects_leading_dot() {
        assert!(validate_name(".hidden").is_err());
        assert!(validate_name(".").is_err());
    }

    // ── parse_skill_spec ──────────────────────────────

    #[test]
    fn parse_valid_skill() {
        let content = "---\nname: project-build\ndescription: Build commands\n---\n\nRun `cargo build` to compile.\n";
        let spec = parse_skill_spec(content, "fallback").unwrap();
        assert_eq!(spec.name, "project-build");
        assert_eq!(spec.description, "Build commands");
        assert!(spec.body.contains("cargo build"));
    }

    #[test]
    fn parse_falls_back_to_dir_name() {
        let content = "---\ndescription: no name field\n---\n\nbody here\n";
        let spec = parse_skill_spec(content, "dir-name").unwrap();
        assert_eq!(spec.name, "dir-name");
    }

    #[test]
    fn parse_rejects_empty_body() {
        let content = "---\nname: test\n---\n   \n";
        assert!(parse_skill_spec(content, "dir").is_none());
    }

    #[test]
    fn parse_no_frontmatter_returns_none() {
        assert!(parse_skill_spec("just body", "dir").is_none());
    }

    #[test]
    fn parse_extracts_tags() {
        let content =
            "---\nname: s\nmetadata:\n  dirge:\n    tags: [build, rust, cargo]\n---\n\nbody\n";
        let spec = parse_skill_spec(content, "s").unwrap();
        assert_eq!(spec.tags, vec!["build", "rust", "cargo"]);
    }

    #[test]
    fn frontmatter_with_empty_name_defaults_to_dir() {
        let content = "---\nname:\ndescription: desc\n---\n\nbody\n";
        let spec = parse_skill_spec(content, "dir-name").unwrap();
        assert_eq!(spec.name, "dir-name");
    }

    // ── validate_content_size ─────────────────────────

    #[test]
    fn content_size_under_limit() {
        assert!(validate_content_size("short").is_ok());
    }

    #[test]
    fn content_size_over_limit() {
        let big = "x".repeat(100_001);
        assert!(validate_content_size(&big).is_err());
    }

    // ── build_frontmatter ─────────────────────────────

    #[test]
    fn build_frontmatter_includes_name_and_description() {
        let fm = build_frontmatter("my-skill", "Does things", &[]);
        assert!(fm.contains("name: my-skill"));
        assert!(fm.contains("description: Does things"));
        assert!(fm.starts_with("---\n"));
        assert!(fm.ends_with("---\n\n"));
    }

    #[test]
    fn build_frontmatter_includes_tags() {
        let fm = build_frontmatter("s", "", &["rust".into(), "build".into()]);
        assert!(fm.contains("tags: [rust, build]"));
    }

    // ── YAML frontmatter parser ───────────────────────

    #[test]
    fn yaml_empty_list_for_missing_key() {
        let yaml = parse_yaml_frontmatter("name: foo\n").unwrap();
        assert!(yaml_list(&yaml["tags"]).is_none());
    }

    #[test]
    fn yaml_single_scalar_promoted_to_list() {
        let yaml = parse_yaml_frontmatter("tags: rust\n").unwrap();
        assert_eq!(yaml_list(&yaml["tags"]), Some(vec!["rust".to_string()]));
    }

    #[test]
    fn parse_skill_spec_handles_multi_line_description() {
        let content =
            "---\nname: s\ndescription: |\n  Multi-line text\n  continues here\n---\n\nbody\n";
        let spec = parse_skill_spec(content, "s").unwrap();
        assert_eq!(spec.description, "Multi-line text\ncontinues here");
    }

    #[test]
    fn parse_skill_spec_handles_folded_description() {
        let content = "---\nname: s\ndescription: >\n  First line\n  second line\n---\n\nbody\n";
        let spec = parse_skill_spec(content, "s").unwrap();
        assert_eq!(spec.description, "First line second line");
    }

    #[test]
    fn parse_skill_spec_handles_nested_map() {
        // `tools: { allowed: [read], denied: [write] }` — flow map.
        // The parser must descend without choking; we don't surface
        // tools in `SkillSpec`, but adjacent fields must still parse.
        let content = "---\nname: s\ntools: { allowed: [read], denied: [write] }\ndescription: ok\n---\n\nbody\n";
        let spec = parse_skill_spec(content, "s").unwrap();
        assert_eq!(spec.name, "s");
        assert_eq!(spec.description, "ok");
    }

    #[test]
    fn parse_skill_spec_handles_flow_array() {
        let content = "---\nname: s\ntags: [a, b, c]\n---\n\nbody\n";
        let spec = parse_skill_spec(content, "s").unwrap();
        assert_eq!(spec.tags, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_skill_spec_handles_quoted_string_with_colon() {
        let content = "---\nname: s\ndescription: \"foo: bar: baz\"\n---\n\nbody\n";
        let spec = parse_skill_spec(content, "s").unwrap();
        assert_eq!(spec.description, "foo: bar: baz");
    }

    #[test]
    fn parse_skill_spec_handles_block_list() {
        let content = "---\nname: s\ntags:\n  - alpha\n  - beta\n  - gamma\n---\n\nbody\n";
        let spec = parse_skill_spec(content, "s").unwrap();
        assert_eq!(spec.tags, vec!["alpha", "beta", "gamma"]);
    }
}
