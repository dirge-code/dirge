pub(crate) mod command;
// Shared 0600 file-store + expiry + account-id helpers used by both OAuth stores.
pub(crate) mod file_store;
// Staged for downstream CLI/provider beads; this child owns the tested protocol flow.
#[allow(dead_code)]
pub(crate) mod openai_device;
// Staged for downstream provider/CLI beads; this child owns persistence behavior.
#[allow(dead_code)]
pub(crate) mod store;

#[cfg(test)]
pub(crate) static DIRGE_DATA_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod command_tests {
    use super::command::{
        OpenAiCredentialStore, OpenAiLoginFlow, login_openai_with, run_auth_action_with,
    };
    use super::openai_device::{
        DeviceAuthError, DeviceAuthHttp, DeviceAuthRuntime, DeviceCode, HttpResponse, OAuthTokens,
        OpenAiDeviceAuthFlow, Result as DeviceAuthResult,
    };
    use super::store::{OpenAiAuthStore, OpenAiOAuthCredential};
    use crate::cli::AuthAction;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "dirge_auth_command_{tag}_{}_{}",
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

    #[derive(Clone)]
    struct FakeHttp {
        responses: Arc<Mutex<VecDeque<DeviceAuthResult<HttpResponse>>>>,
    }

    impl FakeHttp {
        fn new(responses: impl IntoIterator<Item = DeviceAuthResult<HttpResponse>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            }
        }
    }

    impl DeviceAuthHttp for FakeHttp {
        fn post_json(
            &self,
            _url: String,
            _body: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = DeviceAuthResult<HttpResponse>> + Send + '_>> {
            Box::pin(async move { self.responses.lock().unwrap().pop_front().unwrap() })
        }

        fn post_form(
            &self,
            _url: String,
            _form: Vec<(String, String)>,
        ) -> Pin<Box<dyn Future<Output = DeviceAuthResult<HttpResponse>> + Send + '_>> {
            Box::pin(async move { self.responses.lock().unwrap().pop_front().unwrap() })
        }
    }

    #[derive(Clone)]
    struct FakeRuntime {
        now: Instant,
    }

    impl FakeRuntime {
        fn new() -> Self {
            Self {
                now: Instant::now(),
            }
        }
    }

    impl DeviceAuthRuntime for FakeRuntime {
        fn now(&self) -> Instant {
            self.now
        }

        fn sleep(&self, _duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }
    }

    #[derive(Clone)]
    struct FakeLoginFlow {
        state: Arc<Mutex<FakeLoginFlowState>>,
    }

    struct FakeLoginFlowState {
        device_code: Option<DeviceAuthResult<DeviceCode>>,
        tokens: Option<DeviceAuthResult<OAuthTokens>>,
        completed_with: Option<DeviceCode>,
    }

    impl FakeLoginFlow {
        fn new(device_code: DeviceCode, tokens: OAuthTokens) -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeLoginFlowState {
                    device_code: Some(Ok(device_code)),
                    tokens: Some(Ok(tokens)),
                    completed_with: None,
                })),
            }
        }

        fn completed_with(&self) -> Option<DeviceCode> {
            self.state.lock().unwrap().completed_with.clone()
        }
    }

    impl OpenAiLoginFlow for FakeLoginFlow {
        fn request_device_code(
            &self,
        ) -> Pin<Box<dyn Future<Output = DeviceAuthResult<DeviceCode>> + Send + '_>> {
            Box::pin(async move { self.state.lock().unwrap().device_code.take().unwrap() })
        }

        fn complete_device_code_login(
            &self,
            device_code: DeviceCode,
        ) -> Pin<Box<dyn Future<Output = DeviceAuthResult<OAuthTokens>> + Send + '_>> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                state.completed_with = Some(device_code);
                state.tokens.take().unwrap()
            })
        }
    }

    #[derive(Clone)]
    struct FakeStore {
        path: PathBuf,
        state: Arc<Mutex<FakeStoreState>>,
    }

    struct FakeStoreState {
        saved: Vec<OpenAiOAuthCredential>,
        save_error: Option<&'static str>,
    }

    impl FakeStore {
        fn new(path: PathBuf) -> Self {
            Self {
                path,
                state: Arc::new(Mutex::new(FakeStoreState {
                    saved: Vec::new(),
                    save_error: None,
                })),
            }
        }

        fn with_save_error(path: PathBuf, save_error: &'static str) -> Self {
            Self {
                path,
                state: Arc::new(Mutex::new(FakeStoreState {
                    saved: Vec::new(),
                    save_error: Some(save_error),
                })),
            }
        }

        fn saved(&self) -> Vec<OpenAiOAuthCredential> {
            self.state.lock().unwrap().saved.clone()
        }
    }

    impl OpenAiCredentialStore for FakeStore {
        fn path(&self) -> &Path {
            &self.path
        }

        fn save_openai(&self, credential: &OpenAiOAuthCredential) -> anyhow::Result<()> {
            let mut state = self.state.lock().unwrap();
            if let Some(save_error) = state.save_error.take() {
                anyhow::bail!(save_error);
            }
            state.saved.push(credential.clone());
            Ok(())
        }
    }

    fn response(status: u16, body: serde_json::Value) -> DeviceAuthResult<HttpResponse> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
        })
    }

    fn flow(http: FakeHttp) -> OpenAiDeviceAuthFlow<FakeHttp, FakeRuntime> {
        OpenAiDeviceAuthFlow::with_parts(
            "https://auth.openai.com",
            "client-test",
            http,
            FakeRuntime::new(),
        )
    }

    fn device_code() -> DeviceCode {
        DeviceCode {
            verification_url: "https://auth.openai.com/codex/device".to_string(),
            user_code: "USER-CODE".to_string(),
            device_auth_id: "DEVICE-AUTH-ID".to_string(),
            interval: Duration::from_secs(5),
        }
    }

    fn tokens(expires_in: Option<u64>) -> OAuthTokens {
        OAuthTokens {
            access_token: "ACCESS-TOKEN".to_string(),
            refresh_token: "REFRESH-TOKEN".to_string(),
            id_token: "ID-TOKEN".to_string(),
            account_id: None,
            expires_in,
        }
    }

    fn tokens_with_account(account_id: &str) -> OAuthTokens {
        OAuthTokens {
            account_id: Some(account_id.to_string()),
            ..tokens(Some(3600))
        }
    }

    #[tokio::test]
    async fn auth_action_dispatch_invokes_injected_openai_login() {
        let calls = Arc::new(Mutex::new(0));

        run_auth_action_with(&AuthAction::Openai, || {
            let calls = calls.clone();
            async move {
                *calls.lock().unwrap() += 1;
                Ok(())
            }
        })
        .await
        .unwrap();

        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn openai_login_with_fake_flow_and_store_saves_credential_and_redacts_stdout() {
        let flow = FakeLoginFlow::new(device_code(), tokens(Some(3600)));
        let store = FakeStore::new(PathBuf::from("/tmp/fake-auth.json"));
        let mut stdout = Vec::new();

        login_openai_with(flow.clone(), store.clone(), 1_700_000_000_000, &mut stdout)
            .await
            .unwrap();

        assert_eq!(
            flow.completed_with().unwrap().device_auth_id,
            "DEVICE-AUTH-ID"
        );
        let saved = store.saved();
        assert_eq!(saved.len(), 1);
        let credential = &saved[0];
        assert_eq!(credential.access_token(), "ACCESS-TOKEN");
        assert_eq!(credential.refresh_token(), "REFRESH-TOKEN");
        assert_eq!(credential.id_token(), Some("ID-TOKEN"));
        assert_eq!(credential.expires_at_epoch_ms(), 1_700_003_600_000);

        let stdout = String::from_utf8(stdout).unwrap();
        assert!(stdout.contains("https://auth.openai.com/codex/device"));
        assert!(stdout.contains("USER-CODE"));
        assert!(stdout.contains("/tmp/fake-auth.json"));
        assert!(!stdout.contains("ACCESS-TOKEN"));
        assert!(!stdout.contains("REFRESH-TOKEN"));
        assert!(!stdout.contains("ID-TOKEN"));
        assert!(!stdout.contains("DEVICE-AUTH-ID"));
    }

    #[tokio::test]
    async fn openai_login_saves_optional_account_id_from_token_response() {
        let flow = FakeLoginFlow::new(device_code(), tokens_with_account("acct-login"));
        let store = FakeStore::new(PathBuf::from("/tmp/fake-auth.json"));
        let mut stdout = Vec::new();

        login_openai_with(flow, store.clone(), 1_700_000_000_000, &mut stdout)
            .await
            .unwrap();

        let saved = store.saved();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].account_id(), Some("acct-login"));
        let stdout = String::from_utf8(stdout).unwrap();
        assert!(!stdout.contains("ACCESS-TOKEN"));
        assert!(!stdout.contains("REFRESH-TOKEN"));
        assert!(!stdout.contains("ID-TOKEN"));
    }

    #[tokio::test]
    async fn openai_login_propagates_fake_store_error_without_leaking_tokens_to_stdout() {
        let flow = FakeLoginFlow::new(device_code(), tokens(Some(3600)));
        let store = FakeStore::with_save_error(PathBuf::from("/tmp/fake-auth.json"), "disk full");
        let mut stdout = Vec::new();

        let err = login_openai_with(flow, store.clone(), 1_700_000_000_000, &mut stdout)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("disk full"));
        assert!(store.saved().is_empty());
        let stdout = String::from_utf8(stdout).unwrap();
        assert!(stdout.contains("USER-CODE"));
        assert!(!stdout.contains("OpenAI authorization saved"));
        assert!(!stdout.contains("ACCESS-TOKEN"));
        assert!(!stdout.contains("REFRESH-TOKEN"));
        assert!(!stdout.contains("ID-TOKEN"));
    }

    #[tokio::test]
    async fn openai_login_prints_device_instructions_and_saves_tokens_under_data_dir() {
        let dir = TestDir::new("success");
        let store = OpenAiAuthStore::at(dir.auth_path());
        let http = FakeHttp::new([
            response(
                200,
                json!({
                    "device_auth_id": "DEVICE-AUTH-ID",
                    "user_code": "USER-CODE",
                    "interval": 5
                }),
            ),
            response(
                200,
                json!({
                    "authorization_code": "AUTH-CODE",
                    "code_challenge": "challenge",
                    "code_verifier": "verifier"
                }),
            ),
            response(
                200,
                json!({
                    "access_token": "ACCESS-TOKEN",
                    "refresh_token": "REFRESH-TOKEN",
                    "id_token": "ID-TOKEN",
                    "expires_in": 3600
                }),
            ),
        ]);
        let mut stdout = Vec::new();

        login_openai_with(flow(http), store.clone(), 1_700_000_000_000, &mut stdout)
            .await
            .unwrap();

        assert_eq!(store.path(), dir.auth_path());
        assert!(store.path().starts_with(dir.path()));
        let stdout = String::from_utf8(stdout).unwrap();
        assert!(stdout.contains("https://auth.openai.com/codex/device"));
        assert!(stdout.contains("USER-CODE"));
        assert!(stdout.contains("Do not share"));
        assert!(stdout.contains(store.path().to_string_lossy().as_ref()));
        assert!(!stdout.contains("ACCESS-TOKEN"));
        assert!(!stdout.contains("REFRESH-TOKEN"));
        assert!(!stdout.contains("ID-TOKEN"));
        assert!(!stdout.contains("AUTH-CODE"));
        assert!(!stdout.contains("DEVICE-AUTH-ID"));

        let saved = std::fs::read_to_string(store.path()).unwrap();
        assert!(saved.contains("ACCESS-TOKEN"));
        assert!(saved.contains("REFRESH-TOKEN"));
        assert!(saved.contains("ID-TOKEN"));
        assert!(saved.contains("1700003600000"));
    }

    #[tokio::test]
    async fn openai_login_disabled_error_is_actionable_and_does_not_create_auth_file() {
        let dir = TestDir::new("disabled");
        let store = OpenAiAuthStore::at(dir.auth_path());
        let http = FakeHttp::new([response(404, json!({"error": "not found"}))]);
        let mut stdout = Vec::new();

        let err = login_openai_with(flow(http), store.clone(), 1_700_000_000_000, &mut stdout)
            .await
            .unwrap_err();
        let message = err.to_string();

        assert!(matches!(
            err.downcast_ref::<DeviceAuthError>(),
            Some(DeviceAuthError::DeviceAuthDisabled)
        ));
        assert!(message.contains("enable device-code auth"));
        assert!(message.contains("ChatGPT Codex security settings"));
        assert!(!message.contains("ACCESS-TOKEN"));
        assert!(!message.contains("REFRESH-TOKEN"));
        assert!(!message.contains("ID-TOKEN"));
        assert!(!store.path().exists());
        assert!(stdout.is_empty());
    }

    #[tokio::test]
    async fn openai_login_without_expires_in_uses_conservative_access_expiry() {
        let dir = TestDir::new("missing_expires");
        let store = OpenAiAuthStore::at(dir.auth_path());
        let http = FakeHttp::new([
            response(
                200,
                json!({
                    "device_auth_id": "DEVICE-AUTH-ID",
                    "user_code": "USER-CODE",
                    "interval": 5
                }),
            ),
            response(
                200,
                json!({
                    "authorization_code": "AUTH-CODE",
                    "code_challenge": "challenge",
                    "code_verifier": "verifier"
                }),
            ),
            response(
                200,
                json!({
                    "access_token": "ACCESS-TOKEN",
                    "refresh_token": "REFRESH-TOKEN",
                    "id_token": "ID-TOKEN"
                }),
            ),
        ]);
        let mut stdout = Vec::new();

        login_openai_with(flow(http), store.clone(), 1_700_000_000_000, &mut stdout)
            .await
            .unwrap();

        let saved = std::fs::read_to_string(store.path()).unwrap();
        assert!(saved.contains("1700000300000"));
    }

    #[test]
    fn oauth_validation_scripts_isolate_config_dir() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for script in [
            "scripts/validate-openai-oauth.sh",
            "scripts/openai-oauth-interactive.sh",
        ] {
            let body = std::fs::read_to_string(root.join(script)).unwrap();

            assert!(
                body.contains("DIRGE_OAUTH_VALIDATION_CONFIG_DIR"),
                "{script} must document its isolated config dir override"
            );
            assert!(
                body.contains("export DIRGE_CONFIG_DIR=\"$config_dir\""),
                "{script} must export an isolated DIRGE_CONFIG_DIR"
            );
            assert!(
                body.contains("rm -rf -- \"$config_dir\""),
                "{script} must clear stale validation config before each run"
            );
            assert!(
                body.contains("Using DIRGE_CONFIG_DIR"),
                "{script} must print the non-secret config dir in validation evidence"
            );
        }
    }
}
