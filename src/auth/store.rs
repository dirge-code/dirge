use super::file_store::OPENAI_ACCOUNT_ID_ALIASES as ACCOUNT_ID_KEYS;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::fmt;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, AuthStoreError>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum AuthStoreError {
    #[error("OpenAI auth store I/O failed for {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "OpenAI auth store JSON is corrupt at {path:?}; fix or remove the file and run `dirge auth openai` again: {source}"
    )]
    CorruptJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("OpenAI auth entry is invalid at {path:?}; run `dirge auth openai` again: {reason}")]
    InvalidOpenAiCredential { path: PathBuf, reason: String },
    #[error("OpenAI auth store serialization failed for {path:?}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OpenAiOAuthCredential {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
    account_id: Option<String>,
    expires_at_epoch_ms: i64,
}

impl OpenAiOAuthCredential {
    pub(crate) fn new(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        id_token: Option<String>,
        account_id: Option<String>,
        expires_at_epoch_ms: i64,
    ) -> Self {
        Self {
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            id_token,
            account_id: normalize_optional_string(account_id),
            expires_at_epoch_ms,
        }
    }

    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }

    pub(crate) fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    pub(crate) fn id_token(&self) -> Option<&str> {
        self.id_token.as_deref()
    }

    pub(crate) fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub(crate) fn expires_at_epoch_ms(&self) -> i64 {
        self.expires_at_epoch_ms
    }

    pub(crate) fn is_expired_at(&self, epoch_ms: i64) -> bool {
        super::file_store::epoch_ms_is_expired(self.expires_at_epoch_ms, epoch_ms)
    }

    pub(crate) fn is_fresh_at(&self, epoch_ms: i64) -> bool {
        !self.is_expired_at(epoch_ms)
    }
}

impl fmt::Debug for OpenAiOAuthCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let id_token = self.id_token.as_ref().map(|_| "[REDACTED]");
        f.debug_struct("OpenAiOAuthCredential")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("id_token", &id_token)
            .field("account_id", &self.account_id)
            .field("expires_at_epoch_ms", &self.expires_at_epoch_ms)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OpenAiAuthStore {
    path: PathBuf,
}

impl Default for OpenAiAuthStore {
    fn default() -> Self {
        Self::at(crate::session::storage::dirs_path().join("auth.json"))
    }
}

impl OpenAiAuthStore {
    pub(crate) fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn load_openai(&self) -> Result<Option<OpenAiOAuthCredential>> {
        let Some(mut document) = self.load_document()? else {
            return Ok(None);
        };
        let Some(openai) = document.remove("openai") else {
            return Ok(None);
        };
        let mut openai = match openai {
            Value::Object(openai) => openai,
            _ => return Ok(None),
        };
        if openai.get("type").and_then(Value::as_str) != Some("oauth") {
            return Ok(None);
        }
        canonicalize_account_id_aliases(&mut openai);
        let entry: StoredOpenAiCredential =
            serde_json::from_value(Value::Object(openai)).map_err(|_source| {
                AuthStoreError::InvalidOpenAiCredential {
                    path: self.path.clone(),
                    reason: "stored OpenAI OAuth credential fields are malformed".to_string(),
                }
            })?;
        Ok(Some(entry.into_credential()))
    }

    pub(crate) fn save_openai(&self, credential: &OpenAiOAuthCredential) -> Result<()> {
        let mut document = self.load_document()?.unwrap_or_default();
        let mut openai = match document.remove("openai") {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };
        openai.insert("type".to_string(), json!("oauth"));
        openai.insert("access".to_string(), json!(credential.access_token));
        openai.insert("refresh".to_string(), json!(credential.refresh_token));
        openai.insert("expires".to_string(), json!(credential.expires_at_epoch_ms));
        match credential.id_token.as_deref() {
            Some(id_token) => {
                openai.insert("id_token".to_string(), json!(id_token));
            }
            None => {
                openai.remove("id_token");
            }
        }
        for key in ACCOUNT_ID_KEYS {
            openai.remove(*key);
        }
        if let Some(account_id) = credential.account_id.as_deref() {
            openai.insert("account_id".to_string(), json!(account_id));
        }
        document.insert("openai".to_string(), Value::Object(openai));

        super::file_store::save_json_0600(&self.path, &Value::Object(document)).map_err(|source| {
            AuthStoreError::Io {
                path: self.path.clone(),
                source: std::io::Error::other(source.to_string()),
            }
        })
    }

