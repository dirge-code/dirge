//! Addon manifest parsing.
//!
//! A manifest is a single EDN map:
//!
//! ```edn
//! {:addon/id      my.addon
//!  :addon/init-ns my.addon.core
//!  :addon/init-fn addon-ctor}
//! ```
//!
//! `:addon/id` names the addon, `:addon/init-ns` is the namespace to load
//! and `:addon/init-fn` the zero-argument constructor in it that returns
//! the addon instance. An optional `:addon/protocol-ns` names the IAddon
//! protocol namespace the addon implements, when it is not the embedded
//! `hive-addon.protocol`. Values may be written as symbols, strings or
//! keywords. A qualified `:addon/init-fn` (`ns/fn`) is accepted as long as
//! its namespace matches `:addon/init-ns`. Unknown keys are ignored so
//! manifests can carry data for other hosts.

use std::path::Path;

use cljrs_reader::{Form, FormKind, Parser};

/// A parsed addon manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonManifest {
    /// Unique addon id.
    pub id: String,
    /// Namespace to load.
    pub init_ns: String,
    /// Unqualified name of the constructor var in `init_ns`.
    pub init_fn: String,
    /// The IAddon protocol namespace the addon implements, when declared.
    pub protocol_ns: Option<String>,
}

/// Why a manifest could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest is not valid EDN: {0}")]
    Read(String),
    #[error("manifest must contain exactly one top-level form, found {0}")]
    FormCount(usize),
    #[error("manifest must be an EDN map")]
    NotAMap,
    #[error("manifest is missing required key :{0}")]
    Missing(&'static str),
    #[error("manifest key :{key} must be a non-empty symbol, string or keyword")]
    BadValue { key: &'static str },
    #[error(
        "manifest :addon/init-fn namespace `{fn_ns}` does not match :addon/init-ns `{init_ns}`"
    )]
    NamespaceMismatch { fn_ns: String, init_ns: String },
    #[error("cannot read manifest {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

const KEY_ID: &str = "addon/id";
const KEY_INIT_NS: &str = "addon/init-ns";
const KEY_INIT_FN: &str = "addon/init-fn";
const KEY_PROTOCOL_NS: &str = "addon/protocol-ns";

impl AddonManifest {
    /// Parse a manifest from EDN source. `origin` labels read errors.
    pub fn parse(src: &str, origin: &str) -> Result<Self, ManifestError> {
        let mut parser = Parser::new(src.to_string(), origin.to_string());
        let forms = parser
            .parse_all()
            .map_err(|e| ManifestError::Read(e.to_string()))?;
        if forms.len() != 1 {
            return Err(ManifestError::FormCount(forms.len()));
        }
        let entries = forms[0].as_map().ok_or(ManifestError::NotAMap)?;

        let id = required(entries, KEY_ID)?;
        let init_ns = required(entries, KEY_INIT_NS)?;
        let init_fn_raw = required(entries, KEY_INIT_FN)?;
        let protocol_ns = optional(entries, KEY_PROTOCOL_NS)?;

        let init_fn = match init_fn_raw.split_once('/') {
            Some((fn_ns, name)) if !fn_ns.is_empty() && !name.is_empty() => {
                if fn_ns != init_ns {
                    return Err(ManifestError::NamespaceMismatch {
                        fn_ns: fn_ns.to_string(),
                        init_ns,
                    });
                }
                name.to_string()
            }
            Some(_) => return Err(ManifestError::BadValue { key: KEY_INIT_FN }),
            None => init_fn_raw,
        };

        Ok(Self {
            id,
            init_ns,
            init_fn,
            protocol_ns,
        })
    }

    /// Read and parse a manifest file.
    pub fn from_path(path: &Path) -> Result<Self, ManifestError> {
        let src = std::fs::read_to_string(path).map_err(|source| ManifestError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&src, &path.display().to_string())
    }
}

/// Look up `key` (a keyword without the leading colon) in a flat
/// `[k1 v1 k2 v2 ...]` map body and return its value as text.
fn required(entries: &[Form], key: &'static str) -> Result<String, ManifestError> {
    optional(entries, key)?.ok_or(ManifestError::Missing(key))
}

