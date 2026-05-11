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
//! let client = orca_utils::http::Client::new();
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

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::OnceCell;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
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

/// Default-config HTTP client. Internally pools two `reqwest::Client`s — one
/// that verifies TLS and one that does not — created lazily on first use of
/// each. Cheap to clone (`Arc` inside).
#[derive(Clone, Default)]
pub struct Client {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    secure: OnceCell<reqwest::Client>,
    insecure: OnceCell<reqwest::Client>,
}

impl Client {
    pub fn new() -> Self {
        // reqwest's rustls-no-provider feature needs a process-global ring
        // crypto provider before the first client is built. Idempotent.
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        Self::default()
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
        }
    }

    async fn pool(&self, insecure: bool) -> Result<reqwest::Client, HttpError> {
        let cell = if insecure {
            &self.inner.insecure
        } else {
            &self.inner.secure
        };
        cell.get_or_try_init(|| async move {
            let mut b = reqwest::Client::builder();
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
}

enum Body {
    Json(Value),
    Form(Vec<(String, String)>),
    Bytes(Vec<u8>, &'static str),
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
    pub fn insecure(mut self, on: bool) -> Self {
        self.insecure = on;
        self
    }
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Send and collect the body as raw bytes. Use for binary downloads
    /// (release assets, checksums, archives). Status / headers / size cap
    /// behavior mirror [`send`](Self::send).
    pub async fn send_bytes(self) -> Result<BytesResponse, HttpError> {
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
        match self.body {
            Some(Body::Json(v)) => {
                req = req.header("Content-Type", "application/json").json(&v);
            }
            Some(Body::Form(f)) => {
                req = req.form(&f);
            }
            Some(Body::Bytes(b, ct)) => {
                req = req.header("Content-Type", ct).body(b);
            }
            None => {}
        }
        req = req.timeout(self.timeout);

        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = flatten_headers(resp.headers());
        let bytes = resp.bytes().await?.to_vec();
        if bytes.len() > MAX_RESPONSE_BYTES {
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

    pub async fn send(self) -> Result<Response, HttpError> {
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
        if !self.headers.contains_key("Accept") && !self.headers.contains_key("accept") {
            req = req.header("Accept", "application/json");
        }
        match self.body {
            Some(Body::Json(v)) => {
                req = req.header("Content-Type", "application/json").json(&v);
            }
            Some(Body::Form(f)) => {
                req = req.form(&f);
            }
            Some(Body::Bytes(b, ct)) => {
                req = req.header("Content-Type", ct).body(b);
            }
            None => {}
        }
        req = req.timeout(self.timeout);

        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = flatten_headers(resp.headers());

        let bytes = resp.bytes().await?;
        if bytes.len() > MAX_RESPONSE_BYTES {
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
