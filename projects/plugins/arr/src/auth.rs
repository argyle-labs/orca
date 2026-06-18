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
//! Wire-layer primitives (`ApiKey`, `Credentials`, `LoginSession`,
//! `multipart_form_login`, `ApiClientBuilder`) live in
//! `plugin_toolkit::api_client` per
//! `[[feedback-plugin-toolkit-is-the-gateway]]`. This file holds only the
//! *arr-specific composition.

use anyhow::{Context, Result};
use plugin_toolkit::api_client::{ApiClientBuilder, ApiKey, Credentials, LoginSession};

/// Build the `reqwest::Client` the future `arr.*` orca tools hand to
/// `<flavor>::Client::new_with_client(base_url, client)`. Carries
/// `X-Api-Key: <key>` on every request.
pub(crate) fn reqwest_client_with_api_key(key: &ApiKey) -> Result<reqwest::Client> {
    ApiClientBuilder::new()
        .header("x-api-key", key.expose())
        .context("api key contains invalid bytes")?
        .build()
}

/// Post `Credentials` to `<base_url>/login` as `multipart/form-data` —
/// the exact wire format the *arr web UIs use.
pub(crate) async fn login_with_password(
    base_url: &str,
    creds: &Credentials,
) -> Result<LoginSession> {
    plugin_toolkit::api_client::multipart_form_login(base_url, "login", creds).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn install_crypto() {
        _ = rustls::crypto::ring::default_provider().install_default();
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
        let _ = session.into_inner();
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