/// [`required`] for a key that may be absent.
fn optional(entries: &[Form], key: &'static str) -> Result<Option<String>, ManifestError> {
    let Some(value) = entries
        .as_chunks::<2>()
        .0
        .iter()
        .find(|[k, _]| k.as_keyword() == Some(key))
        .map(|[_, v]| v)
    else {
        return Ok(None);
    };
    let text = match &value.kind {
        FormKind::Symbol(s) | FormKind::Str(s) | FormKind::Keyword(s) => s.as_str(),
        _ => return Err(ManifestError::BadValue { key }),
    };
    if text.trim().is_empty() {
        return Err(ManifestError::BadValue { key });
    }
    Ok(Some(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Result<AddonManifest, ManifestError> {
        AddonManifest::parse(src, "test.edn")
    }

    #[test]
    fn parses_symbol_manifest() {
        let m = parse(
            "{:addon/id example.probe\n :addon/init-ns example.probe.addon\n :addon/init-fn addon-ctor}",
        )
        .unwrap();
        assert_eq!(
            m,
            AddonManifest {
                id: "example.probe".into(),
                init_ns: "example.probe.addon".into(),
                init_fn: "addon-ctor".into(),
                protocol_ns: None,
            }
        );
    }

    #[test]
    fn reads_a_declared_protocol_namespace() {
        let m = parse("{:addon/id a :addon/init-ns a.core :addon/init-fn ctor :addon/protocol-ns other.protocol}")
            .unwrap();
        assert_eq!(m.protocol_ns.as_deref(), Some("other.protocol"));
        assert!(matches!(
            parse("{:addon/id a :addon/init-ns a.core :addon/init-fn ctor :addon/protocol-ns 1}"),
            Err(ManifestError::BadValue {
                key: "addon/protocol-ns"
            })
        ));
    }

    #[test]
    fn accepts_strings_keywords_and_ignores_unknown_keys() {
        let m = parse(
            r#"{:addon/id "example" :addon/init-ns :example.core
                :addon/init-fn "make" :addon/config {:x 1} :other [1 2 3]}"#,
        )
        .unwrap();
        assert_eq!(m.id, "example");
        assert_eq!(m.init_ns, "example.core");
        assert_eq!(m.init_fn, "make");
    }

    #[test]
    fn qualified_init_fn_must_match_ns() {
        let m = parse("{:addon/id a :addon/init-ns a.core :addon/init-fn a.core/ctor}").unwrap();
        assert_eq!(m.init_fn, "ctor");
        let err =
            parse("{:addon/id a :addon/init-ns a.core :addon/init-fn b.core/ctor}").unwrap_err();
        assert!(matches!(err, ManifestError::NamespaceMismatch { .. }));
    }

    #[test]
    fn missing_key_is_reported() {
        let err = parse("{:addon/id a :addon/init-ns a.core}").unwrap_err();
        assert!(matches!(err, ManifestError::Missing("addon/init-fn")));
    }

    #[test]
    fn rejects_non_map_and_bad_values() {
        assert!(matches!(parse("[1 2]"), Err(ManifestError::NotAMap)));
        assert!(matches!(parse("{} {}"), Err(ManifestError::FormCount(2))));
        assert!(matches!(
            parse("{:addon/id 42 :addon/init-ns a :addon/init-fn f}"),
            Err(ManifestError::BadValue { key: "addon/id" })
        ));
        assert!(matches!(
            parse("{:addon/id \"  \" :addon/init-ns a :addon/init-fn f}"),
            Err(ManifestError::BadValue { key: "addon/id" })
        ));
        assert!(matches!(parse("{:addon/id"), Err(ManifestError::Read(_))));
    }

    #[test]
    fn reads_from_file() {
        let dir = std::env::temp_dir().join(format!("dirge-addon-manifest-{}", std::process::id()));
        // Clear first: a recycled pid would otherwise inherit a previous run's files.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("addon.edn");
        std::fs::write(
            &path,
            "{:addon/id x :addon/init-ns x.y :addon/init-fn ctor}",
        )
        .unwrap();
        assert_eq!(AddonManifest::from_path(&path).unwrap().id, "x");
        assert!(matches!(
            AddonManifest::from_path(&dir.join("missing.edn")),
            Err(ManifestError::Io { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
