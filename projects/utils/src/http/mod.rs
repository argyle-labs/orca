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
const MAX_REDIRECTS: usize = 10;
/// Default-deny: the only request headers carried to a different origin on a
/// redirect. Anything else may be a credential.
const CROSS_ORIGIN_SAFE_HEADERS: &[&str] =
    &["accept", "accept-language", "user-agent", "content-type"];

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("request failed: {0}")]
    Request(reqwest::Error),
    #[error("decode response: {0}")]
    Decode(String),
    #[error("redirect: {0}")]
    Redirect(String),
    #[error("response body exceeded {limit} bytes")]
    ResponseTooLarge { limit: usize },
    #[error("http {status}: {summary}")]
    Status {
        status: u16,
        summary: String,
        response: Box<Response>,
    },
}

// reqwest's Display/Debug embed the full request URL, query string included;
// callers put API keys in query params, so the URL is stripped at conversion.
impl From<reqwest::Error> for HttpError {
    fn from(e: reqwest::Error) -> Self {
        HttpError::Request(e.without_url())
    }
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

pub fn shared() -> &'static Client {
    static CLIENT: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(Client::new)
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

    pub fn raw_request(&self, req: RawRequest) -> Result<RequestBuilder, HttpError> {
        let mut builder = self
            .request_str(&req.method, req.url)?
            .insecure(req.insecure);
        for (k, v) in &req.headers {
            builder = builder.header(k, v);
        }
        if !req.body.is_empty() {
            builder = builder.raw_body(req.body);
        }
        if let Some(ms) = req.timeout_ms {
            builder = builder.timeout(Duration::from_millis(ms));
        }
        Ok(builder)
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
                // Redirects are followed in `RequestBuilder::dispatch`.
                .redirect(reqwest::redirect::Policy::none())
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

    /// Assemble one hop's `reqwest` request: pooled client, headers, body.
    /// `timeout` is the hop's share of the total deadline; `None` for the
    /// streaming terminal, which must not abort a long-lived token stream.
    async fn build_request(
        &self,
        method: &reqwest::Method,
        url: &url::Url,
        headers: &HashMap<String, String>,
        with_body: bool,
        timeout: Option<Duration>,
    ) -> Result<reqwest::RequestBuilder, HttpError> {
        let client = self.client.pool(self.insecure).await?;
        let mut req = client.request(method.clone(), url.clone());
        for (k, v) in headers {
            req = req.header(k, v);
        }
        match self.body.as_ref().filter(|_| with_body) {
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
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        Ok(req)
    }

    /// Send, following redirects here rather than in reqwest: reqwest strips
    /// only Authorization/Cookie cross-host, so custom credential headers
    /// (`x-api-key`, `x-csrf-token`, ...) would leak to whatever host a 30x
    /// names (orca#738). Cross-origin hops keep only [`CROSS_ORIGIN_SAFE_HEADERS`];
    /// https→http hops are refused.
    async fn dispatch(
        &self,
        extra_headers: &[(&str, &str)],
        apply_timeout: bool,
    ) -> Result<reqwest::Response, HttpError> {
        let mut url =
            url::Url::parse(&self.url).map_err(|e| HttpError::InvalidUrl(e.to_string()))?;
        if !self.query.is_empty() {
            url.query_pairs_mut().extend_pairs(&self.query);
        }
        let mut headers = self.headers.clone();
        for (k, v) in extra_headers {
            headers.insert((*k).to_string(), (*v).to_string());
        }
        let mut method = self.method.clone();
        let mut with_body = true;
        // One deadline for the whole chain, so redirects can't multiply it.
        let deadline = apply_timeout.then(|| std::time::Instant::now() + self.timeout);
        for _ in 0..=MAX_REDIRECTS {
            let hop_timeout = match deadline {
                Some(d) => {
                    let left = d.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return Err(HttpError::Redirect(format!(
                            "timeout of {:?} exhausted while following redirects",
                            self.timeout
                        )));
                    }
                    Some(left)
                }
                None => None,
            };
            let resp = self
                .build_request(&method, &url, &headers, with_body, hop_timeout)
                .await?
                .send()
                .await?;
            let status = resp.status().as_u16();
            if !matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Ok(resp);
            }
            let Some(next) = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|l| l.to_str().ok())
                .and_then(|l| url.join(l).ok())
            else {
                return Ok(resp);
            };
            check_redirect_scheme(&url, &next)?;
            // 307/308 replay method and body; 303 (and 301/302 after a POST,
            // per browser practice) become a bodyless GET.
            if status == 303 && method != reqwest::Method::HEAD
                || matches!(status, 301 | 302) && method == reqwest::Method::POST
            {
                method = reqwest::Method::GET;
                with_body = false;
            }
            if !with_body {
                headers.retain(|k, _| !BODY_HEADERS.iter().any(|h| k.eq_ignore_ascii_case(h)));
            }
            let cross_origin = !same_origin(&url, &next);
            if cross_origin && with_body && self.body.is_some() {
                return Err(HttpError::Redirect(format!(
                    "refused {status} redirect to {}: would replay the request body cross-origin",
                    next.origin().ascii_serialization()
                )));
            }
            if cross_origin {
                headers.retain(|k, _| {
                    CROSS_ORIGIN_SAFE_HEADERS
                        .iter()
                        .any(|s| k.eq_ignore_ascii_case(s))
                });
            }
            url = next;
        }
        Err(HttpError::Redirect(format!(
            "more than {MAX_REDIRECTS} redirects"
        )))
    }

    /// Stream the response body unbuffered. Unlike [`send`](Self::send) /
    /// [`send_bytes`](Self::send_bytes) this applies **no** `max_body` cap and
    /// **no** total-request timeout — it is the terminal for SSE / chunked
    /// upstreams (model token streams, log tails). The caller checks
    /// [`StreamResponse::status`] and drains [`StreamResponse::bytes_stream`].
    pub async fn send_stream(self) -> Result<StreamResponse, HttpError> {
        let resp = self.dispatch(&[], false).await?;
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
        let resp = self.dispatch(&[], true).await?;
        let status = resp.status().as_u16();
        let headers = flatten_headers(resp.headers());
        let bytes = resp.bytes().await?.to_vec();
        let limit = self.max_body.unwrap_or(MAX_RESPONSE_BYTES);
        if bytes.len() > limit {
            return Err(HttpError::ResponseTooLarge { limit });
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
        let resp = self.dispatch(&[], true).await?;
        let status = resp.status().as_u16();
        let headers = flatten_headers(resp.headers());
        let bytes = resp.bytes().await?.to_vec();
        if bytes.len() > max {
            return Err(HttpError::ResponseTooLarge { limit: max });
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
        let accept: &[(&str, &str)] = if default_accept {
            &[("Accept", "application/json")]
        } else {
            &[]
        };
        let resp = self.dispatch(accept, true).await?;
        let status = resp.status().as_u16();
        let headers = flatten_headers(resp.headers());

        let bytes = resp.bytes().await?;
        let limit = self.max_body.unwrap_or(MAX_RESPONSE_BYTES);
        if bytes.len() > limit {
            return Err(HttpError::ResponseTooLarge { limit });
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

#[derive(Debug, Clone, Default)]
pub struct RawRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout_ms: Option<u64>,
    pub insecure: bool,
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

const BODY_HEADERS: &[&str] = &[
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "content-type",
];

/// Scheme, host and effective port; `https://h` and `https://h:443` match.
fn same_origin(a: &url::Url, b: &url::Url) -> bool {
    a.origin() == b.origin()
}

fn check_redirect_scheme(from: &url::Url, to: &url::Url) -> Result<(), HttpError> {
    if !matches!(to.scheme(), "http" | "https") {
        return Err(HttpError::Redirect(format!(
            "refused redirect to scheme '{}'",
            to.scheme()
        )));
    }
    if from.scheme() == "https" && to.scheme() == "http" {
        return Err(HttpError::Redirect("refused https→http redirect".into()));
    }
    Ok(())
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
    async fn transport_error_does_not_leak_query_secrets() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = Client::new()
            .get(format!("http://127.0.0.1:{port}/api?apikey=SECRET"))
            .query("token", "SECRET")
            .send()
            .await
            .unwrap_err();
        assert!(matches!(err, HttpError::Request(_)));
        assert!(!format!("{err}").contains("SECRET"), "{err}");
        assert!(!format!("{err:?}").contains("SECRET"), "{err:?}");
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
        assert!(matches!(err, HttpError::ResponseTooLarge { .. }));
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
        assert!(matches!(err, HttpError::ResponseTooLarge { .. }));
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

    async fn redirect_to(from: &MockServer, to: String, code: u16) {
        Mock::given(path("/start"))
            .respond_with(ResponseTemplate::new(code).insert_header("Location", to))
            .mount(from)
            .await;
    }

    #[tokio::test]
    async fn cross_origin_redirect_strips_credential_headers() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        redirect_to(&a, format!("{}/land", b.uri()), 302).await;
        Mock::given(path("/land"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&b)
            .await;
        let r = Client::new()
            .get(format!("{}/start", a.uri()))
            .header("x-api-key", "k")
            .header("X-CSRF-Token", "c")
            .header("x-auth-token", "t")
            .header("Proxy-Authorization", "p")
            .bearer("abc")
            .header("User-Agent", "orca-test")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        let reqs = b.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let h = &reqs[0].headers;
        for k in [
            "x-api-key",
            "x-csrf-token",
            "x-auth-token",
            "proxy-authorization",
            "authorization",
        ] {
            assert!(h.get(k).is_none(), "{k} leaked cross-origin");
        }
        assert_eq!(h.get("user-agent").unwrap(), "orca-test");
        assert_eq!(h.get("accept").unwrap(), "application/json");
    }

    #[tokio::test]
    async fn same_origin_redirect_keeps_headers() {
        let a = MockServer::start().await;
        redirect_to(&a, "/land".into(), 307).await;
        Mock::given(method("POST"))
            .and(path("/land"))
            .and(header("x-api-key", "k"))
            .and(wiremock::matchers::body_string("payload"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&a)
            .await;
        let r = Client::new()
            .post(format!("{}/start", a.uri()))
            .header("x-api-key", "k")
            .bytes(b"payload".to_vec(), "text/plain")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn redirect_loop_is_capped() {
        let a = MockServer::start().await;
        redirect_to(&a, "/start".into(), 302).await;
        let err = Client::new()
            .get(format!("{}/start", a.uri()))
            .send()
            .await
            .unwrap_err();
        assert!(matches!(err, HttpError::Redirect(_)), "{err}");
        assert_eq!(
            a.received_requests().await.unwrap().len(),
            MAX_REDIRECTS + 1
        );
    }

    #[test]
    fn https_to_http_redirect_refused() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        assert!(check_redirect_scheme(&u("https://a/x"), &u("http://a/y")).is_err());
        assert!(check_redirect_scheme(&u("http://a/x"), &u("file:///etc/passwd")).is_err());
        assert!(check_redirect_scheme(&u("http://a/x"), &u("https://b/y")).is_ok());
        assert!(check_redirect_scheme(&u("https://a/x"), &u("https://b/y")).is_ok());
    }

    #[test]
    fn default_port_is_same_origin() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        assert!(same_origin(&u("https://h/a"), &u("https://h:443/b")));
        assert!(same_origin(&u("http://h/a"), &u("http://h:80/b")));
        assert!(!same_origin(&u("http://h/a"), &u("http://h:81/b")));
        assert!(!same_origin(&u("http://h/a"), &u("https://h/b")));
        assert!(!same_origin(&u("https://h/a"), &u("https://g/b")));
    }

    async fn assert_rewritten_to_bare_get(code: u16) {
        let a = MockServer::start().await;
        redirect_to(&a, "/land".into(), code).await;
        Mock::given(method("GET"))
            .and(path("/land"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&a)
            .await;
        let r = Client::new()
            .post(format!("{}/start", a.uri()))
            .header("Content-Encoding", "gzip")
            .bytes(b"payload".to_vec(), "text/plain")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        let reqs = a.received_requests().await.unwrap();
        let land = &reqs[1];
        assert!(land.body.is_empty());
        for k in ["content-type", "content-encoding", "transfer-encoding"] {
            assert!(land.headers.get(k).is_none(), "{k} kept on {code} GET");
        }
        assert!(land.headers.get("content-length").is_none_or(|v| v == "0"));
    }

    #[tokio::test]
    async fn see_other_rewrites_to_get_without_body() {
        assert_rewritten_to_bare_get(303).await;
    }

    #[tokio::test]
    async fn post_302_rewrites_to_get_without_body() {
        assert_rewritten_to_bare_get(302).await;
    }

    #[tokio::test]
    async fn relative_cross_port_location_strips_credentials() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        let port = b.address().port();
        redirect_to(&a, format!("//127.0.0.1:{port}/land"), 302).await;
        Mock::given(path("/land"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&b)
            .await;
        Client::new()
            .get(format!("{}/start", a.uri()))
            .header("x-api-key", "k")
            .send()
            .await
            .unwrap();
        let reqs = b.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].headers.get("x-api-key").is_none());
    }

    #[tokio::test]
    async fn timeout_is_total_across_redirect_hops() {
        let a = MockServer::start().await;
        Mock::given(path("/start"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "/start")
                    .set_delay(Duration::from_millis(150)),
            )
            .mount(&a)
            .await;
        let started = std::time::Instant::now();
        let err = Client::new()
            .get(format!("{}/start", a.uri()))
            .timeout(Duration::from_millis(400))
            .send()
            .await
            .unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(400), "{err}");
        let timed_out = match &err {
            HttpError::Request(e) => e.is_timeout(),
            HttpError::Redirect(m) => m.contains("exhausted"),
            _ => false,
        };
        assert!(timed_out, "{err}");
        assert!(a.received_requests().await.unwrap().len() < MAX_REDIRECTS + 1);
    }

    #[tokio::test]
    async fn cross_origin_307_with_body_refused() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        redirect_to(&a, format!("{}/land", b.uri()), 307).await;
        let err = Client::new()
            .post(format!("{}/start", a.uri()))
            .bytes(b"payload".to_vec(), "text/plain")
            .send()
            .await
            .unwrap_err();
        assert!(matches!(err, HttpError::Redirect(_)), "{err}");
        assert!(b.received_requests().await.unwrap().is_empty());
    }
}