    fn load_document(&self) -> Result<Option<Map<String, Value>>> {
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(AuthStoreError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let value: Value =
            serde_json::from_str(&contents).map_err(|source| AuthStoreError::CorruptJson {
                path: self.path.clone(),
                source,
            })?;
        match value {
            Value::Object(document) => Ok(Some(document)),
            _ => Err(AuthStoreError::InvalidOpenAiCredential {
                path: self.path.clone(),
                reason: "top-level auth document must be a JSON object".to_string(),
            }),
        }
    }

    #[cfg(unix)]
    fn prepare_existing_file_for_private_replace(&self) -> Result<()> {
        match std::fs::metadata(&self.path) {
            Ok(_) => self.restrict_file_permissions(),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(AuthStoreError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    #[cfg(not(unix))]
    fn prepare_existing_file_for_private_replace(&self) -> Result<()> {
        Ok(())
    }

    #[cfg(unix)]
    fn restrict_file_permissions(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| AuthStoreError::Io {
                path: self.path.clone(),
                source,
            },
        )
    }

    #[cfg(not(unix))]
    fn restrict_file_permissions(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Deserialize)]
struct StoredOpenAiCredential {
    access: String,
    refresh: String,
    id_token: Option<String>,
    #[serde(
        default,
        alias = "chatgpt_account_id",
        alias = "chatgptAccountId",
        alias = "chatgpt_account",
        alias = "accountId"
    )]
    account_id: Option<String>,
    expires: i64,
}

impl StoredOpenAiCredential {
    fn into_credential(self) -> OpenAiOAuthCredential {
        OpenAiOAuthCredential::new(
            self.access,
            self.refresh,
            self.id_token,
            self.account_id,
            self.expires,
        )
    }
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn canonicalize_account_id_aliases(openai: &mut Map<String, Value>) {
    let account_id = super::file_store::extract_account_id(&Value::Object(openai.clone()));
    for key in ACCOUNT_ID_KEYS {
        openai.remove(*key);
    }
    if let Some(account_id) = account_id {
        openai.insert("account_id".to_string(), json!(account_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    const ACCOUNT_ID_KEYS: &[&str] = &[
        "account_id",
        "chatgpt_account_id",
        "chatgptAccountId",
        "chatgpt_account",
        "accountId",
    ];

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "dirge_auth_store_{tag}_{}_{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn auth_path(&self) -> PathBuf {
            self.path().join("auth.json")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn credential() -> OpenAiOAuthCredential {
        OpenAiOAuthCredential::new(
            "ACCESS-TOKEN",
            "REFRESH-TOKEN",
            Some("ID-TOKEN".to_string()),
            Some("acct-new".to_string()),
            1_900_000_000_000,
        )
    }

    #[test]
    fn missing_auth_file_loads_as_none() {
        let dir = TestDir::new("missing");
        let store = OpenAiAuthStore::at(dir.auth_path());

        assert!(store.load_openai().unwrap().is_none());
    }

    #[test]
    fn valid_openai_oauth_entry_loads() {
        let dir = TestDir::new("valid");
        std::fs::write(
            dir.auth_path(),
            json!({
                "openai": {
                    "type": "oauth",
                    "access": "ACCESS-TOKEN",
                    "refresh": "REFRESH-TOKEN",
                    "id_token": "ID-TOKEN",
                    "account_id": "acct-load",
                    "expires": 1900000000000_i64
                }
            })
            .to_string(),
        )
        .unwrap();
        let store = OpenAiAuthStore::at(dir.auth_path());

        let loaded = store.load_openai().unwrap().unwrap();

        assert_eq!(loaded.access_token(), "ACCESS-TOKEN");
        assert_eq!(loaded.refresh_token(), "REFRESH-TOKEN");
        assert_eq!(loaded.id_token(), Some("ID-TOKEN"));
        assert_eq!(loaded.account_id(), Some("acct-load"));
        assert_eq!(loaded.expires_at_epoch_ms(), 1_900_000_000_000);
    }

    #[test]
    fn openai_oauth_entry_with_canonical_and_alias_account_ids_loads_canonical() {
        let dir = TestDir::new("load_duplicate_account_aliases");
        std::fs::write(
            dir.auth_path(),
            json!({
                "openai": {
                    "type": "oauth",
                    "access": "ACCESS-TOKEN",
                    "refresh": "REFRESH-TOKEN",
                    "id_token": "ID-TOKEN",
                    "account_id": "acct-canonical",
                    "chatgpt_account_id": "acct-stale-alias",
                    "expires": 1900000000000_i64
                }
            })
            .to_string(),
        )
        .unwrap();
        let store = OpenAiAuthStore::at(dir.auth_path());

        let loaded = store.load_openai().unwrap().unwrap();

        assert_eq!(loaded.account_id(), Some("acct-canonical"));
    }

    #[test]
    fn legacy_openai_oauth_entry_without_account_id_loads() {
        let dir = TestDir::new("legacy_without_account");
        std::fs::write(
            dir.auth_path(),
            json!({
                "openai": {
                    "type": "oauth",
                    "access": "ACCESS-TOKEN",
                    "refresh": "REFRESH-TOKEN",
                    "id_token": "ID-TOKEN",
                    "expires": 1900000000000_i64
                }
            })
            .to_string(),
        )
        .unwrap();
        let store = OpenAiAuthStore::at(dir.auth_path());

        let loaded = store.load_openai().unwrap().unwrap();

        assert_eq!(loaded.account_id(), None);
        assert_eq!(loaded.access_token(), "ACCESS-TOKEN");
    }

    #[test]
    fn corrupt_auth_file_errors_without_deleting_or_echoing_secrets() {
        let dir = TestDir::new("corrupt");
        let secret_body = "{ ACCESS-TOKEN REFRESH-TOKEN ID-TOKEN USER-CODE";
        std::fs::write(dir.auth_path(), secret_body).unwrap();
        let store = OpenAiAuthStore::at(dir.auth_path());

        let err = store.load_openai().unwrap_err();
        let message = err.to_string();

        assert!(matches!(err, AuthStoreError::CorruptJson { .. }));
        assert_eq!(
            std::fs::read_to_string(dir.auth_path()).unwrap(),
            secret_body
        );
        assert!(!message.contains("ACCESS-TOKEN"));
        assert!(!message.contains("REFRESH-TOKEN"));
        assert!(!message.contains("ID-TOKEN"));
        assert!(!message.contains("USER-CODE"));
    }

    #[test]
    fn save_openai_preserves_other_providers_and_unknown_openai_fields() {
        let dir = TestDir::new("preserve");
        std::fs::write(
            dir.auth_path(),
            json!({
                "anthropic": {
                    "type": "api_key",
                    "key": "ANTHROPIC-SECRET",
                    "extra": { "keep": true }
                },
                "openai": {
                    "type": "oauth",
                    "access": "OLD-ACCESS",
                    "refresh": "OLD-REFRESH",
                    "id_token": "OLD-ID",
                    "expires": 1_i64,
                    "account_id": "acct_keep",
                    "fedramp": true
                },
                "custom": "keep-me"
            })
            .to_string(),
        )
        .unwrap();
        let store = OpenAiAuthStore::at(dir.auth_path());

        store.save_openai(&credential()).unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.auth_path()).unwrap()).unwrap();
        assert_eq!(saved["anthropic"]["key"], "ANTHROPIC-SECRET");
        assert_eq!(saved["custom"], "keep-me");
        assert_eq!(saved["openai"]["type"], "oauth");
        assert_eq!(saved["openai"]["access"], "ACCESS-TOKEN");
        assert_eq!(saved["openai"]["refresh"], "REFRESH-TOKEN");
        assert_eq!(saved["openai"]["id_token"], "ID-TOKEN");
        assert_eq!(saved["openai"]["expires"], 1_900_000_000_000_i64);
        assert_eq!(saved["openai"]["account_id"], "acct-new");
        assert_eq!(saved["openai"]["fedramp"], true);
    }

    #[test]
    fn save_openai_removes_stale_account_id_when_new_credential_has_none() {
        let dir = TestDir::new("remove_account_id");
        std::fs::write(
            dir.auth_path(),
            json!({
                "openai": {
                    "type": "oauth",
                    "access": "OLD-ACCESS",
                    "refresh": "OLD-REFRESH",
                    "expires": 1_i64,
                    "account_id": "acct-stale"
                }
            })
            .to_string(),
        )
        .unwrap();
        let store = OpenAiAuthStore::at(dir.auth_path());
        let credential = OpenAiOAuthCredential::new(
            "ACCESS-TOKEN",
            "REFRESH-TOKEN",
            Some("ID-TOKEN".to_string()),
            None,
            1_900_000_000_000,
        );

        store.save_openai(&credential).unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.auth_path()).unwrap()).unwrap();
        assert!(saved["openai"].get("account_id").is_none());
    }

