//! Native HTTP client primitive for orca.
//!
//! Replaces the former `rest` plugin. Exposes a `Client` with the same surface
//! (get / post / put / patch / delete + per-request headers, query, body,
//! insecure, timeout) and returns a structured `Response` with parsed
//! JSON when applicable.
//!
//! Used by the integration crates (dockge, proxmox, ntfy, ...) so HTTP bug
//! fixes land in one place.
//!
//! # Example
//!
//! ```no_run
//! # async fn doit() -> anyhow::Result<()> {
//! let client = utils::http::Client::new();
//! let resp = client
//!     .get("https://api.example.com/items")
//!     .header("Authorization", "Bearer xyz")
//!     .query("page", "1")
//!     .send()
//!     .await?;
//! let items: Vec<serde_json::Value> = resp.json()?;
//! # Ok(()) }
//! ```
//!
//! `serde_json::Value` is used in `ResponseBody::Json` and `Body::Json`
//! because HTTP response bodies are upstream-controlled — their shape is not
//! known at this layer. Callers downcast via `Response::json::<T>()`.
#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::OnceCell;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Connection-establishment cap applied to every pooled client (secure and
/// insecure). Bounds a dead endpoint to a fast failure without capping the
/// total request — streaming callers ([`RequestBuilder::send_stream`]) need
/// an unbounded body but still want a bounded *connect*.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Mirror of the rest plugin's 8 MiB response cap. Larger responses are
/// rejected with `HttpError::ResponseTooLarge`.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("decode response: {0}")]
    Decode(String),
    #[error("response body exceeded {MAX_RESPONSE_BYTES} bytes")]
    ResponseTooLarge,
    #[error("http {status}: {summary}")]
    Status {
        status: u16,
        summary: String,
        response: Box<Response>,
    },
}

/// HTTP client with a composable base configuration. Internally pools two
/// `reqwest::Client`s — one that verifies TLS and one that does not — built
/// lazily from the config on first use of each. Cheap to clone (`Arc` inside).
///
/// [`Client::new`] uses the shared defaults; [`Client::builder`] composes a
/// client that supports the same client-level knobs a hand-rolled
/// `reqwest::Client` would (notably a short probe `connect_timeout`), so
/// callers never need to drop down to raw reqwest. Per-request knobs (total
/// timeout, insecure, headers, body) compose on top via [`RequestBuilder`].
#[derive(Clone, Default)]
pub struct Client {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    config: ClientConfig,
    secure: OnceCell<reqwest::Client>,
    insecure: OnceCell<reqwest::Client>,
}

/// Client-level transport settings composed into a [`Client`]. These cannot
/// vary per request (reqwest builds them into the pooled client); per-request
/// options live on [`RequestBuilder`].
#[derive(Clone)]
struct ClientConfig {
    connect_timeout: Duration,
    pool_max_idle_per_host: usize,
    pool_idle_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: CONNECT_TIMEOUT,
            pool_max_idle_per_host: 8,
            pool_idle_timeout: Duration::from_secs(30),
        }
    }
}

/// Composable builder for [`Client`] — start from [`Client::builder`], layer
/// only the settings you need, then [`build`](ClientBuilder::build).
#[derive(Default)]
pub struct ClientBuilder {
    config: ClientConfig,
}

impl ClientBuilder {
    /// Cap connection establishment. Bounds a dead endpoint to a fast failure
    /// without capping the (possibly streaming) request body; use a short
    /// value such as `Duration::from_millis(500)` for reachability probes.
    pub fn connect_timeout(mut self, d: Duration) -> Self {
        self.config.connect_timeout = d;
        self
    }

    /// Max idle connections retained per host in the pool.
    pub fn pool_max_idle_per_host(mut self, n: usize) -> Self {
        self.config.pool_max_idle_per_host = n;
        self
    }

    /// How long an idle pooled connection is kept before being dropped.
    pub fn pool_idle_timeout(mut self, d: Duration) -> Self {
        self.config.pool_idle_timeout = d;
        self
    }

    pub fn build(self) -> Client {
        ensure_crypto_provider();
        Client {
            inner: Arc::new(Inner {
                config: self.config,
                secure: OnceCell::new(),
                insecure: OnceCell::new(),
            }),
        }
    }
}

/// Install the process-global rustls **ring** crypto provider, idempotently.
///
/// reqwest's `rustls-no-provider` feature needs a default provider set before
/// the first client is built, or TLS panics with "no process-level
/// CryptoProvider". This is the one shared home for that install — call it
/// from any entrypoint that might reach HTTPS first (a model backend, a test,
/// an early probe) instead of re-implementing the `install_default` dance.
/// `Client::new` calls it for you; standalone callers that build their own
/// `reqwest::Client` (e.g. for streaming specifics) can call it directly.
pub fn ensure_crypto_provider() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
}

