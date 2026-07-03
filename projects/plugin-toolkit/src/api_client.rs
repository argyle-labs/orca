//! Composable builder for the `reqwest::Client` that plugins inject into
//! their progenitor-generated typed clients.
//!
//! Every codegen plugin needs the same wire-layer recipe: a base
//! reqwest::Client carrying auth headers by default, with optional
//! self-signed-cert acceptance (homelab) and shared timeouts. Centralising
//! it here pays off the moment a second plugin lands on this pattern
//! (proxmox was first; *arr is next) — bug fixes and TLS defaults
//! propagate from one place, and plugin authors stop touching reqwest.
//!
//! Usage from a plugin:
//!
//! ```rust,ignore
//! let http = ApiClientBuilder::new()
//!     .header("authorization", format!("PVEAPIToken={tid}={tsec}"))?
//!     .insecure(cfg.insecure)
//!     .build()?;
//! generated::Client::new_with_client(&base_url, http)
//! ```
//!
//! Sensitive header values are flagged with `set_sensitive(true)` so a
//! stray reqwest debug print can't leak them.

use crate::logging::Redacted;
use anyhow::{Context, Result, bail};
use reqwest::{
    Client, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use std::fmt;
use std::time::Duration;

/// Shared connect+request timeout for plugin-built HTTP clients. Matches
/// progenitor's generated default so swapping a typed-client default
/// constructor for `new_with_client` is behaviour-preserving.
pub const DEFAULT_TIMEOUT_SECS: u64 = 15;

/// Composable `reqwest::Client` builder for plugin transports.
///
/// Holds the set of headers to send by default, a self-signed-cert
/// allow flag, and a uniform timeout. `build()` produces a
/// `reqwest::Client` ready to hand to a progenitor-generated
/// `Client::new_with_client`.
#[derive(Debug)]
pub struct ApiClientBuilder {
    headers: HeaderMap,
    insecure: bool,
    timeout: Duration,
    cookie_store: bool,
}

impl Default for ApiClientBuilder {
    fn default() -> Self {
        Self {
            headers: HeaderMap::new(),
            insecure: false,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            cookie_store: false,
        }
    }
}

impl ApiClientBuilder {
    /// New builder with no headers, TLS verification on, default timeout.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a default header sent on every request. Marks the value
    /// sensitive so debug formatting can't leak it. Returns `Err` if the
    /// header name or value contains bytes that aren't valid in an HTTP
    /// header (newlines, NULs, etc.) — the caller's config should be
    /// validated upstream, this is the final wire guard.
    pub fn header(mut self, name: &str, value: impl AsRef<str>) -> Result<Self> {
        let name = HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid HTTP header name: {name:?}"))?;
        let mut v = HeaderValue::from_str(value.as_ref())
            .with_context(|| format!("invalid HTTP header value for {name}"))?;
        v.set_sensitive(true);
        self.headers.insert(name, v);
        Ok(self)
    }

    /// `Authorization: Bearer <token>` shorthand.
    pub fn bearer(self, token: impl AsRef<str>) -> Result<Self> {
        self.header("authorization", format!("Bearer {}", token.as_ref()))
    }

    /// Skip TLS certificate verification. Required for homelab Proxmox
    /// nodes and any other endpoint behind a self-signed cert.
    pub fn insecure(mut self, on: bool) -> Self {
        self.insecure = on;
        self
    }

    /// Override the connect+request timeout. Defaults to
    /// `DEFAULT_TIMEOUT_SECS`.
    pub fn timeout(mut self, dur: Duration) -> Self {
        self.timeout = dur;
        self
    }

    /// Persist cookies across requests. Required for session-cookie
    /// auth flows (e.g. *arr `/login` form post).
    pub fn cookie_store(mut self, on: bool) -> Self {
        self.cookie_store = on;
        self
    }

    /// Materialise the `reqwest::Client`.
    pub fn build(self) -> Result<Client> {
        // reqwest links `rustls-no-provider`, so a process-default rustls
        // `CryptoProvider` must be installed before the first `Client::build`
        // or reqwest panics (non-unwinding → aborts the host process). orca
        // core installs one at startup, but a plugin cdylib carries its own
        // copy of rustls' statics: the host's install never reaches the
        // plugin's provider slot. Install one here, idempotently, so every
        // plugin building a client through this toolkit is self-sufficient.
        ensure_crypto_provider();
        let Self {
            headers,
            insecure,
            timeout,
            cookie_store,
        } = self;
        Client::builder()
            .default_headers(headers)
            .danger_accept_invalid_certs(insecure)
            .connect_timeout(timeout)
            .timeout(timeout)
            .cookie_store(cookie_store)
            .build()
            .context("build reqwest client")
    }
}

/// Install a process-default rustls `CryptoProvider` if none is set yet.
///
/// Idempotent and cheap: `install_default` returns `Err` when a provider is
/// already installed (by orca core or a prior call), which we deliberately
/// ignore. The point is only to guarantee *some* provider exists in *this*
/// cdylib's rustls instance before reqwest tries to build a TLS client.
fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // A racing thread may install between the check and here; `install_default`
        // then returns Err (already installed). Either way a provider now exists,
        // which is all we need — discard the result via `.ok()`.
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
    }
}

