// Consumed by the future `arr.*` orca tool surface (not yet built —
// see module docs). Acknowledged dead-code until that lands.
#![allow(dead_code)]

//! Crate-internal transport primitives for *arr authentication.
//!
//! **Do not call from outside this crate.** Per
//! `feedback-canonical-model-is-load-bearing`, every credential-touching
//! affordance must go through an `#[orca_tool]` that:
//!   - resolves service coordinates via the capability transport chain
//!     (`project-capability-transport-fallbacks`),
//!   - pulls/persists creds through the orca `secrets` table with
//!     `self_secure`-gated replication
//!     (`project-secrets-replication`, `project-unified-mesh-state`),
//!   - runs under the authenticated caller identity from `orca login`
//!     (`project-orca-login-local-auth`).
//!
//! Those prereqs aren't built yet. This module is the bottom transport
//! layer the future `arr.*` orca tools will sit on top of; nothing
//! else is allowed to depend on it.
//!
//! Secret-bearing values use [`Redacted`] so a stray `Debug` print or
//! log line can't leak the key/password. Constructors are explicit;
//! plaintext access is via [`Redacted::expose`] and stays within this
//! crate's call paths.

use anyhow::{Context, Result, bail};
use reqwest::{Client, Url, header};
use std::fmt;

/// Wraps a secret-bearing value so `Debug`/`Display` never reveal it.
/// Memory is zeroed on drop. Used for API keys, passwords, and any
/// other material that must not appear in logs, error chains, or
/// process dumps.
pub(crate) struct Redacted<T: Zeroize>(T);

impl<T: Zeroize> Redacted<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &T {
        &self.0
    }
}

impl<T: Zeroize> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Redacted(***)")
    }
}

impl<T: Zeroize> Drop for Redacted<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Minimal in-crate `Zeroize` to avoid pulling the upstream crate just
/// for `String`. Overwrites the buffer with zero bytes before drop.
pub(crate) trait Zeroize {
    fn zeroize(&mut self);
}

impl Zeroize for String {
    fn zeroize(&mut self) {
        // Safety: writing zero bytes over the existing capacity is
        // valid UTF-8 (NULs are valid). Then clear the length.
        let bytes = unsafe { self.as_bytes_mut() };
        for b in bytes.iter_mut() {
            *b = 0;
        }
        self.clear();
    }
}

/// Static *arr API key (the value in `config.xml > ApiKey`).
pub(crate) struct ApiKey(Redacted<String>);

impl ApiKey {
    pub(crate) fn new(key: String) -> Self {
        Self(Redacted::new(key))
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ApiKey").field(&self.0).finish()
    }
}

/// Browser-style credentials for the `/login` form.
pub(crate) struct Credentials {
    pub(crate) username: String,
    password: Redacted<String>,
}

impl Credentials {
    pub(crate) fn new(username: String, password: String) -> Self {
        Self {
            username,
            password: Redacted::new(password),
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &self.password)
            .finish()
    }
}

/// Build a `reqwest::Client` that sends `X-Api-Key: <key>` by default
/// on every request. Intended to be passed to
/// `<flavor>::Client::new_with_client(base_url, client)` from the
/// future `arr.*` orca tools — not from user code.
pub(crate) fn reqwest_client_with_api_key(key: &ApiKey) -> Result<Client> {
    let mut headers = header::HeaderMap::new();
    let mut v =
        header::HeaderValue::from_str(key.0.expose()).context("api key contains invalid bytes")?;
    v.set_sensitive(true);
    headers.insert("X-Api-Key", v);
    Client::builder()
        .default_headers(headers)
        .build()
        .context("build reqwest client")
}

/// Result of a successful password login: the cookie store on the
/// returned client now carries the session cookie.
pub(crate) struct LoginSession {
    pub(crate) client: Client,
}

/// Post `Credentials` to `<base_url>/login` as `multipart/form-data` —
/// the exact wire format the *arr web UIs use.
pub(crate) async fn login_with_password(
    base_url: &str,
    creds: &Credentials,
) -> Result<LoginSession> {
    let client = Client::builder()
        .cookie_store(true)
        .build()
        .context("build reqwest client")?;
    let url = Url::parse(base_url)
        .and_then(|u| u.join("login"))
        .context("invalid base_url")?;
    let form = reqwest::multipart::Form::new()
        .text("username", creds.username.clone())
        .text("password", creds.password.expose().clone())
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
