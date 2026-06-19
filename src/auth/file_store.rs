//! Shared helpers for the two OAuth credential stores (Anthropic and
//! OpenAI). The on-disk JSON shapes and paths stay provider-specific
//! (external-compat-dictated); only the narrow, genuinely-duplicated
//! mechanics live here: atomic 0600 writes, JSON object loading, epoch
//! expiry comparison, and the OpenAI account-id alias list.

use std::path::Path;

use serde_json::{Map, Value};

/// Account-id aliases seen across Codex/ChatGPT credential payloads.
/// `account_id` is canonical; the rest are legacy/variant spellings.
pub(crate) const OPENAI_ACCOUNT_ID_ALIASES: &[&str] = &[
    "account_id",
    "chatgpt_account_id",
    "chatgptAccountId",
    "chatgpt_account",
    "accountId",
];

/// `now >= expires_at` — a token at exactly its expiry instant is expired.
pub(crate) fn epoch_ms_is_expired(expires_at_ms: i64, now_ms: i64) -> bool {
    now_ms >= expires_at_ms
}

/// Pull the first present account-id value (canonical key first, then
/// aliases) out of a JSON object, trimmed and non-empty. Returns `None`
/// for non-objects or when no alias holds a usable string.
pub(crate) fn extract_account_id(obj: &Value) -> Option<String> {
    let map = obj.as_object()?;
    OPENAI_ACCOUNT_ID_ALIASES.iter().find_map(|key| {
        map.get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

/// Read and parse a JSON object from `path`. `Ok(None)` when the file is
/// absent; errors on I/O failure, invalid JSON, or a non-object top level.
///
/// Part of the shared store API. The two concrete stores keep their own
/// typed-error load paths (Anthropic recurses for nested keys; OpenAI maps
/// to redacting `AuthStoreError` variants), so this generic loader is
/// currently only exercised by tests — kept for parity and future callers.
#[allow(dead_code)]
pub(crate) fn load_json(path: &Path) -> anyhow::Result<Option<Map<String, Value>>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(source.into()),
    };
    let value: Value = serde_json::from_str(&contents)?;
    match value {
        Value::Object(document) => Ok(Some(document)),
        _ => anyhow::bail!("top-level auth document must be a JSON object"),
    }
}

/// Atomically write `value` as pretty JSON to `path` with owner-only
/// (0600) permissions: ensure the parent dir exists at 0700, tighten any
/// existing file to 0600 before replacing it (closing the world-readable
/// window), write atomically, then restrict the result to 0600.
pub(crate) fn save_json_0600(path: &Path, value: &Value) -> anyhow::Result<()> {
    ensure_parent_dir(path)?;
    let bytes = serde_json::to_vec_pretty(value)?;
    prepare_existing_file_for_private_replace(path)?;
    crate::fs_atomic::atomic_write_sync(path, &bytes)?;
    restrict_file_permissions(path)?;
    Ok(())
}

fn ensure_parent_dir(path: &Path) -> anyhow::Result<()> {
    let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn prepare_existing_file_for_private_replace(path: &Path) -> anyhow::Result<()> {
    match std::fs::metadata(path) {
        Ok(_) => restrict_file_permissions(path),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(source.into()),
    }
}

#[cfg(not(unix))]
fn prepare_existing_file_for_private_replace(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "dirge_file_store_{tag}_{}_{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("auth.json")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn epoch_ms_is_expired_uses_inclusive_lower_bound() {
        assert!(!epoch_ms_is_expired(1_000, 999));
        assert!(epoch_ms_is_expired(1_000, 1_000));
        assert!(epoch_ms_is_expired(1_000, 1_001));
    }

    #[test]
    fn extract_account_id_prefers_canonical_then_aliases() {
        assert_eq!(
            extract_account_id(&json!({"account_id": "canon", "chatgpt_account_id": "alias"}))
                .as_deref(),
            Some("canon")
        );
        assert_eq!(
            extract_account_id(&json!({"chatgptAccountId": " acct "})).as_deref(),
            Some("acct")
        );
        assert_eq!(extract_account_id(&json!({"account_id": "  "})), None);
        assert_eq!(extract_account_id(&json!({})), None);
        assert_eq!(extract_account_id(&json!("not-an-object")), None);
    }

    #[test]
    fn load_json_returns_none_when_absent() {
        let dir = TestDir::new("absent");
        assert!(load_json(&dir.file()).unwrap().is_none());
    }

    #[test]
    fn load_json_parses_object() {
        let dir = TestDir::new("parse");
        std::fs::write(dir.file(), r#"{"a": 1}"#).unwrap();
        let map = load_json(&dir.file()).unwrap().unwrap();
        assert_eq!(map.get("a"), Some(&json!(1)));
    }

    #[test]
    fn save_json_0600_writes_private_file_that_round_trips() {
        let dir = TestDir::new("save");
        let value = json!({"hello": "world"});
        save_json_0600(&dir.file(), &value).unwrap();

        let loaded = load_json(&dir.file()).unwrap().unwrap();
        assert_eq!(loaded.get("hello"), Some(&json!("world")));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.file()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