// ── Response envelope unwrapping ─────────────────────────────────────────────

/// A `ClientHooks::exec` implementation that transparently strips a JSON
/// response envelope before the progenitor client deserializes the body.
///
/// Many appliance APIs wrap every payload in a single-key object — Proxmox VE
/// answers `{"data": <payload>}` for *every* endpoint, so the OpenAPI schema
/// (which describes only the inner `<payload>`) never matches the wire body and
/// deserialization fails. Rather than modelling the envelope in each spec and
/// forcing `.data` at every call site, plugins opt in at codegen time
/// (`plugin_toolkit_build::openapi::generate_all_unwrapping`) and the generated
/// client routes `exec` through here. The generated types stay the plain inner
/// shape and no plugin code touches the envelope.
///
/// Defensive by construction: only a **successful**, **JSON**, **object** body
/// that actually carries `key` is unwrapped. Errors, non-JSON, arrays, and
/// bodies missing the key pass through byte-for-byte, so error extraction still
/// sees the upstream envelope.
#[allow(clippy::disallowed_types)] // transport-layer body rewrite: the wrapper is arbitrary JSON
pub async fn exec_with_unwrapper<F>(
    client: &Client,
    request: reqwest::Request,
    unwrap: F,
) -> reqwest::Result<reqwest::Response>
where
    F: FnOnce(serde_json::Value) -> Option<serde_json::Value>,
{
    let resp = client.execute(request).await?;

    let is_json = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.contains("json"));
    if !resp.status().is_success() || !is_json {
        return Ok(resp);
    }

    let status = resp.status();
    let mut headers = resp.headers().clone();
    let body = resp.bytes().await?;

    let unwrapped: Option<Vec<u8>> = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(unwrap)
        .and_then(|inner| serde_json::to_vec(&inner).ok());

    let new_body = match unwrapped {
        Some(b) => b,
        None => return Ok(rebuild(status, headers, body.to_vec())),
    };
    // The rewritten body has a different length; drop the stale Content-Length
    // so the rebuilt response is self-consistent.
    headers.remove(reqwest::header::CONTENT_LENGTH);
    Ok(rebuild(status, headers, new_body))
}

/// Convenience [`exec_with_unwrapper`] for the common `{ "<key>": <inner> }`
/// envelope (Proxmox VE's `{"data": …}`, many others). A plugin that peels a
/// single top-level key hands codegen a one-line unwrapper:
///
/// ```rust,ignore
/// pub async fn unwrap_data(
///     client: &::plugin_toolkit::api_client::Client,
///     request: ::plugin_toolkit::reqwest::Request,
/// ) -> ::plugin_toolkit::reqwest::Result<::plugin_toolkit::reqwest::Response> {
///     ::plugin_toolkit::api_client::exec_unwrap_envelope(client, request, "data").await
/// }
/// ```
#[allow(clippy::disallowed_types)] // transport-layer body rewrite: the wrapper is arbitrary JSON
pub async fn exec_unwrap_envelope(
    client: &Client,
    request: reqwest::Request,
    key: &str,
) -> reqwest::Result<reqwest::Response> {
    exec_with_unwrapper(client, request, |v| match v {
        serde_json::Value::Object(mut m) => m.remove(key),
        _ => None,
    })
    .await
}

/// Reassemble a `reqwest::Response` from parts + an owned body. Used only by
/// [`exec_with_unwrapper`]; the progenitor client reads status, headers, and
/// bytes, so dropping the original URL/extensions is immaterial.
fn rebuild(status: reqwest::StatusCode, headers: HeaderMap, body: Vec<u8>) -> reqwest::Response {
    let mut builder = http::Response::builder().status(status);
    if let Some(dst) = builder.headers_mut() {
        *dst = headers;
    }
    reqwest::Response::from(
        builder
            .body(body)
            .expect("status + validated headers form a valid response"),
    )
}

// ── Credentials ────────────────────────────────────────────────────────────

/// Static API key (e.g. *arr's `config.xml > ApiKey`, ntfy access token).
/// Wraps the secret in [`Redacted`] so `Debug` never reveals it and memory
/// is zeroed on drop.
pub struct ApiKey(Redacted<String>);

impl ApiKey {
    pub fn new(key: String) -> Self {
        Self(Redacted::new(key))
    }

    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ApiKey").field(&self.0).finish()
    }
}

/// Browser-style username + password credentials for form-based login
/// flows (e.g. *arr `/login`). Password is wrapped in [`Redacted`].
pub struct Credentials {
    pub username: String,
    password: Redacted<String>,
}

impl Credentials {
    pub fn new(username: String, password: String) -> Self {
        Self {
            username,
            password: Redacted::new(password),
        }
    }