impl Client {
    pub fn new() -> Self {
        ensure_crypto_provider();
        Self::default()
    }

    /// Compose a client with non-default transport settings (see
    /// [`ClientBuilder`]). For everything else, prefer [`Client::new`].
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    pub fn get(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(reqwest::Method::GET, url)
    }
    pub fn post(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(reqwest::Method::POST, url)
    }
    pub fn put(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(reqwest::Method::PUT, url)
    }
    pub fn patch(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(reqwest::Method::PATCH, url)
    }
    pub fn delete(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(reqwest::Method::DELETE, url)
    }

    /// Start a request for an arbitrary method named by string (`"GET"`,
    /// `"POST"`, custom verbs). The seam the `http.request` capability proxy
    /// uses to relay a delegating plugin's method without the plugin naming
    /// `reqwest::Method`. Errors on a malformed method token.
    pub fn request_str(
        &self,
        method: &str,
        url: impl Into<String>,
    ) -> Result<RequestBuilder, HttpError> {
        let m = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| HttpError::InvalidUrl(format!("bad HTTP method '{method}'")))?;
        Ok(self.request(m, url))
    }

    fn request(&self, method: reqwest::Method, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder {
            client: self.clone(),
            method,
            url: url.into(),
            headers: HashMap::new(),
            query: Vec::new(),
            body: None,
            insecure: false,
            timeout: DEFAULT_TIMEOUT,
            max_body: None,
        }
    }

    async fn pool(&self, insecure: bool) -> Result<reqwest::Client, HttpError> {
        let cell = if insecure {
            &self.inner.insecure
        } else {
            &self.inner.secure
        };
        let cfg = self.inner.config.clone();
        cell.get_or_try_init(|| async move {
            // reqwest (rustls + ring, no aws-lc) panics `No provider set`
            // unless a process-default crypto provider is installed before a
            // client is built. Production entrypoints install ring at startup,
            // but anything that reaches HTTP first (tests, early init, a plugin
            // probe) would otherwise panic — and which runs first is not
            // guaranteed. Install it idempotently here, the one chokepoint every
            // `utils::http` client funnels through, so HTTP is self-healing
            // regardless of init order.
            ensure_crypto_provider();
            let mut b = reqwest::Client::builder()
                // Idle-pool bounds keep a daemon that fans out to many distinct
                // hostnames (mesh peers, plugin upstreams) from accumulating
                // unbounded idle pools.
                .pool_max_idle_per_host(cfg.pool_max_idle_per_host)
                .pool_idle_timeout(cfg.pool_idle_timeout)
                .connect_timeout(cfg.connect_timeout);
            if insecure {
                b = b.danger_accept_invalid_certs(true);
            }
            b.build().map_err(HttpError::from)
        })
        .await
        .cloned()
    }
}

pub struct RequestBuilder {
    client: Client,
    method: reqwest::Method,
    url: String,
    headers: HashMap<String, String>,
    query: Vec<(String, String)>,
    body: Option<Body>,
    insecure: bool,
    timeout: Duration,
    max_body: Option<usize>,
}

