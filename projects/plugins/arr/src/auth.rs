//! Authentication helpers for every *arr flavor. All five forks share the
//! same auth surface, so one module covers sonarr/radarr/prowlarr/lidarr/
//! readarr:
//!
//! - **API key** (canonical, used by orca): static per-instance key the
//!   server reads from `config.xml`. Send it as the `X-Api-Key` header on
//!   every request. Use [`reqwest_client_with_api_key`] to build a
//!   `reqwest::Client` that injects the header by default; pass it into
//!   any `<flavor>::Client::new_with_client(base_url, ...)`.
//!
//! - **Username + password** (browser-equivalent): multipart POST to
//!   `/login`, server responds with an auth cookie that future requests
//!   carry. Use [`login_with_password`] when you only have credentials.
//!   The generated `post_login` fn can't set the `multipart/form-data`
//!   `Content-Type` header — its body is rewritten to
//!   `application/octet-stream` by `openapi::normalize` so progenitor
//!   could codegen it. This helper bypasses that and posts a real form.
//!
//! Credential storage is the orca-wide auth system's job (see
//! `project_orca_login_local_auth`); this module only handles the
//! transport.

use anyhow::{Context, Result, bail};
use reqwest::{Client, Url, header};

/// Static *arr API key (the value in `config.xml > ApiKey`).
#[derive(Clone, Debug)]
pub struct ApiKey(pub String);

/// Browser-style credentials posted to `/login`.
#[derive(Clone, Debug)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// Build a `reqwest::Client` that sends `X-Api-Key: <key>` by default on
/// every request. Pass the result to
/// `<flavor>::Client::new_with_client(base_url, client)`.
pub fn reqwest_client_with_api_key(key: &ApiKey) -> Result<Client> {
    let mut headers = header::HeaderMap::new();
    let mut v = header::HeaderValue::from_str(&key.0).context("api key contains invalid bytes")?;
    v.set_sensitive(true);
    headers.insert("X-Api-Key", v);
    Client::builder()
        .default_headers(headers)
        .build()
        .context("build reqwest client")
}

/// Result of a successful password login: the cookie store on the
/// returned client now carries the session cookie. Reuse this client for
/// subsequent API calls via `<flavor>::Client::new_with_client`.
pub struct LoginSession {
    pub client: Client,
}

/// Post `Credentials` to `<base_url>/login` as `multipart/form-data` —
/// the exact wire format the *arr web UIs use. On success the returned
/// client carries the auth cookie. `base_url` should be the same value
/// you pass to `Client::new` (e.g. `https://sonarr.local`).
pub async fn login_with_password(base_url: &str, creds: &Credentials) -> Result<LoginSession> {
    let client = Client::builder()
        .cookie_store(true)
        .build()
        .context("build reqwest client")?;
    let url = Url::parse(base_url)
        .and_then(|u| u.join("login"))
        .context("invalid base_url")?;
    let form = reqwest::multipart::Form::new()
        .text("username", creds.username.clone())
        .text("password", creds.password.clone())
        // *arr's login form posts a hidden `rememberMe` to keep the
        // cookie sticky; mirror that.
        .text("rememberMe", "on");
    let resp = client
        .post(url)
        .multipart(form)
        .send()
        .await
        .context("login request failed")?;
    let status = resp.status();
    if !status.is_success() && !status.is_redirection() {
        bail!(
            "login failed: HTTP {} from /login (body bytes: {})",
            status,
            resp.bytes().await.map(|b| b.len()).unwrap_or(0)
        );
    }
    Ok(LoginSession { client })
}
