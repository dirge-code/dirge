# OpenAI Device Auth For Rust Integrations

This guide documents the OpenAI ChatGPT/Codex device-code OAuth flow as a reusable Rust integration pattern. It is based on the Dirge implementation and is intended for future projects that need headless login from SSH sessions, containers, CI-like shells, or terminals without a browser.

OpenAI may change this private-ish ChatGPT/Codex auth surface. Treat the constants below as versioned integration knowledge, not as a guaranteed public OAuth contract.

## Use Case

Use this flow when a Rust CLI needs to authenticate a ChatGPT/Codex user account without placing OpenAI API keys in argv, environment variables, or project config.

Do not use this flow for OpenAI API-key billing. OAuth access tokens obtained here are for the ChatGPT/Codex backend. Keep the API-key path separate and require explicit user consent before switching a request to paid API-key billing.

## Constants

Default issuer:

```text
https://auth.openai.com
```

Client id:

```text
app_EMoamEEZ73f0CkXaXp7hrann
```

Redirect URI:

```text
https://auth.openai.com/deviceauth/callback
```

Verification URL to show the user:

```text
https://auth.openai.com/codex/device
```

Recommended polling defaults:

```text
poll interval fallback: 5 seconds
overall timeout: 15 minutes
per-request timeout: 30 seconds
```

## Protocol Flow

### 1. Request A Device Code

Request:

```http
POST https://auth.openai.com/api/accounts/deviceauth/usercode
Content-Type: application/json

{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}
```

Successful response fields observed:

```json
{
  "device_auth_id": "...",
  "user_code": "...",
  "interval": 5
}
```

Compatibility notes:

- Accept `usercode` as an alias for `user_code`.
- Accept numeric or string `interval` values.
- If `interval` is absent, null, or zero, use the fallback interval.
- Treat HTTP `404` as an actionable device-auth-disabled error. The user likely needs to enable device-code auth in ChatGPT Codex security settings.

Show the user:

- the verification URL
- the user code
- a warning that device codes are phishing-sensitive and should only be entered in the official OpenAI page they intentionally opened

Never log the user code or device auth id in debug output.

### 2. Poll For Authorization

After the user approves the code in the browser, poll:

```http
POST https://auth.openai.com/api/accounts/deviceauth/token
Content-Type: application/json

{"device_auth_id":"...","user_code":"..."}
```

Successful response fields observed:

```json
{
  "authorization_code": "...",
  "code_challenge": "...",
  "code_verifier": "..."
}
```

Polling behavior:

- Treat HTTP `403` and `404` as pending or not-yet-approved states.
- Sleep for the device-code interval, capped by the remaining overall timeout.
- Keep polling until success or timeout.
- Treat any other status as a polling failure.

Never log the authorization code, code verifier, user code, or device auth id.

### 3. Exchange Authorization Code For Tokens

Request:

```http
POST https://auth.openai.com/oauth/token
Content-Type: application/x-www-form-urlencoded

grant_type=authorization_code&
code=...&
redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback&
client_id=app_EMoamEEZ73f0CkXaXp7hrann&
code_verifier=...
```

Successful response fields observed:

```json
{
  "access_token": "...",
  "refresh_token": "...",
  "id_token": "...",
  "expires_in": 3600,
  "account_id": "..."
}
```

Account-id compatibility aliases to accept:

```text
account_id
chatgpt_account_id
chatgptAccountId
chatgpt_account
accountId
```

`account_id` is optional. It is account/workspace selection metadata, not a replacement for the OAuth access token. Normalize blank strings to `None`.

Never log access tokens, refresh tokens, id tokens, authorization codes, or code verifiers.

## Rust Architecture

Use three small boundaries so the flow is deterministic in tests:

```rust
trait DeviceAuthHttp: Clone + Send + Sync + 'static {
    fn post_json(&self, url: String, body: serde_json::Value) -> HttpFuture<'_>;
    fn post_form(&self, url: String, form: Vec<(String, String)>) -> HttpFuture<'_>;
}

trait DeviceAuthRuntime: Clone + Send + Sync + 'static {
    fn now(&self) -> std::time::Instant;
    fn sleep(&self, duration: std::time::Duration) -> SleepFuture<'_>;
}

struct OpenAiDeviceAuthFlow<H, R> {
    issuer: String,
    client_id: String,
    http: H,
    runtime: R,
    timeout: std::time::Duration,
}
```

Production adapters:

- `reqwest::Client` for HTTP.
- `tokio::time::sleep` and `Instant::now` for runtime.
- Per-request timeout on each HTTP call.

Test adapters:

- Queue fake HTTP responses and record request bodies.
- Fake runtime with deterministic `now()` and recorded sleeps.
- No live network, no real user codes, no real tokens.

## Data Types

Recommended model types:

```rust
struct DeviceCode {
    verification_url: String,
    user_code: String,
    device_auth_id: String,
    interval: Duration,
}

struct AuthorizationCode {
    authorization_code: String,
    code_verifier: String,
}

struct OAuthTokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
    account_id: Option<String>,
    expires_in: Option<u64>,
}

struct StoredOpenAiCredential {
    access: String,
    refresh: String,
    id_token: Option<String>,
    account_id: Option<String>,
    expires: i64,
}
```

Implement custom `Debug` for every type that can contain secrets. Redact:

- user code
- device auth id
- authorization code
- code verifier
- access token
- refresh token
- id token
- raw HTTP response body

It is acceptable to show `account_id` in debug output if your product treats it as non-secret metadata, but avoid printing it in normal login output unless users explicitly need workspace diagnostics.

## Storage Contract

Persist credentials in an application-owned data directory, not in project-local files.

Recommended JSON shape:

```json
{
  "openai": {
    "type": "oauth",
    "access": "...",
    "refresh": "...",
    "id_token": "...",
    "account_id": "...",
    "expires": 1900000000000
  }
}
```

Storage rules:

- Use atomic writes.
- Preserve unrelated providers or unknown top-level entries.
- Restrict file permissions to owner-read/write on Unix, typically `0600` for the file and `0700` for the parent directory.
- Canonicalize account-id aliases before deserialization so files containing both canonical and legacy aliases do not fail with duplicate serde fields.
- When saving, remove all accepted account-id aliases and write only canonical `account_id` when present.
- Load legacy entries with no account id.
- Expired credentials should produce an actionable login error or be refreshed if the project implements refresh.

Do not write auth logs, validation artifacts, or raw auth files into git-tracked paths.

## Provider Integration

Keep provider selection explicit:

- Apply native OpenAI OAuth only to canonical `openai` with no configured/custom `base_url`.
- Do not send ChatGPT/Codex OAuth tokens to OpenAI-compatible proxies, local aliases, or custom base URLs.
- Keep API-key clients separate from ChatGPT/Codex OAuth clients.
- If an API key is used only as a fallback after quota exhaustion, ask for explicit user confirmation first because that may create API-billing charges.
- For headless modes, a missing or unanswered confirmation path must fail closed with an actionable error, not silently switch billing and not hang forever.

For Rig-based Rust projects, the ChatGPT/Codex path can be represented as:

```rust
rig::providers::chatgpt::ChatGPTAuth::AccessToken {
    access_token,
    account_id,
}
```

The expected backend for Codex subscription auth is:

```text
https://chatgpt.com/backend-api/codex
```

## Error Handling

Use specific errors for:

- device auth disabled (`404` from user-code request)
- overall timeout
- user-code request status failure
- polling status failure
- token exchange status failure
- malformed JSON or unexpected response shape
- transport failure

Error messages must be actionable but secret-free. For example:

```text
OpenAI device-code auth is not enabled. Enable device-code auth in ChatGPT Codex security settings, then run login again.
```

Avoid including raw response bodies in errors. If response bodies need diagnostics, capture only redacted status and a categorized parse/status reason.

## Test Checklist

Minimum deterministic tests:

- user-code request sends the expected endpoint and client id
- `usercode` alias parses as `user_code`
- numeric, string, missing, null, and zero intervals behave correctly
- device-auth-disabled `404` is actionable
- pending poll sleeps and retries for `403` and `404`
- polling times out without real sleeping
- successful poll exchanges authorization code with form-encoded body
- token exchange parses access, refresh, id token, optional expiry, and account-id aliases
- malformed JSON produces redacted parse errors
- transport and status errors do not echo bodies or secrets
- debug output redacts every secret-bearing field
- storage loads legacy entries without account id
- storage canonicalizes duplicate account-id aliases and prefers canonical `account_id`
- storage removes stale account-id aliases on save
- storage preserves unrelated providers and unknown fields
- provider integration refuses native OAuth for aliases or custom base URLs
- API-key billing fallback requires user confirmation and fails closed in non-interactive modes

Live validation, when required, should use a segregated account and record only secret-free evidence:

- command shape with secrets omitted
- provider/backend/model name
- redacted status summary
- expected minimal response text or categorized error

Never record access tokens, refresh tokens, id tokens, authorization codes, user codes, browser callback values, or auth file contents.

## Implementation Order

1. Build the HTTP/runtime traits and fake adapters.
2. Implement user-code request parsing and errors.
3. Implement polling with deterministic timeout behavior.
4. Implement authorization-code exchange with form encoding.
5. Add secret-redacting debug and errors.
6. Add auth-store load/save with atomic writes and permission hardening.
7. Integrate with the provider boundary only after storage and tests are stable.
8. Add CLI/login orchestration that prints only verification instructions and success/failure status.
9. Add live validation evidence only through a segregated, secret-free path.