enum Body {
    Json(Value),
    Form(Vec<(String, String)>),
    Bytes(Vec<u8>, &'static str),
    /// Raw body with no implied `Content-Type` — the caller's own headers carry
    /// it (used by the `http.request` capability, which relays a plugin's body
    /// and Content-Type header verbatim).
    Raw(Vec<u8>),
}

impl RequestBuilder {
    pub fn header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.insert(k.into(), v.into());
        self
    }
    pub fn headers<I, K, V>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.headers
            .extend(headers.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }
    pub fn bearer(self, token: impl AsRef<str>) -> Self {
        self.header("Authorization", format!("Bearer {}", token.as_ref()))
    }
    pub fn query(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.query.push((k.into(), v.into()));
        self
    }
    pub fn json(mut self, body: impl Serialize) -> Self {
        self.body = Some(Body::Json(
            serde_json::to_value(body).unwrap_or(Value::Null),
        ));
        self
    }
    pub fn form(mut self, fields: Vec<(String, String)>) -> Self {
        self.body = Some(Body::Form(fields));
        self
    }
    pub fn bytes(mut self, b: Vec<u8>, content_type: &'static str) -> Self {
        self.body = Some(Body::Bytes(b, content_type));
        self
    }
    /// Set a raw body with no implied `Content-Type` (the caller's headers carry
    /// it). Used by the `http.request` capability to relay a plugin's body
    /// verbatim.
    pub fn raw_body(mut self, b: Vec<u8>) -> Self {
        self.body = Some(Body::Raw(b));
        self
    }
    pub fn insecure(mut self, on: bool) -> Self {
        self.insecure = on;
        self
    }
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }
    /// Override the default 8 MiB response cap. Use for legitimate large
    /// payloads (binary releases, container images, backup archives).
    pub fn max_body(mut self, n: usize) -> Self {
        self.max_body = Some(n);
        self
    }

    /// Assemble the underlying `reqwest` request shared by every terminal
    /// (`send`, `send_bytes`, `send_stream`): url parse, pooled client, query,
    /// headers, and body. `apply_timeout` is the only axis that differs — the
    /// buffered terminals cap total request time; the streaming terminal must
    /// not, or it would abort a long-lived token stream mid-flight.
    async fn build_request(
        &self,
        apply_timeout: bool,
    ) -> Result<reqwest::RequestBuilder, HttpError> {
        let parsed =
            url::Url::parse(&self.url).map_err(|e| HttpError::InvalidUrl(e.to_string()))?;
        let client = self.client.pool(self.insecure).await?;
        let mut req = client.request(self.method.clone(), parsed);
        if !self.query.is_empty() {
            req = req.query(&self.query);
        }
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        match &self.body {
            Some(Body::Json(v)) => {
                req = req.header("Content-Type", "application/json").json(v);
            }
            Some(Body::Form(f)) => {
                req = req.form(f);
            }
            Some(Body::Bytes(b, ct)) => {
                req = req.header("Content-Type", *ct).body(b.clone());
            }
            Some(Body::Raw(b)) => {
                req = req.body(b.clone());
            }
            None => {}
        }
        if apply_timeout {
            req = req.timeout(self.timeout);
        }
        Ok(req)
    }

    /// Stream the response body unbuffered. Unlike [`send`](Self::send) /
    /// [`send_bytes`](Self::send_bytes) this applies **no** `max_body` cap and
    /// **no** total-request timeout — it is the terminal for SSE / chunked
    /// upstreams (model token streams, log tails). The caller checks
    /// [`StreamResponse::status`] and drains [`StreamResponse::bytes_stream`].
    pub async fn send_stream(self) -> Result<StreamResponse, HttpError> {
        let req = self.build_request(false).await?;
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = flatten_headers(resp.headers());
        Ok(StreamResponse {
            status,
            headers,
            inner: resp,
        })
    }

    /// Send and collect the body as raw bytes. Use for binary downloads
    /// (release assets, checksums, archives). Status / headers / size cap
    /// behavior mirror [`send`](Self::send).
    pub async fn send_bytes(self) -> Result<BytesResponse, HttpError> {
        let req = self.build_request(true).await?;
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = flatten_headers(resp.headers());
        let bytes = resp.bytes().await?.to_vec();
        if bytes.len() > self.max_body.unwrap_or(MAX_RESPONSE_BYTES) {
            return Err(HttpError::ResponseTooLarge);
        }
        if !(200..300).contains(&status) {
            let summary = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]).into_owned();
            return Err(HttpError::Status {
                status,
                summary: summary.clone(),
                response: Box::new(Response {
                    status,
                    headers,
                    body: ResponseBody::Text { text: summary },
                }),
            });
        }
        Ok(BytesResponse {
            status,
            headers,
            body: bytes,
        })
    }

    /// Send and collect the body as raw bytes for **any** status — unlike
    /// [`send`](Self::send) / [`send_bytes`](Self::send_bytes) a non-2xx status
    /// is NOT an error; the caller inspects [`BytesResponse::status`] itself.
    /// This is the terminal a transparent proxy needs (the `http.request`
    /// capability): it must relay 4xx/5xx to the delegating plugin verbatim so
    /// the plugin's own client applies its status semantics. The `max_body` cap
    /// still applies.
    pub async fn send_raw(self) -> Result<BytesResponse, HttpError> {
        let max = self.max_body.unwrap_or(MAX_RESPONSE_BYTES);
        let req = self.build_request(true).await?;
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = flatten_headers(resp.headers());
        let bytes = resp.bytes().await?.to_vec();
        if bytes.len() > max {
            return Err(HttpError::ResponseTooLarge);
        }
        Ok(BytesResponse {
            status,
            headers,
            body: bytes,
        })
    }

    pub async fn send(self) -> Result<Response, HttpError> {
        let default_accept =
            !self.headers.contains_key("Accept") && !self.headers.contains_key("accept");
        let mut req = self.build_request(true).await?;
        if default_accept {
            req = req.header("Accept", "application/json");
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = flatten_headers(resp.headers());

        let bytes = resp.bytes().await?;
        if bytes.len() > self.max_body.unwrap_or(MAX_RESPONSE_BYTES) {
            return Err(HttpError::ResponseTooLarge);
        }
        let body = ResponseBody::from_bytes(&bytes);
        let response = Response {
            status,
            headers,
            body,
        };

        if !(200..300).contains(&status) {
            return Err(HttpError::Status {
                status,
                summary: response.summary(),
                response: Box::new(response),
            });
        }
        Ok(response)
    }
}

