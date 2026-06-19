use super::openai_device::{
    DeviceAuthHttp, DeviceAuthRuntime, DeviceCode, OAuthTokens, OpenAiDeviceAuthFlow,
    Result as DeviceAuthResult,
};
use super::store::{OpenAiAuthStore, OpenAiOAuthCredential};
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// If OpenAI omits expires_in, assume a short-lived access token so future
// provider work refreshes early while keeping the persisted refresh token.
const FALLBACK_ACCESS_TOKEN_EXPIRES_IN: Duration = Duration::from_secs(5 * 60);
type DeviceCodeFuture<'a> = Pin<Box<dyn Future<Output = DeviceAuthResult<DeviceCode>> + Send + 'a>>;
type TokenFuture<'a> = Pin<Box<dyn Future<Output = DeviceAuthResult<OAuthTokens>> + Send + 'a>>;

pub(crate) trait OpenAiLoginFlow {
    fn request_device_code(&self) -> DeviceCodeFuture<'_>;

    fn complete_device_code_login(&self, device_code: DeviceCode) -> TokenFuture<'_>;
}

impl<H, R> OpenAiLoginFlow for OpenAiDeviceAuthFlow<H, R>
where
    H: DeviceAuthHttp,
    R: DeviceAuthRuntime,
{
    fn request_device_code(&self) -> DeviceCodeFuture<'_> {
        Box::pin(async move { OpenAiDeviceAuthFlow::request_device_code(self).await })
    }

    fn complete_device_code_login(&self, device_code: DeviceCode) -> TokenFuture<'_> {
        Box::pin(async move {
            OpenAiDeviceAuthFlow::complete_device_code_login(self, device_code).await
        })
    }
}

pub(crate) trait OpenAiCredentialStore {
    fn path(&self) -> &Path;

    fn save_openai(&self, credential: &OpenAiOAuthCredential) -> anyhow::Result<()>;
}

impl OpenAiCredentialStore for OpenAiAuthStore {
    fn path(&self) -> &Path {
        OpenAiAuthStore::path(self)
    }

    fn save_openai(&self, credential: &OpenAiOAuthCredential) -> anyhow::Result<()> {
        OpenAiAuthStore::save_openai(self, credential)?;
        Ok(())
    }
}

pub(crate) async fn run_auth_action(action: &crate::cli::AuthAction) -> anyhow::Result<()> {
    run_auth_action_with(action, login_openai).await
}

pub(crate) async fn run_auth_action_with<L, Fut>(
    action: &crate::cli::AuthAction,
    openai_login: L,
) -> anyhow::Result<()>
where
    L: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    match action {
        crate::cli::AuthAction::Openai => openai_login().await,
        crate::cli::AuthAction::Anthropic => {
            let path = crate::provider::anthropic_oauth::login_and_persist().await?;
            println!("Anthropic OAuth credentials saved to {}", path.display());
            Ok(())
        }
    }
}

pub(crate) async fn login_openai() -> anyhow::Result<()> {
    let flow = OpenAiDeviceAuthFlow::default();
    let store = OpenAiAuthStore::default();
    let mut stdout = std::io::stdout().lock();
    login_openai_with_clock(flow, store, current_epoch_ms, &mut stdout).await
}

#[cfg(test)]
pub(crate) async fn login_openai_with<F, S, W>(
    flow: F,
    store: S,
    now_epoch_ms: i64,
    stdout: &mut W,
) -> anyhow::Result<()>
where
    F: OpenAiLoginFlow,
    S: OpenAiCredentialStore,
    W: Write,
{
    login_openai_with_clock(flow, store, || Ok(now_epoch_ms), stdout).await
}

async fn login_openai_with_clock<F, S, W, N>(
    flow: F,
    store: S,
    now_epoch_ms: N,
    stdout: &mut W,
) -> anyhow::Result<()>
where
    F: OpenAiLoginFlow,
    S: OpenAiCredentialStore,
    W: Write,
    N: FnOnce() -> anyhow::Result<i64>,
{
    let device_code = flow.request_device_code().await?;

    writeln!(stdout, "OpenAI device-code login")?;
    writeln!(stdout, "1. Open: {}", device_code.verification_url)?;
    writeln!(stdout, "2. Enter code: {}", device_code.user_code)?;
    writeln!(
        stdout,
        "Do not share this code. Anyone with it may be able to authorize Dirge as you."
    )?;
    writeln!(stdout, "Waiting for OpenAI authorization...")?;

    let tokens = flow.complete_device_code_login(device_code).await?;
    let credential = oauth_tokens_to_credential(tokens, now_epoch_ms()?);
    store.save_openai(&credential)?;

    writeln!(
        stdout,
        "OpenAI authorization saved to {}",
        store.path().display()
    )?;
    writeln!(
        stdout,
        "This login persists across Dirge sessions until you delete that file or OpenAI revokes it."
    )?;

    Ok(())
}

fn oauth_tokens_to_credential(tokens: OAuthTokens, now_epoch_ms: i64) -> OpenAiOAuthCredential {
    let expires_at_epoch_ms = access_token_expires_at_epoch_ms(now_epoch_ms, tokens.expires_in);
    OpenAiOAuthCredential::new(
        tokens.access_token,
        tokens.refresh_token,
        Some(tokens.id_token),
        tokens.account_id,
        expires_at_epoch_ms,
    )
}

fn access_token_expires_at_epoch_ms(now_epoch_ms: i64, expires_in_seconds: Option<u64>) -> i64 {
    let expires_in_seconds =
        expires_in_seconds.unwrap_or(FALLBACK_ACCESS_TOKEN_EXPIRES_IN.as_secs());
    let expires_in_ms = expires_in_seconds.saturating_mul(1000);
    let expires_in_ms = i64::try_from(expires_in_ms).unwrap_or(i64::MAX);
    now_epoch_ms.saturating_add(expires_in_ms)
}

fn current_epoch_ms() -> anyhow::Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| anyhow::anyhow!("system clock is before Unix epoch: {err}"))?;
    Ok(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
}