    pub fn expose_password(&self) -> &str {
        self.password.expose()
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

// ── Multipart form login ───────────────────────────────────────────────────

/// Opaque result of a successful password login. Holds the underlying
/// `reqwest::Client` whose cookie jar now carries the session cookie.
/// Hand `into_inner()` to a progenitor-generated `Client::new_with_client`
/// to make authenticated calls.
pub struct LoginSession {
    client: Client,
}

impl LoginSession {
    pub fn into_inner(self) -> Client {
        self.client
    }

    pub fn client(&self) -> &Client {
        &self.client
    }
}

/// Post `username` + `password` (+ `rememberMe=on`) as `multipart/form-data`
/// to `<base_url>/<login_path>`. Mirrors the *arr web-UI login wire format.
/// On 2xx or 3xx, returns a [`LoginSession`] whose client carries the
/// session cookie.
pub async fn multipart_form_login(
    base_url: &str,
    login_path: &str,
    creds: &Credentials,
) -> Result<LoginSession> {
    let client = ApiClientBuilder::new().cookie_store(true).build()?;
    let url = Url::parse(base_url)
        .and_then(|u| u.join(login_path))
        .context("invalid base_url")?;
    let form = reqwest::multipart::Form::new()
        .text("username", creds.username.clone())
        .text("password", creds.expose_password().to_string())
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
            "login failed: HTTP {} from /{login_path} (body bytes: {})",
            status,
            resp.bytes().await.map(|b| b.len()).unwrap_or(0)
        );
    }
    Ok(LoginSession { client })
}

#[cfg(test)]
#[allow(clippy::disallowed_types)] // tests build arbitrary JSON envelopes with serde_json::Value
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn install_crypto() {
        // reqwest links rustls-no-provider; tests must install a default
        // provider before constructing a client. Idempotent.
        _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn rejects_header_name_with_invalid_bytes() {
        let err = ApiClientBuilder::new()
            .header("X-Bad Name", "ok")
            .unwrap_err();
        assert!(err.to_string().contains("invalid HTTP header name"));
    }

    #[test]
    fn rejects_header_value_with_newline() {
        let err = ApiClientBuilder::new()
            .header("authorization", "Bearer abc\nXSS")
            .unwrap_err();
        assert!(err.to_string().contains("invalid HTTP header value"));
    }

    #[test]
    fn builds_with_no_headers() {
        install_crypto();
        assert!(ApiClientBuilder::new().build().is_ok());
    }

    #[tokio::test]
    async fn header_is_attached_to_outgoing_requests() {
        install_crypto();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/probe"))
            .and(header("x-api-key", "abc-123"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let http = ApiClientBuilder::new()
            .header("x-api-key", "abc-123")
            .unwrap()
            .build()
            .unwrap();
        let r = http
            .get(format!("{}/probe", server.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test]
    async fn unwraps_data_envelope_on_success_json() {
        install_crypto();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data": {"version": "9.1.9"}})),
            )
            .mount(&server)
            .await;
        let client = ApiClientBuilder::new().build().unwrap();
        let req = client.get(format!("{}/v", server.uri())).build().unwrap();
        let resp = exec_unwrap_envelope(&client, req, "data").await.unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body, serde_json::json!({"version": "9.1.9"}));
    }

    #[tokio::test]
    async fn unwraps_data_envelope_wrapping_an_array() {
        install_crypto();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/list"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [1, 2, 3]})),
            )
            .mount(&server)
            .await;
        let client = ApiClientBuilder::new().build().unwrap();
        let req = client
            .get(format!("{}/list", server.uri()))
            .build()
            .unwrap();
        let resp = exec_unwrap_envelope(&client, req, "data").await.unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body, serde_json::json!([1, 2, 3]));
    }

    #[tokio::test]
    async fn passes_through_body_without_the_key() {
        install_crypto();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/raw"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"version": "1.0"})),
            )
            .mount(&server)
            .await;
        let client = ApiClientBuilder::new().build().unwrap();
        let req = client.get(format!("{}/raw", server.uri())).build().unwrap();
        let resp = exec_unwrap_envelope(&client, req, "data").await.unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body, serde_json::json!({"version": "1.0"}));
    }

    #[tokio::test]
    async fn passes_through_error_responses_untouched() {
        install_crypto();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/err"))
            .respond_with(
                ResponseTemplate::new(500).set_body_json(serde_json::json!({"data": "ignored"})),
            )
            .mount(&server)
            .await;
        let client = ApiClientBuilder::new().build().unwrap();
        let req = client.get(format!("{}/err", server.uri())).build().unwrap();
        let resp = exec_unwrap_envelope(&client, req, "data").await.unwrap();
        assert_eq!(resp.status(), 500);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body, serde_json::json!({"data": "ignored"}));
    }

    #[tokio::test]
    async fn bearer_shorthand_attaches_authorization_header() {
        install_crypto();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/probe"))
            .and(header("authorization", "Bearer tok-xyz"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let http = ApiClientBuilder::new()
            .bearer("tok-xyz")
            .unwrap()
            .build()
            .unwrap();
        let r = http
            .get(format!("{}/probe", server.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
}