/// Response from [`RequestBuilder::send_bytes`]. Body is the raw byte stream.
#[derive(Debug, Clone)]
pub struct BytesResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// Response from [`RequestBuilder::send_stream`] — status + headers available
/// immediately, body consumed lazily as a chunk stream. Unlike the buffered
/// responses this does **not** error on a non-2xx status: a streaming caller
/// inspects [`status`](Self::status) itself and, on failure, drains
/// [`text`](Self::text) for the error body before deciding what to do.
pub struct StreamResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    inner: reqwest::Response,
}

impl StreamResponse {
    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Drain the entire body to a String. For error bodies on a non-2xx
    /// status — do not use on a long-lived stream you mean to consume chunk
    /// by chunk (that is what [`bytes_stream`](Self::bytes_stream) is for).
    pub async fn text(self) -> Result<String, HttpError> {
        Ok(self.inner.text().await?)
    }

    /// The unbuffered body as a stream of byte chunks. Each item is one
    /// transport chunk or a transport error; no `max_body` cap applies.
    pub fn bytes_stream(self) -> impl Stream<Item = Result<Vec<u8>, HttpError>> {
        self.inner
            .bytes_stream()
            .map(|r| r.map(|b| b.to_vec()).map_err(HttpError::from))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub status: u16,
    pub headers: HashMap<String, String>,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseBody {
    Json { json: Value },
    Text { text: String },
}

impl ResponseBody {
    fn from_bytes(b: &[u8]) -> Self {
        if !b.is_empty()
            && let Ok(v) = serde_json::from_slice::<Value>(b)
        {
            return ResponseBody::Json { json: v };
        }
        ResponseBody::Text {
            text: String::from_utf8_lossy(b).into_owned(),
        }
    }
}

impl Response {
    pub fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, HttpError> {
        match &self.body {
            ResponseBody::Json { json } => {
                serde_json::from_value(json.clone()).map_err(|e| HttpError::Decode(e.to_string()))
            }
            ResponseBody::Text { text } => {
                serde_json::from_str(text).map_err(|e| HttpError::Decode(e.to_string()))
            }
        }
    }
    pub fn text(&self) -> String {
        match &self.body {
            ResponseBody::Json { json } => json.to_string(),
            ResponseBody::Text { text } => text.clone(),
        }
    }
    fn summary(&self) -> String {
        let s = self.text();
        if s.len() <= 256 {
            s
        } else {
            format!("{}…", &s[..256])
        }
    }
}

fn flatten_headers(h: &reqwest::header::HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::with_capacity(h.len());
    for (k, v) in h {
        let v = v.to_str().unwrap_or("").to_string();
        out.entry(k.as_str().to_string())
            .and_modify(|prev: &mut String| {
                prev.push_str(", ");
                prev.push_str(&v);
            })
            .or_insert(v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn get_returns_parsed_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/items"))
            .and(query_param("page", "1"))
            .and(header("Authorization", "Bearer abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let c = Client::new();
        let r = c
            .get(format!("{}/items", server.uri()))
            .bearer("abc")
            .query("page", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        let v: serde_json::Value = r.json().unwrap();
        assert_eq!(v["ok"], true);
    }

    #[tokio::test]
    async fn post_with_json_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/items"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 7})))
            .mount(&server)
            .await;
        let r = Client::new()
            .post(format!("{}/items", server.uri()))
            .json(serde_json::json!({"name": "a"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status, 201);
    }

    #[tokio::test]
    async fn non_2xx_returns_status_error_with_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let err = Client::new()
            .get(format!("{}/missing", server.uri()))
            .send()
            .await
            .unwrap_err();
        match err {
            HttpError::Status {
                status, response, ..
            } => {
                assert_eq!(status, 404);
                assert_eq!(response.status, 404);
                assert!(matches!(response.body, ResponseBody::Text { .. }));
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn put_patch_delete_dispatch_correct_methods() {
        let server = MockServer::start().await;
        for m in ["PUT", "PATCH", "DELETE"] {
            Mock::given(method(m))
                .and(path("/resource"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"m": m})))
                .mount(&server)
                .await;
        }
        let c = Client::new();
        let url = format!("{}/resource", server.uri());
        assert_eq!(c.put(&url).send().await.unwrap().status, 200);
        assert_eq!(c.patch(&url).send().await.unwrap().status, 200);
        assert_eq!(c.delete(&url).send().await.unwrap().status, 200);
    }

    #[tokio::test]
    async fn headers_iter_applies_multiple_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/h"))
            .and(header("X-A", "1"))
            .and(header("X-B", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;
        let r = Client::new()
            .get(format!("{}/h", server.uri()))
            .headers([("X-A", "1"), ("X-B", "2")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn form_body_is_url_encoded() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/f"))
            .and(wiremock::matchers::body_string("k=v&x=y"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        let r = Client::new()
            .post(format!("{}/f", server.uri()))
            .form(vec![("k".into(), "v".into()), ("x".into(), "y".into())])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn bytes_body_uses_supplied_content_type() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/b"))
            .and(header("Content-Type", "application/octet-stream"))
            .and(wiremock::matchers::body_bytes(vec![1u8, 2, 3]))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        let r = Client::new()
            .post(format!("{}/b", server.uri()))
            .bytes(vec![1, 2, 3], "application/octet-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn invalid_url_returns_invalid_url_error() {
        let err = Client::new().get("not a url").send().await.unwrap_err();
        assert!(matches!(err, HttpError::InvalidUrl(_)));
    }

    #[tokio::test]
    async fn response_too_large_when_body_exceeds_cap() {
        let server = MockServer::start().await;
        let big = "x".repeat(1024);
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_string(big))
            .mount(&server)
            .await;
        let err = Client::new()
            .get(format!("{}/big", server.uri()))
            .max_body(100)
            .send()
            .await
            .unwrap_err();
        assert!(matches!(err, HttpError::ResponseTooLarge));
    }

    #[tokio::test]
    async fn send_bytes_returns_raw_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![9u8, 8, 7]))
            .mount(&server)
            .await;
        let r = Client::new()
            .get(format!("{}/bin", server.uri()))
            .send_bytes()
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, vec![9, 8, 7]);
    }

    #[tokio::test]
    async fn send_bytes_status_error_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nope"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let err = Client::new()
            .get(format!("{}/nope", server.uri()))
            .send_bytes()
            .await
            .unwrap_err();
        match err {
            HttpError::Status { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_bytes_too_large_when_over_cap() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big2"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 1024]))
            .mount(&server)
            .await;
        let err = Client::new()
            .get(format!("{}/big2", server.uri()))
            .max_body(10)
            .send_bytes()
            .await
            .unwrap_err();
        assert!(matches!(err, HttpError::ResponseTooLarge));
    }

    #[tokio::test]
    async fn insecure_and_timeout_builders_compose() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;
        let r = Client::new()
            .get(format!("{}/x", server.uri()))
            .insecure(true)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status, 200);
    }

    #[test]
    fn response_summary_truncates_long_text() {
        let long = "a".repeat(300);
        let r = Response {
            status: 200,
            headers: HashMap::new(),
            body: ResponseBody::Text { text: long },
        };
        let s = r.summary();
        // 256 chars + the ellipsis.
        assert!(s.ends_with('…'));
        assert_eq!(s.chars().count(), 257);
    }

    #[test]
    fn response_json_decodes_text_body_holding_json() {
        let r = Response {
            status: 200,
            headers: HashMap::new(),
            body: ResponseBody::Text {
                text: r#"{"n":42}"#.into(),
            },
        };
        let v: serde_json::Value = r.json().unwrap();
        assert_eq!(v["n"], 42);
    }

    #[test]
    fn response_json_decode_error_when_invalid() {
        let r = Response {
            status: 200,
            headers: HashMap::new(),
            body: ResponseBody::Text {
                text: "not json".into(),
            },
        };
        let res: Result<serde_json::Value, _> = r.json();
        assert!(matches!(res, Err(HttpError::Decode(_))));
    }

    #[test]
    fn response_text_serializes_json_body() {
        let r = Response {
            status: 200,
            headers: HashMap::new(),
            body: ResponseBody::Json {
                json: serde_json::json!({"a": 1}),
            },
        };
        assert_eq!(r.text(), r#"{"a":1}"#);
    }

    #[tokio::test]
    async fn text_response_when_not_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/raw"))
            .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
            .mount(&server)
            .await;
        let r = Client::new()
            .get(format!("{}/raw", server.uri()))
            .send()
            .await
            .unwrap();
        assert!(matches!(r.body, ResponseBody::Text { .. }));
        assert_eq!(r.text(), "hello");
    }
}
