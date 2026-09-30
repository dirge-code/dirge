//! Where the feed lives: the `panel_feed` config block resolved to an
//! endpoint `{url, token}`.
//!
//! Two sources, both re-read on every (re)connect so a producer that
//! restarts on a new port with a new token is picked up:
//! - a discovery file `<dir>/dirge.json` = `{"url": .., "token": ..}`,
//!   where a relative `discovery_dir` is taken under
//!   `$XDG_RUNTIME_DIR`;
//! - an explicit `url` plus an optional `token_file`.
//!
//! Any file carrying the token must be a regular file (not a
//! symlink), owned by the current user and not readable or writable
//! by group or others; otherwise it is refused. The token is never
//! logged: [`Endpoint`]'s `Debug` redacts it.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// File name looked up inside the discovery directory.
pub const DISCOVERY_FILE: &str = "dirge.json";
/// Largest discovery / token file read.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The `panel_feed` config block. Absent or empty = off.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PanelFeedConfig {
    /// Explicit kill switch; defaults to on when a source is set.
    pub enabled: Option<bool>,
    /// Directory holding `dirge.json`. Relative paths resolve under
    /// `$XDG_RUNTIME_DIR`.
    pub discovery_dir: Option<String>,
    /// Explicit feed base URL (takes precedence over discovery).
    pub url: Option<String>,
    /// File holding the token for `url` (same permission rules).
    pub token_file: Option<String>,
    /// Accept `loop/*` ops: the producer's directives are injected into
    /// the running agent loop (see `agent_loop::loop_inbox`). Defaults to
    /// on when the feed is on; `false` keeps the feed display-only.
    #[serde(rename = "loop")]
    pub loop_directives: Option<bool>,
}

impl PanelFeedConfig {
    /// Whether the feed advertises and acts on `loop/*` ops.
    pub fn loop_enabled(&self) -> bool {
        self.loop_directives != Some(false)
    }
}

/// Where to find the endpoint, resolved from config + environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    DiscoveryFile(PathBuf),
    Explicit {
        url: String,
        token_file: Option<PathBuf>,
    },
}

impl PanelFeedConfig {
    /// `None` when the feed is disabled or has no source.
    pub fn source(&self, runtime_dir: Option<&Path>) -> Option<Source> {
        if self.enabled == Some(false) {
            return None;
        }
        fn nonblank(s: &Option<String>) -> Option<&str> {
            s.as_deref().map(str::trim).filter(|s| !s.is_empty())
        }
        if let Some(url) = nonblank(&self.url) {
            return Some(Source::Explicit {
                url: url.to_string(),
                token_file: nonblank(&self.token_file).map(PathBuf::from),
            });
        }
        let dir = PathBuf::from(nonblank(&self.discovery_dir)?);
        let dir = if dir.is_absolute() {
            dir
        } else {
            runtime_dir?.join(dir)
        };
        Some(Source::DiscoveryFile(dir.join(DISCOVERY_FILE)))
    }
}

/// A resolved feed endpoint.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Base URL; the client appends `/events` and `/reply`.
    pub url: String,
    pub token: Option<String>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("url", &self.url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("{path}: not found")]
    Missing { path: PathBuf },
    #[error("{path}: {why}; refusing a file that carries a token")]
    Insecure { path: PathBuf, why: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: not a discovery document ({why})")]
    Malformed { path: PathBuf, why: String },
}

impl DiscoveryError {
    /// Missing files are the normal "producer not running" state and
    /// are retried quietly; everything else is worth a warning once.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing { .. })
    }
}

/// Read a token-bearing file after checking it is private.
pub fn read_private(path: &Path) -> Result<String, DiscoveryError> {
    use std::io::Read;
    let io = |source: std::io::Error| {
        if source.kind() == std::io::ErrorKind::NotFound {
            DiscoveryError::Missing {
                path: path.to_path_buf(),
            }
        } else {
            DiscoveryError::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    };
    let insecure = |why: String| DiscoveryError::Insecure {
        path: path.to_path_buf(),
        why,
    };
    let link = std::fs::symlink_metadata(path).map_err(io)?;
    if link.file_type().is_symlink() {
        return Err(insecure("is a symlink".into()));
    }
    let file = open_nofollow(path).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(insecure("not a regular file".into()));
    }
    check_private(&meta).map_err(insecure)?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(insecure(format!("larger than {MAX_FILE_BYTES} bytes")));
    }
    let mut text = String::new();
    file.take(MAX_FILE_BYTES)
        .read_to_string(&mut text)
        .map_err(io)?;
    Ok(text)
}

#[cfg(unix)]
fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