    #[test]
    fn save_openai_removes_stale_account_id_aliases_when_new_credential_has_none() {
        let dir = TestDir::new("remove_account_id_aliases");
        std::fs::write(
            dir.auth_path(),
            json!({
                "openai": {
                    "type": "oauth",
                    "access": "OLD-ACCESS",
                    "refresh": "OLD-REFRESH",
                    "expires": 1_i64,
                    "account_id": "acct-stale-canonical",
                    "chatgpt_account_id": "acct-stale-snake",
                    "chatgptAccountId": "acct-stale-camel",
                    "chatgpt_account": "acct-stale-short",
                    "accountId": "acct-stale-account-id"
                }
            })
            .to_string(),
        )
        .unwrap();
        let store = OpenAiAuthStore::at(dir.auth_path());
        let credential = OpenAiOAuthCredential::new(
            "ACCESS-TOKEN",
            "REFRESH-TOKEN",
            Some("ID-TOKEN".to_string()),
            None,
            1_900_000_000_000,
        );

        store.save_openai(&credential).unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.auth_path()).unwrap()).unwrap();
        for key in ACCOUNT_ID_KEYS {
            assert!(
                saved["openai"].get(key).is_none(),
                "stale alias {key} remained"
            );
        }
    }

    #[test]
    fn save_openai_canonicalizes_account_id_aliases_when_new_credential_has_account_id() {
        let dir = TestDir::new("canonicalize_account_id_aliases");
        std::fs::write(
            dir.auth_path(),
            json!({
                "openai": {
                    "type": "oauth",
                    "access": "OLD-ACCESS",
                    "refresh": "OLD-REFRESH",
                    "expires": 1_i64,
                    "chatgpt_account_id": "acct-stale-snake",
                    "chatgptAccountId": "acct-stale-camel",
                    "chatgpt_account": "acct-stale-short",
                    "accountId": "acct-stale-account-id"
                }
            })
            .to_string(),
        )
        .unwrap();
        let store = OpenAiAuthStore::at(dir.auth_path());

        store.save_openai(&credential()).unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.auth_path()).unwrap()).unwrap();
        assert_eq!(saved["openai"]["account_id"], "acct-new");
        for key in ACCOUNT_ID_KEYS
            .iter()
            .copied()
            .filter(|key| *key != "account_id")
        {
            assert!(
                saved["openai"].get(key).is_none(),
                "stale alias {key} remained"
            );
        }
    }

    #[test]
    fn save_openai_creates_private_auth_file_on_unix() {
        let dir = TestDir::new("permissions");
        let store = OpenAiAuthStore::at(dir.auth_path());

        store.save_openai(&credential()).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.auth_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn save_openai_tightens_existing_auth_file_permissions_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new("tighten_permissions");
        std::fs::write(dir.auth_path(), "{}").unwrap();
        std::fs::set_permissions(dir.auth_path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let store = OpenAiAuthStore::at(dir.auth_path());

        store.save_openai(&credential()).unwrap();

        let mode = std::fs::metadata(dir.auth_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn prepares_existing_auth_file_private_before_atomic_replacement_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new("prepare_permissions");
        std::fs::write(dir.auth_path(), "{}").unwrap();
        std::fs::set_permissions(dir.auth_path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let store = OpenAiAuthStore::at(dir.auth_path());

        store.prepare_existing_file_for_private_replace().unwrap();

        let mode = std::fs::metadata(dir.auth_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn expiry_helpers_distinguish_fresh_and_expired_tokens() {
        let token = OpenAiOAuthCredential::new("ACCESS-TOKEN", "REFRESH-TOKEN", None, None, 1_000);

        assert!(token.is_fresh_at(999));
        assert!(!token.is_expired_at(999));
        assert!(token.is_expired_at(1_000));
        assert!(!token.is_fresh_at(1_000));
    }

    #[test]
    fn default_store_uses_dirge_data_dir_override() {
        let _guard = crate::auth::DIRGE_DATA_DIR_ENV_LOCK.lock().unwrap();
        let dir = TestDir::new("env");
        let previous = std::env::var_os("DIRGE_DATA_DIR");
        // SAFETY: auth tests serialize DIRGE_DATA_DIR changes with DIRGE_DATA_DIR_ENV_LOCK.
        unsafe {
            std::env::set_var("DIRGE_DATA_DIR", dir.path());
        }

        let store = OpenAiAuthStore::default();

        assert_eq!(store.path(), dir.auth_path());
        // SAFETY: DIRGE_DATA_DIR_ENV_LOCK remains held until after restoration.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("DIRGE_DATA_DIR", value),
                None => std::env::remove_var("DIRGE_DATA_DIR"),
            }
        }
    }

    #[test]
    fn debug_and_errors_redact_secret_values() {
        let dir = TestDir::new("redact");
        let token = credential();
        let debug = format!("{token:?}");

        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("ACCESS-TOKEN"));
        assert!(!debug.contains("REFRESH-TOKEN"));
        assert!(!debug.contains("ID-TOKEN"));

        std::fs::write(
            dir.auth_path(),
            json!({
                "openai": {
                    "type": "oauth",
                    "access": "ACCESS-TOKEN",
                    "refresh": "REFRESH-TOKEN",
                    "id_token": "ID-TOKEN",
                    "expires": "ACCESS-TOKEN"
                }
            })
            .to_string(),
        )
        .unwrap();
        let err = OpenAiAuthStore::at(dir.auth_path())
            .load_openai()
            .unwrap_err();
        let message = err.to_string();
        let error_debug = format!("{err:?}");

        assert!(!message.contains("ACCESS-TOKEN"));
        assert!(!message.contains("REFRESH-TOKEN"));
        assert!(!message.contains("ID-TOKEN"));
        assert!(!message.contains("USER-CODE"));
        assert!(!error_debug.contains("ACCESS-TOKEN"));
        assert!(!error_debug.contains("REFRESH-TOKEN"));
        assert!(!error_debug.contains("ID-TOKEN"));
        assert!(!error_debug.contains("USER-CODE"));
    }
}
