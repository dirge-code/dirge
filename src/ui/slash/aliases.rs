//! Config-driven aliases for built-in slash commands.
//!
//! A user can rename a built-in command (or add a short alias) via the
//! `slash_aliases` config map — `{"exit": "quit"}` makes `/exit` run
//! `/quit`. The raw config is just data; this module resolves it into an
//! [`AliasMap`] and warns about targets that don't name a known built-in
//! (likely typos). Expansion happens at the single `handle_slash` call
//! site, so aliases never reach the dispatch `match` — they are not
//! built-ins and don't belong in `slash_command_names()`.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::config::Config;

/// Resolved slash-command aliases: alias name (no leading `/`) -> target
/// command (WITH leading `/`, e.g. `"/quit"`). Built from the user's
/// `slash_aliases` config by [`build_alias_map`].
#[derive(Debug, Default, Clone)]
pub(crate) struct AliasMap {
    map: HashMap<String, String>,
}

impl AliasMap {
    /// Alias names (without leading `/`) — for tab-completion registration.
    /// Gated to `slash-completion` — its only caller
    /// (`register_alias_commands` in `ui::mod`) is, so without the feature
    /// this would be dead code on a `--no-default-features` build.
    #[cfg(feature = "slash-completion")]
    pub(crate) fn names(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }
}

/// Resolve the user's `slash_aliases` config into an [`AliasMap`] plus
/// startup warnings. Both alias keys and target commands are normalized
/// to tolerate a leading `/` on either side (alias key stored without,
/// target stored with). A target that isn't a known built-in command
/// produces a warning (likely a typo) but is still stored — plugin
/// commands can't be validated here and resolve naturally at dispatch.
pub(crate) fn build_alias_map(cfg: &Config) -> (AliasMap, Vec<String>) {
    let mut map = HashMap::new();
    let mut warnings = Vec::new();
    let Some(raw) = cfg.slash_aliases.as_ref() else {
        return (AliasMap { map }, warnings);
    };
    for (alias, target) in raw {
        let alias_key = alias.trim_start_matches('/').to_string();
        let target_cmd = if target.starts_with('/') {
            target.clone()
        } else {
            format!("/{target}")
        };
        if alias_key.is_empty() {
            // A bare "/" must never dispatch: an empty alias key would make
            // "/" expand to the target (e.g. quit the app). Drop it.
            warnings.push(format!(
                "slash_aliases: {:?} -> {:?}: empty alias key is ignored \
                 (it would make bare \"/\" run {target_cmd})",
                alias, target,
            ));
            continue;
        }
        if !super::is_known_slash_command(&target_cmd) {
            warnings.push(format!(
                "slash_aliases: {:?} -> {:?}: {:?} is not a known built-in command \
                 (see /help); it will be passed through but may not resolve",
                alias, target, target_cmd,
            ));
        }
        // Shadowing a real built-in is allowed (the doc'd use case is
        // renaming one), but it's also a common footgun — `{"quit":
        // "clear"}` silently makes `/quit` run `/clear` and locks the user
        // out of the real command. Warn so it's a deliberate choice, not a
        // typo that swallows a command.
        if super::is_known_slash_command(&format!("/{alias_key}")) {
            warnings.push(format!(
                "slash_aliases: {:?} -> {:?}: alias key {:?} shadows the built-in \
                 /{alias_key}, which can no longer be run by that name",
                alias, target, alias_key,
            ));
        }
        map.insert(alias_key, target_cmd);
    }
    (AliasMap { map }, warnings)
}

/// Expand a leading alias in `text`, if any. Only the first token (the
/// command name) is rewritten to the alias's target; any arguments the
/// user typed are passed through verbatim. Non-alias input (an unknown
/// command, text without a leading `/`, or empty) is returned unchanged
/// (borrowed, no allocation).
pub(crate) fn expand_alias<'a>(text: &'a str, aliases: &AliasMap) -> Cow<'a, str> {
    let Some(rest) = text.strip_prefix('/') else {
        return Cow::Borrowed(text);
    };
    let (name, args) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let Some(target) = aliases.map.get(name) else {
        return Cow::Borrowed(text);
    };
    if args.is_empty() {
        Cow::Owned(target.clone())
    } else {
        Cow::Owned(format!("{target}{args}"))
    }
}