#[cfg(unix)]
fn check_private(meta: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    if meta.uid() != uid {
        return Err(format!("owned by uid {}, not {uid}", meta.uid()));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!("mode {mode:04o} is not owner-only (want 0600)"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_meta: &std::fs::Metadata) -> Result<(), String> {
    Err("owner-only permissions cannot be verified on this platform".into())
}

#[derive(Deserialize)]
struct DiscoveryDoc {
    url: String,
    token: Option<String>,
}

/// Parse a discovery document (pure).
pub fn parse_discovery(text: &str) -> Result<Endpoint, String> {
    let doc: DiscoveryDoc = serde_json::from_str(text).map_err(|e| {
        // serde's message can quote input; keep only the category so
        // a token never reaches a log line.
        format!("invalid JSON ({:?})", e.classify())
    })?;
    let url = doc.url.trim().trim_end_matches('/').to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("url must be http:// or https://".into());
    }
    Ok(Endpoint {
        url,
        token: doc.token.filter(|t| !t.is_empty()),
    })
}

/// Resolve the endpoint now (file I/O).
pub fn resolve(source: &Source) -> Result<Endpoint, DiscoveryError> {
    match source {
        Source::DiscoveryFile(path) => {
            let text = read_private(path)?;
            parse_discovery(&text).map_err(|why| DiscoveryError::Malformed {
                path: path.clone(),
                why,
            })
        }
        Source::Explicit { url, token_file } => {
            let token = match token_file {
                Some(p) => Some(read_private(p)?.trim().to_string()).filter(|t| !t.is_empty()),
                None => None,
            };
            Ok(Endpoint {
                url: url.trim_end_matches('/').to_string(),
                token,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dirge-panel-feed-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_mode(path: &Path, text: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn config_is_off_by_default_and_resolves_sources() {
        let rt = Path::new("/run/user/1000");
        assert_eq!(PanelFeedConfig::default().source(Some(rt)), None);
        let rel = PanelFeedConfig {
            discovery_dir: Some("feeds".into()),
            ..Default::default()
        };
        assert_eq!(
            rel.source(Some(rt)),
            Some(Source::DiscoveryFile(rt.join("feeds").join(DISCOVERY_FILE)))
        );
        assert_eq!(rel.source(None), None, "relative dir needs a runtime dir");
        let abs = PanelFeedConfig {
            discovery_dir: Some("/srv/feed".into()),
            ..Default::default()
        };
        assert_eq!(
            abs.source(None),
            Some(Source::DiscoveryFile(PathBuf::from("/srv/feed/dirge.json")))
        );
        let off = PanelFeedConfig {
            enabled: Some(false),
            ..rel.clone()
        };
        assert_eq!(off.source(Some(rt)), None);
        let explicit = PanelFeedConfig {
            url: Some("http://127.0.0.1:9/x".into()),
            token_file: Some("/tmp/t".into()),
            ..rel
        };
        assert_eq!(
            explicit.source(Some(rt)),
            Some(Source::Explicit {
                url: "http://127.0.0.1:9/x".into(),
                token_file: Some(PathBuf::from("/tmp/t")),
            })
        );
    }

    #[test]
    fn discovery_document_parses_and_validates() {
        let ep =
            parse_discovery(r#"{"url":"http://127.0.0.1:5/p/","token":"s3cret","pid":1}"#).unwrap();
        assert_eq!(ep.url, "http://127.0.0.1:5/p");
        assert_eq!(ep.token.as_deref(), Some("s3cret"));
        assert!(parse_discovery(r#"{"url":"file:///etc"}"#).is_err());
        let err = parse_discovery(r#"{"url": 1, "token":"s3cret"}"#).unwrap_err();
        assert!(!err.contains("s3cret"));
    }

    #[test]
    fn endpoint_debug_redacts_token() {
        let ep = Endpoint {
            url: "http://h".into(),
            token: Some("s3cret".into()),
        };
        let dbg = format!("{ep:?}");
        assert!(!dbg.contains("s3cret"), "{dbg}");
        assert!(dbg.contains("redacted"));
    }

    #[cfg(unix)]
    #[test]
    fn owner_only_file_is_accepted() {
        let dir = temp_dir("ok");
        let path = dir.join(DISCOVERY_FILE);
        write_mode(&path, r#"{"url":"http://127.0.0.1:1","token":"t"}"#, 0o600);
        let ep = resolve(&Source::DiscoveryFile(path)).unwrap();
        assert_eq!(ep.token.as_deref(), Some("t"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn group_or_world_accessible_file_is_refused() {
        let dir = temp_dir("perm");
        let path = dir.join(DISCOVERY_FILE);
        for mode in [0o644, 0o640, 0o604, 0o660] {
            write_mode(&path, r#"{"url":"http://127.0.0.1:1","token":"t"}"#, mode);
            let err = resolve(&Source::DiscoveryFile(path.clone())).unwrap_err();
            assert!(
                matches!(err, DiscoveryError::Insecure { .. }),
                "mode {mode:o}: {err}"
            );
            assert!(!err.to_string().contains("\"t\""));
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_missing_and_directory_are_refused() {
        let dir = temp_dir("link");
        let real = dir.join("real.json");
        write_mode(&real, r#"{"url":"http://127.0.0.1:1"}"#, 0o600);
        let link = dir.join(DISCOVERY_FILE);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            resolve(&Source::DiscoveryFile(link)),
            Err(DiscoveryError::Insecure { .. })
        ));
        let missing = resolve(&Source::DiscoveryFile(dir.join("nope.json"))).unwrap_err();
        assert!(missing.is_missing());
        assert!(read_private(&dir).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn explicit_url_with_private_token_file() {
        let dir = temp_dir("tok");
        let tok = dir.join("token");
        write_mode(&tok, "abc\n", 0o600);
        let ep = resolve(&Source::Explicit {
            url: "http://127.0.0.1:2/".into(),
            token_file: Some(tok.clone()),
        })
        .unwrap();
        assert_eq!(ep.url, "http://127.0.0.1:2");
        assert_eq!(ep.token.as_deref(), Some("abc"));
        write_mode(&tok, "abc\n", 0o644);
        assert!(
            resolve(&Source::Explicit {
                url: "http://127.0.0.1:2".into(),
                token_file: Some(tok),
            })
            .is_err()
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
