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
use plugin_toolkit::logging::Redacted;
use reqwest::{Client, Url};
use std::fmt;

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
    plugin_toolkit::api_client::ApiClientBuilder::new()
        .header("x-api-key", key.0.expose())
        .context("api key contains invalid bytes")?
        .build()
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

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn install_crypto() {
        // reqwest is built against rustls-no-provider; tests must install one
        // before constructing any client. Idempotent across tests.
        _ = rustls::crypto::ring::default_provider().install_default();
    }

    // zeroize behaviour is covered upstream by
    // `plugin_toolkit::logging::tests::zeroize_string_clears_buffer`.

    #[test]
    fn redacted_debug_hides_value() {
        let r = Redacted::new(String::from("topsecret"));
        let dbg = format!("{r:?}");
        assert_eq!(dbg, "Redacted(***)");
        assert!(!dbg.contains("topsecret"));
        assert_eq!(r.expose(), "topsecret");
    }

    #[test]
    fn api_key_debug_redacts_inner_value() {
        let k = ApiKey::new("abc123".into());
        let s = format!("{k:?}");
        assert!(s.contains("ApiKey"));
        assert!(s.contains("***"));
        assert!(!s.contains("abc123"));
    }

    #[test]
    fn credentials_debug_redacts_password_but_keeps_username() {
        let c = Credentials::new("scott".into(), "hunter2".into());
        let s = format!("{c:?}");
        assert!(s.contains("scott"));
        assert!(!s.contains("hunter2"));
        assert!(s.contains("***"));
    }

    #[test]
    fn reqwest_client_with_api_key_builds_for_ascii_key() {
        install_crypto();
        let k = ApiKey::new("abc-123".into());
        assert!(reqwest_client_with_api_key(&k).is_ok());
    }

    #[test]
    fn reqwest_client_with_api_key_rejects_invalid_header_bytes() {
        // Newlines aren't valid header values.
        let k = ApiKey::new("bad\nkey".into());
        let err = reqwest_client_with_api_key(&k).unwrap_err();
        assert!(err.to_string().contains("api key"));
    }

    #[tokio::test]
    async fn login_with_password_invalid_base_url_errors() {
        install_crypto();
        let creds = Credentials::new("u".into(), "p".into());
        let err = login_with_password("not a url", &creds)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("invalid base_url"));
    }

    #[tokio::test]
    async fn login_with_password_succeeds_on_2xx() {
        install_crypto();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let creds = Credentials::new("scott".into(), "pw".into());
        let base = format!("{}/", server.uri());
        let session = login_with_password(&base, &creds).await.unwrap();
        // Client was returned with cookie store; just check it exists.
        let _ = session.client;
    }

    #[tokio::test]
    async fn login_with_password_succeeds_on_redirect() {
        install_crypto();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login"))
            .respond_with(ResponseTemplate::new(302))
            .mount(&server)
            .await;
        let creds = Credentials::new("scott".into(), "pw".into());
        let base = format!("{}/", server.uri());
        login_with_password(&base, &creds).await.unwrap();
    }

    #[tokio::test]
    async fn login_with_password_bails_on_4xx() {
        install_crypto();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login"))
            .respond_with(ResponseTemplate::new(401).set_body_bytes(b"nope" as &[u8]))
            .mount(&server)
            .await;
        let creds = Credentials::new("scott".into(), "wrong".into());
        let base = format!("{}/", server.uri());
        let err = login_with_password(&base, &creds).await.err().unwrap();
        let msg = err.to_string();
        assert!(msg.contains("login failed"));
        assert!(msg.contains("401"));
    }
}