/// User-facing alias lines for `/help`: one `"/alias -> /target"` per
/// configured alias, sorted by alias name. The empty-key footgun is
/// excluded (same normalization + rejection as [`build_alias_map`]).
pub(crate) fn display_entries(cfg: &Config) -> Vec<String> {
    let (am, _warnings) = build_alias_map(cfg);
    let mut entries: Vec<(String, String)> = am.map.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
        .into_iter()
        .map(|(alias, target)| format!("/{alias} -> {target}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_slash_aliases_flat_map() {
        let cfg: Config =
            serde_json::from_str(r#"{ "slash_aliases": { "exit": "quit" } }"#).unwrap();
        let map = cfg.slash_aliases.expect("slash_aliases should deserialize");
        assert_eq!(map.get("exit").map(String::as_str), Some("quit"));

        let cfg: Config = serde_json::from_str(r"{}").unwrap();
        assert!(cfg.slash_aliases.is_none(), "absent key -> None");
    }

    #[test]
    fn build_alias_map_normalizes_leading_slashes() {
        // key with slash + target without, and key without + target with,
        // both normalize to `exit`/`q` -> `/quit`.
        let cfg: Config =
            serde_json::from_str(r#"{ "slash_aliases": { "/exit": "quit", "q": "/quit" } }"#)
                .unwrap();
        let (am, warnings) = build_alias_map(&cfg);
        assert_eq!(am.map.get("exit").map(String::as_str), Some("/quit"));
        assert_eq!(am.map.get("q").map(String::as_str), Some("/quit"));
        assert!(
            warnings.is_empty(),
            "quit is a known built-in: {warnings:?}"
        );
    }

    #[test]
    fn build_alias_map_warns_on_unknown_target() {
        let cfg: Config =
            serde_json::from_str(r#"{ "slash_aliases": { "exit": "qiut" } }"#).unwrap();
        let (am, warnings) = build_alias_map(&cfg);
        // Still stored — passed through (might be a plugin command).
        assert_eq!(am.map.get("exit").map(String::as_str), Some("/qiut"));
        assert_eq!(warnings.len(), 1, "exactly one warning: {warnings:?}");
        assert!(
            warnings[0].contains("/qiut") && warnings[0].contains("not a known built-in"),
            "warning should name the target: {}",
            warnings[0],
        );
    }

    #[test]
    fn build_alias_map_no_warnings_for_known_targets() {
        let cfg: Config =
            serde_json::from_str(r#"{ "slash_aliases": { "bye": "quit", "cls": "clear" } }"#)
                .unwrap();
        let (am, warnings) = build_alias_map(&cfg);
        assert_eq!(am.map.get("bye").map(String::as_str), Some("/quit"));
        assert_eq!(am.map.get("cls").map(String::as_str), Some("/clear"));
        assert!(
            warnings.is_empty(),
            "both targets are known built-ins: {warnings:?}"
        );
    }

    #[test]
    fn build_alias_map_warns_when_key_shadows_builtin() {
        // `/quit` is a built-in; aliasing the key `quit` to `/clear`
        // shadows it. Still stored (intended use case is renaming), but it
        // must warn so it isn't a silent footgun.
        let cfg: Config =
            serde_json::from_str(r#"{ "slash_aliases": { "quit": "clear" } }"#).unwrap();
        let (am, warnings) = build_alias_map(&cfg);
        assert_eq!(am.map.get("quit").map(String::as_str), Some("/clear"));
        assert_eq!(warnings.len(), 1, "exactly one warning: {warnings:?}");
        assert!(
            warnings[0].contains("shadows") && warnings[0].contains("/quit"),
            "warning should name the shadowed built-in: {}",
            warnings[0],
        );
    }

    #[test]
    fn build_alias_map_absent_config_is_empty() {
        let cfg = Config::default();
        let (am, warnings) = build_alias_map(&cfg);
        assert!(am.map.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn build_alias_map_empty_map_is_empty() {
        let cfg: Config = serde_json::from_str(r#"{ "slash_aliases": {} }"#).unwrap();
        let (am, warnings) = build_alias_map(&cfg);
        assert!(am.map.is_empty());
        assert!(warnings.is_empty());
    }

    fn map_from(pairs: impl IntoIterator<Item = (&'static str, &'static str)>) -> AliasMap {
        AliasMap {
            map: pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn expand_alias_rewrites_command_name_only() {
        let am = map_from([("exit", "/quit")]);
        assert_eq!(expand_alias("/exit", &am), "/quit");
        assert_eq!(expand_alias("/exit keep recent", &am), "/quit keep recent");
        // whitespace between command and args is preserved
        assert_eq!(expand_alias("/exit  two", &am), "/quit  two");
    }

    #[test]
    fn expand_alias_passes_through_unaliased_commands() {
        let am = map_from([("exit", "/quit")]);
        assert_eq!(expand_alias("/model gpt", &am), "/model gpt");
        // the target itself isn't an alias key -> no rewrite
        assert_eq!(expand_alias("/quit", &am), "/quit");
    }

    #[test]
    fn expand_alias_passes_through_non_slash_and_empty() {
        let am = map_from([("exit", "/quit")]);
        assert_eq!(expand_alias("hello world", &am), "hello world");
        assert_eq!(expand_alias("", &am), "");
    }

    #[test]
    fn expand_alias_empty_map_passes_through() {
        let am = AliasMap::default();
        assert_eq!(expand_alias("/exit", &am), "/exit");
        assert_eq!(expand_alias("/exit x", &am), "/exit x");
    }

    #[test]
    fn build_alias_map_rejects_empty_alias_key() {
        // An empty alias key is a footgun: it would make bare "/" expand to
        // the target (e.g. quit the app). Reject it with a warning; keep the
        // valid entry.
        let cfg: Config =
            serde_json::from_str(r#"{ "slash_aliases": { "": "quit", "exit": "quit" } }"#).unwrap();
        let (am, warnings) = build_alias_map(&cfg);
        assert!(!am.map.contains_key(""), "empty alias key must be dropped");
        assert_eq!(am.map.get("exit").map(String::as_str), Some("/quit"));
        assert!(
            warnings.iter().any(|w| w.contains("empty")),
            "should warn about the dropped empty key: {warnings:?}",
        );
    }

    #[test]
    fn display_entries_lists_normalized_aliases_sorted() {
        let cfg: Config = serde_json::from_str(
            r#"{ "slash_aliases": { "bye": "/quit", "cls": "clear", "": "quit" } }"#,
        )
        .unwrap();
        // leading-slash normalized, empty key dropped, sorted by alias name
        assert_eq!(
            display_entries(&cfg),
            vec!["/bye -> /quit", "/cls -> /clear"]
        );
    }

    #[test]
    fn display_entries_empty_when_no_aliases() {
        assert!(display_entries(&Config::default()).is_empty());
        let cfg: Config = serde_json::from_str(r#"{ "slash_aliases": {} }"#).unwrap();
        assert!(display_entries(&cfg).is_empty());
    }
}
