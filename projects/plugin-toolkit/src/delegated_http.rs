//! Cap-backed HTTP transport for **delegated** (subprocess) plugins.
//!
//! A progenitor-generated client normally links `reqwest` (+ rustls + hyper) —
//! the single largest source of plugin bloat. But the codegen
//! (`plugin_toolkit_build::openapi`) rewrites every emitted path to
//! `::plugin_toolkit::{reqwest, progenitor_client, api_client}`, so we own what
//! those names resolve to. Under the `delegated-http` feature they resolve to
//! the **minimal, cap-backed contract** in this module instead of the real
//! crates: the generated client keeps compiling unchanged, but every request
//! executes through the `http.request` capability
//! ([`crate::runtime::http_request`]) — orca's one HTTP/TLS stack — so the
//! plugin links none of it.
//!
//! This is *not* a reqwest clone. It implements only the surface our generated
//! code actually uses (Client/RequestBuilder/Request/Response + a `header`
//! module; ResponseValue/Error/ClientHooks/… on the progenitor side). We define
//! the contract; the generated code follows it.

#![allow(clippy::disallowed_types)] // JSON is the transport-dynamic boundary here

/// The `::plugin_toolkit::reqwest` surface a generated client needs.
pub mod reqwest {
    pub use super::header;
    use super::header::{HeaderMap, HeaderValue};

    /// Transport error (opaque — the generated code only formats it).
    #[derive(Debug)]
    pub struct Error(pub String);
    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::error::Error for Error {}

    /// `reqwest::Result` alias used in generated signatures.
    pub type Result<T> = std::result::Result<T, Error>;

    /// HTTP status code. Only the ops the generated code + progenitor shim call.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct StatusCode(pub u16);
    impl StatusCode {
        pub const SWITCHING_PROTOCOLS: StatusCode = StatusCode(101);
        pub fn as_u16(&self) -> u16 {
            self.0
        }
        pub fn is_success(&self) -> bool {
            (200..300).contains(&self.0)
        }
    }

    /// A built request: method + url + headers + body, ready to hand to the
    /// capability. Opaque to the generated code (it only `build()`s and passes
    /// it to `exec`).
    #[derive(Debug, Clone)]
    pub struct Request {
        pub(crate) method: String,
        pub(crate) url: String,
        pub(crate) headers: HeaderMap,
        pub(crate) body: Vec<u8>,
    }

    /// A response the generated code inspects (`status()`, `headers()`) and the
    /// progenitor shim consumes (`bytes()`/`text()`/`json()`). Fully buffered —
    /// the capability returns the whole body.
    #[derive(Debug, Clone)]
    pub struct Response {
        pub(crate) status: StatusCode,
        pub(crate) headers: HeaderMap,
        pub(crate) body: Vec<u8>,
    }
    impl Response {
        pub fn status(&self) -> StatusCode {
            self.status
        }
        pub fn headers(&self) -> &HeaderMap {
            &self.headers
        }
        pub async fn bytes(self) -> Result<Vec<u8>> {
            Ok(self.body)
        }
        pub async fn text(self) -> Result<String> {
            Ok(String::from_utf8_lossy(&self.body).into_owned())
        }
        /// Consume the whole body without erroring (used by the progenitor shim
        /// to buffer before deserialize).
        pub(crate) fn into_parts(self) -> (StatusCode, HeaderMap, Vec<u8>) {
            (self.status, self.headers, self.body)
        }
        /// Same, public for the `api_client` submodule's envelope rewrite.
        pub fn into_parts_pub(self) -> (StatusCode, HeaderMap, Vec<u8>) {
            (self.status, self.headers, self.body)
        }
        /// Rebuild a response from parts (envelope rewrite in `api_client`).
        pub fn from_parts(status: StatusCode, headers: HeaderMap, body: Vec<u8>) -> Self {
            Response {
                status,
                headers,
                body,
            }
        }
    }

    /// The client. Carries per-connection defaults (insecure TLS) that the
    /// capability honors; the actual transport is orca's.
    #[derive(Debug, Clone, Default)]
    pub struct Client {
        pub(crate) insecure: bool,
        pub(crate) default_headers: HeaderMap,
    }
    impl Client {
        pub fn new() -> Self {
            Client::default()
        }
        pub fn get(&self, url: impl Into<String>) -> RequestBuilder {
            self.request("GET", url)
        }
        pub fn post(&self, url: impl Into<String>) -> RequestBuilder {
            self.request("POST", url)
        }
        pub fn put(&self, url: impl Into<String>) -> RequestBuilder {
            self.request("PUT", url)
        }
        pub fn patch(&self, url: impl Into<String>) -> RequestBuilder {
            self.request("PATCH", url)
        }
        pub fn delete(&self, url: impl Into<String>) -> RequestBuilder {
            self.request("DELETE", url)
        }
        fn request(&self, method: &str, url: impl Into<String>) -> RequestBuilder {
            RequestBuilder {
                insecure: self.insecure,
                method: method.to_string(),
                url: url.into(),
                // Seed the per-connection default headers (e.g. an API-token
                // Authorization) so every generated call carries auth, matching
                // reqwest's `default_headers` behavior.
                headers: self.default_headers.clone(),
                body: Vec::new(),
                error: None,
            }
        }
        /// Execute a built request over the `http.request` capability. This is
        /// the single point where a delegated plugin performs I/O — through
        /// orca, never a local socket.
        pub async fn execute(&self, request: Request) -> Result<Response> {
            let req = crate::abi::HttpRequest {
                method: request.method,
                url: request.url,
                headers: request
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.0.clone()))
                    .collect(),
                body: request.body,
                timeout_ms: None,
                insecure: self.insecure,
            };
            let resp = crate::capsink::http_request(&req).map_err(|e| Error(e.to_string()))?;
            let mut headers = HeaderMap::new();
            for (k, v) in resp.headers {
                headers.append_str(&k, &v);
            }
            Ok(Response {
                status: StatusCode(resp.status),
                headers,
                body: resp.body,
            })
        }
    }

    /// `ClientBuilder::new().build()` — used by the generated `Client::new`.
    #[derive(Debug, Default)]
    pub struct ClientBuilder {
        insecure: bool,
        default_headers: HeaderMap,
    }
    impl ClientBuilder {
        pub fn new() -> Self {
            ClientBuilder::default()
        }
        pub fn danger_accept_invalid_certs(mut self, on: bool) -> Self {
            self.insecure = on;
            self
        }
        /// Default headers sent on every request built from this client. The
        /// timeout / cookie-store knobs the real reqwest builder exposes are
        /// no-ops here — the transport is orca's single client, which owns them.
        pub fn default_headers(mut self, headers: HeaderMap) -> Self {
            self.default_headers = headers;
            self
        }
        /// Accepted for parity with the real reqwest builder (progenitor's
        /// generated `Client::new` chains `.connect_timeout(..).timeout(..)`);
        /// no-ops here — orca's single client owns transport timeouts.
        pub fn connect_timeout(self, _dur: ::std::time::Duration) -> Self {
            self
        }
        pub fn timeout(self, _dur: ::std::time::Duration) -> Self {
            self
        }
        pub fn build(self) -> Result<Client> {
            Ok(Client {
                insecure: self.insecure,
                default_headers: self.default_headers,
            })
        }
    }

    /// Fluent request builder — the subset progenitor emits (`header`,
    /// `headers`, `query`, `json`, `body`, `build`). Errors are deferred to
    /// `build()` (matching reqwest's fallible-builder ergonomics).
    pub struct RequestBuilder {
        insecure: bool,
        method: String,
        url: String,
        headers: HeaderMap,
        body: Vec<u8>,
        error: Option<String>,
    }
    impl RequestBuilder {
        pub fn header(mut self, name: impl AsHeaderName, value: HeaderValue) -> Self {
            self.headers.append_str(name.as_header_name(), &value.0);
            self
        }
        pub fn headers(mut self, map: HeaderMap) -> Self {
            for (k, v) in map.iter() {
                self.headers.append_str(k, &v.0);
            }
            self
        }
        pub fn query<T: super::serialize::ToQuery>(mut self, params: &T) -> Self {
            match params.to_query_string() {
                Ok(q) if !q.is_empty() => {
                    let sep = if self.url.contains('?') { '&' } else { '?' };
                    self.url = format!("{}{sep}{q}", self.url);
                }
                Ok(_) => {}
                Err(e) => self.error = Some(e),
            }
            self
        }
        pub fn json<T: ::serde::Serialize>(mut self, body: &T) -> Self {
            match ::serde_json::to_vec(body) {
                Ok(b) => {
                    self.headers.append_str("content-type", "application/json");
                    self.body = b;
                }
                Err(e) => self.error = Some(e.to_string()),
            }
            self
        }
        pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
            self.body = body.into();
            self
        }
        /// Set a form-urlencoded body (progenitor's `RequestBuilderExt::form`).
        pub fn form_urlencoded<T: super::serialize::ToQuery>(mut self, form: &T) -> Self {
            match form.to_query_string() {
                Ok(q) => {
                    self.headers
                        .append_str("content-type", "application/x-www-form-urlencoded");
                    self.body = q.into_bytes();
                }
                Err(e) => self.error = Some(e),
            }
            self
        }
        pub fn build(self) -> Result<Request> {
            if let Some(e) = self.error {
                return Err(Error(e));
            }
            Ok(Request {
                method: self.method,
                url: self.url,
                headers: self.headers,
                body: self.body,
            })
        }
        #[doc(hidden)]
        pub fn insecure(&self) -> bool {
            self.insecure
        }
        /// Build + execute over the `http.request` capability. Mirrors reqwest's
        /// `RequestBuilder::send` so hand-written plugin code (e.g. proxmox's
        /// service-liveness probes) that calls `client.get(url).send()` compiles
        /// and routes through orca unchanged. Reuses [`Client::execute`], carrying
        /// this builder's `insecure` flag.
        pub async fn send(self) -> Result<Response> {
            let insecure = self.insecure;
            let request = self.build()?;
            Client {
                insecure,
                default_headers: HeaderMap::new(),
            }
            .execute(request)
            .await
        }
    }

    /// Header names accepted by [`RequestBuilder::header`] — a `HeaderName` or a
    /// `&str`/`String`, so both generated call shapes compile.
    pub trait AsHeaderName {
        fn as_header_name(&self) -> &str;
    }
    impl AsHeaderName for super::header::HeaderName {
        fn as_header_name(&self) -> &str {
            &self.0
        }
    }
    impl AsHeaderName for &str {
        fn as_header_name(&self) -> &str {
            self
        }
    }
    impl AsHeaderName for String {
        fn as_header_name(&self) -> &str {
            self.as_str()
        }
    }
}

/// A minimal `reqwest::header` surface (`HeaderMap`/`HeaderName`/`HeaderValue`
/// + the well-known names the generated code references).
pub mod header {
    /// Ordered header list — repeats preserved, case-insensitive lookup.
    #[derive(Debug, Clone, Default)]
    pub struct HeaderMap(Vec<(String, HeaderValue)>);
    impl HeaderMap {
        pub fn new() -> Self {
            HeaderMap(Vec::new())
        }
        pub fn with_capacity(n: usize) -> Self {
            HeaderMap(Vec::with_capacity(n))
        }
        pub fn append(&mut self, name: HeaderName, value: HeaderValue) {
            self.0.push((name.0, value));
        }
        pub(crate) fn append_str(&mut self, name: &str, value: &str) {
            self.0
                .push((name.to_ascii_lowercase(), HeaderValue(value.to_string())));
        }
        pub fn get(&self, name: impl AsRef<str>) -> Option<&HeaderValue> {
            let n = name.as_ref().to_ascii_lowercase();
            self.0.iter().find(|(k, _)| *k == n).map(|(_, v)| v)
        }
        pub fn remove(&mut self, name: impl AsRef<str>) {
            let n = name.as_ref().to_ascii_lowercase();
            self.0.retain(|(k, _)| *k != n);
        }
        pub fn iter(&self) -> impl Iterator<Item = (&String, &HeaderValue)> {
            self.0.iter().map(|(k, v)| (k, v))
        }
    }

    /// A header name. Only the constructors the generated code uses.
    #[derive(Debug, Clone)]
    pub struct HeaderName(pub(crate) String);
    impl HeaderName {
        pub fn from_static(s: &'static str) -> Self {
            HeaderName(s.to_ascii_lowercase())
        }
        /// Construct from runtime bytes (used by `ApiClientBuilder::header`).
        /// Rejects non-UTF-8; header-name grammar is enforced upstream.
        pub fn from_bytes(b: &[u8]) -> Result<Self, InvalidHeaderValue> {
            let s = std::str::from_utf8(b).map_err(|_| InvalidHeaderValue)?;
            Ok(HeaderName(s.to_ascii_lowercase()))
        }
        pub fn as_str(&self) -> &str {
            &self.0
        }
    }

    /// A header value. `to_str` is fallible in reqwest; here values are always
    /// valid UTF-8 (we build them from strings), so it never errors.
    #[derive(Debug, Clone)]
    pub struct HeaderValue(pub(crate) String);
    impl HeaderValue {
        pub fn from_static(s: &'static str) -> Self {
            HeaderValue(s.to_string())
        }
        #[allow(clippy::should_implement_trait)] // mirrors reqwest's inherent `from_str`
        pub fn from_str(s: &str) -> Result<Self, InvalidHeaderValue> {
            Ok(HeaderValue(s.to_string()))
        }
        pub fn to_str(&self) -> Result<&str, InvalidHeaderValue> {
            Ok(&self.0)
        }
    }

    /// Placeholder error type for the fallible header constructors.
    #[derive(Debug)]
    pub struct InvalidHeaderValue;
    impl std::fmt::Display for InvalidHeaderValue {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "invalid header value")
        }
    }
    impl std::error::Error for InvalidHeaderValue {}

    /// Well-known header names the generated code references by constant.
    pub const ACCEPT: &str = "accept";
    pub const CONTENT_TYPE: &str = "content-type";
    pub const CONTENT_LENGTH: &str = "content-length";
}

/// Query/form serialization helper — turns progenitor's `QueryParam`s (single,
/// slice, or array) and form structs into a urlencoded string via serde_urlencoded-free manual join.
pub mod serialize {
    /// Anything the shim can render into a `k=v&…` string. Implemented for the
    /// single `QueryParam`s and the slices/arrays progenitor passes to `.query`.
    pub trait ToQuery {
        fn to_query_string(&self) -> Result<String, String>;
    }
}

/// The `::plugin_toolkit::progenitor_client` surface a generated client needs.
pub mod progenitor_client {
    use super::header::HeaderMap;
    use super::reqwest::{Error as ReqError, Request, Response, StatusCode};

    /// Buffered body stream stand-in (generated byte endpoints). Holds the whole
    /// body — the capability already buffered it.
    pub struct ByteStream(pub Vec<u8>);
    impl ByteStream {
        pub fn into_inner(self) -> Vec<u8> {
            self.0
        }
    }

    /// Operation metadata passed to the hooks (id only — we don't use it).
    pub struct OperationInfo {
        pub operation_id: &'static str,
    }

    /// A `k=v` query parameter as progenitor emits it: `QueryParam::new(name,
    /// &value)`. Rendered by [`super::serialize::ToQuery`] on the slice.
    pub struct QueryParam<'a> {
        name: &'a str,
        pairs: Result<Vec<(String, String)>, String>,
    }
    impl<'a> QueryParam<'a> {
        pub fn new<T: ::serde::Serialize>(name: &'a str, value: &T) -> Self {
            let pairs = ::serde_json::to_value(value)
                .map_err(|e| e.to_string())
                .and_then(|v| query_pairs(name, v));
            QueryParam { name, pairs }
        }
    }

    /// Mirrors upstream progenitor-client: sequences repeat the key, `None` is
    /// omitted, and an object flattens to its own `field=value` pairs (the
    /// param name is dropped, as serde_urlencoded does). Anything deeper errors.
    fn query_pairs(name: &str, v: ::serde_json::Value) -> Result<Vec<(String, String)>, String> {
        use ::serde_json::Value;
        match v {
            Value::Null => Ok(Vec::new()),
            Value::Array(items) => items
                .into_iter()
                .filter(|i| !i.is_null())
                .map(|i| scalar(name, i).map(|s| (name.to_string(), s)))
                .collect(),
            Value::Object(fields) => fields
                .into_iter()
                .filter(|(_, f)| !f.is_null())
                .map(|(k, f)| scalar(&k, f).map(|s| (k, s)))
                .collect(),
            v => scalar(name, v).map(|s| vec![(name.to_string(), s)]),
        }
    }

    fn scalar(name: &str, v: ::serde_json::Value) -> Result<String, String> {
        use ::serde_json::Value;
        match v {
            Value::String(s) => Ok(s),
            Value::Bool(b) => Ok(b.to_string()),
            Value::Number(n) => Ok(n.to_string()),
            other => Err(format!("unsupported nested value for `{name}`: {other}")),
        }
    }

    impl super::serialize::ToQuery for QueryParam<'_> {
        fn to_query_string(&self) -> Result<String, String> {
            let pairs = self
                .pairs
                .as_ref()
                .map_err(|e| format!("query param `{}`: {e}", self.name))?;
            Ok(pairs
                .iter()
                .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
                .collect::<Vec<_>>()
                .join("&"))
        }
    }
    impl super::serialize::ToQuery for [QueryParam<'_>] {
        fn to_query_string(&self) -> Result<String, String> {
            let parts = self
                .iter()
                .map(|p| p.to_query_string())
                .collect::<Result<Vec<_>, _>>()?;
            Ok(parts
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("&"))
        }
    }
    impl<const N: usize> super::serialize::ToQuery for [QueryParam<'_>; N] {
        fn to_query_string(&self) -> Result<String, String> {
            self.as_slice().to_query_string()
        }
    }

    fn urlencode(s: &str) -> String {
        crate::url::encode(s)
    }

    /// Client identity hooks progenitor generates an impl of.
    pub trait ClientInfo<Inner> {
        fn api_version() -> &'static str {
            "1"
        }
        fn baseurl(&self) -> &str;
        fn client(&self) -> &super::reqwest::Client;
        fn inner(&self) -> &Inner;
    }

    /// progenitor emits `impl ClientHooks for &Client`, whose supertrait bound is
    /// `&Client: ClientInfo`, but only generates `impl ClientInfo for Client`.
    /// Real `progenitor-client` closes that gap with this blanket forward for
    /// shared references, so mirror it here or the generated `exec` override
    /// fails to resolve `ClientInfo` on `&Client`.
    impl<Inner, T: ClientInfo<Inner>> ClientInfo<Inner> for &T {
        fn api_version() -> &'static str {
            T::api_version()
        }
        fn baseurl(&self) -> &str {
            (**self).baseurl()
        }
        fn client(&self) -> &super::reqwest::Client {
            (**self).client()
        }
        fn inner(&self) -> &Inner {
            (**self).inner()
        }
    }

    /// Execution hooks. The generated `exec` override calls
    /// `api_client::exec_with_unwrapper`; the default here just executes over the
    /// capability. `pre`/`post` default to no-ops.
    #[allow(async_fn_in_trait)]
    pub trait ClientHooks<Inner = ()>: ClientInfo<Inner> {
        async fn pre<E>(&self, _req: &mut Request, _info: &OperationInfo) -> Result<(), Error<E>> {
            Ok(())
        }
        async fn post<E>(
            &self,
            _result: &super::reqwest::Result<Response>,
            _info: &OperationInfo,
        ) -> Result<(), Error<E>> {
            Ok(())
        }
        async fn exec(
            &self,
            request: Request,
            _info: &OperationInfo,
        ) -> super::reqwest::Result<Response> {
            self.client().execute(request).await
        }
    }

    /// A typed, status-and-header-bearing response wrapper — progenitor's
    /// `ResponseValue<T>`. `from_response` buffers + deserializes.
    pub struct ResponseValue<T> {
        inner: T,
        status: StatusCode,
        headers: HeaderMap,
    }
    impl<T: ::serde::de::DeserializeOwned> ResponseValue<T> {
        pub async fn from_response<E>(response: Response) -> Result<Self, Error<E>> {
            let (status, headers, body) = response.into_parts();
            let inner = ::serde_json::from_slice(&body)
                .map_err(|e| Error::InvalidResponsePayload(body, e.to_string()))?;
            Ok(ResponseValue {
                inner,
                status,
                headers,
            })
        }
    }
    impl ResponseValue<()> {
        pub fn empty(response: Response) -> Self {
            let (status, headers, _) = response.into_parts();
            ResponseValue {
                inner: (),
                status,
                headers,
            }
        }
    }
    impl ResponseValue<ByteStream> {
        pub fn stream(response: Response) -> Self {
            let (status, headers, body) = response.into_parts();
            ResponseValue {
                inner: ByteStream(body),
                status,
                headers,
            }
        }
    }
    impl<T> ResponseValue<T> {
        pub fn new(inner: T, status: StatusCode, headers: HeaderMap) -> Self {
            ResponseValue {
                inner,
                status,
                headers,
            }
        }
        pub fn into_inner(self) -> T {
            self.inner
        }
        pub fn status(&self) -> StatusCode {
            self.status
        }
        pub fn headers(&self) -> &HeaderMap {
            &self.headers
        }
    }

    /// The error progenitor threads through generated signatures. Only the
    /// variants our generated code constructs/observes. `Debug`/`Display` are
    /// hand-written so they don't require `E: Debug` (the typed-error payload is
    /// never formatted — only its status matters).
    pub enum Error<E = ()> {
        /// A request could not be executed (capability/transport failure).
        CommunicationError(ReqError),
        /// The body did not match the expected schema.
        InvalidResponsePayload(Vec<u8>, String),
        /// A status with no matching generated arm; carries the raw response.
        UnexpectedResponse(Response),
        /// A typed error response the generated code decoded.
        ErrorResponse(ResponseValue<E>),
    }
    impl<E> Error<E> {
        pub fn status(&self) -> Option<StatusCode> {
            match self {
                Error::UnexpectedResponse(r) => Some(r.status()),
                Error::ErrorResponse(rv) => Some(rv.status()),
                _ => None,
            }
        }
    }
    impl<E> From<ReqError> for Error<E> {
        fn from(e: ReqError) -> Self {
            Error::CommunicationError(e)
        }
    }
    impl<E> std::fmt::Display for Error<E> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Error::CommunicationError(e) => write!(f, "communication error: {e}"),
                Error::InvalidResponsePayload(_, e) => write!(f, "invalid response payload: {e}"),
                Error::UnexpectedResponse(r) => write!(f, "unexpected response: {}", r.status().0),
                Error::ErrorResponse(rv) => write!(f, "error response: {}", rv.status().0),
            }
        }
    }
    impl<E> std::fmt::Debug for Error<E> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{self}")
        }
    }
    impl<E> std::error::Error for Error<E> {}

    /// Percent-encode a path segment (progenitor's `encode_path`).
    pub fn encode_path(pc: &str) -> String {
        crate::url::encode(pc)
    }

    /// Progenitor's form extension on the request builder.
    pub trait RequestBuilderExt {
        fn form_urlencoded<T: ::serde::Serialize>(
            self,
            form: &T,
        ) -> super::reqwest::Result<super::reqwest::RequestBuilder>;
    }
}

/// The `::plugin_toolkit::api_client` surface — the client builder plugins use
/// and the `exec_with_unwrapper` execution chokepoint, both cap-backed.
pub mod api_client {
    use super::header::{HeaderMap, HeaderName, HeaderValue};
    use super::reqwest::{Client, Request, Response, Result};
    use ::anyhow::{Context, Result as AnyResult};

    /// Delegated counterpart of the real `api_client::ApiClientBuilder`. Same
    /// surface, but `build()` produces the cap-backed shim [`Client`] instead of
    /// a real reqwest client — so a plugin builds its typed API client exactly as
    /// before and every request rides orca's `http.request` capability. The
    /// `timeout` / `cookie_store` knobs are accepted for API parity but are
    /// no-ops (orca's single client owns transport policy).
    #[derive(Debug, Default)]
    pub struct ApiClientBuilder {
        headers: HeaderMap,
        insecure: bool,
    }

    impl ApiClientBuilder {
        pub fn new() -> Self {
            Self::default()
        }

        /// Add a default header sent on every request. Returns `Err` on bytes
        /// invalid in an HTTP header — the final wire guard, matching the real
        /// builder.
        pub fn header(mut self, name: &str, value: impl AsRef<str>) -> AnyResult<Self> {
            let name = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("invalid HTTP header name: {name:?}"))?;
            let v = HeaderValue::from_str(value.as_ref())
                .with_context(|| format!("invalid HTTP header value for {}", name.as_str()))?;
            self.headers.append(name, v);
            Ok(self)
        }

        /// `Authorization: Bearer <token>` shorthand.
        pub fn bearer(self, token: impl AsRef<str>) -> AnyResult<Self> {
            self.header("authorization", format!("Bearer {}", token.as_ref()))
        }

        /// Skip TLS verification (self-signed homelab endpoints). Forwarded to
        /// the capability's `insecure` flag so orca's client honors it.
        pub fn insecure(mut self, on: bool) -> Self {
            self.insecure = on;
            self
        }

        /// Accepted for parity with the real builder; no-op (orca owns timeout).
        pub fn timeout(self, _dur: ::std::time::Duration) -> Self {
            self
        }

        /// Accepted for parity; no-op (orca's client owns the cookie jar).
        pub fn cookie_store(self, _on: bool) -> Self {
            self
        }

        /// Materialise the cap-backed [`Client`] with default headers + insecure
        /// pre-attached. No crypto-provider install needed — no local TLS stack.
        pub fn build(self) -> AnyResult<Client> {
            super::reqwest::ClientBuilder::new()
                .default_headers(self.headers)
                .danger_accept_invalid_certs(self.insecure)
                .build()
                .map_err(|e| ::anyhow::anyhow!("build delegated client: {e}"))
        }
    }

    /// Execute a request, optionally peeling a JSON envelope (`{"data": …}`),
    /// over the capability. The delegated counterpart of the real
    /// `exec_with_unwrapper`: same signature, but the transport is orca's.
    pub async fn exec_with_unwrapper<F>(
        client: &Client,
        request: Request,
        unwrap: F,
    ) -> Result<Response>
    where
        F: FnOnce(::serde_json::Value) -> Option<::serde_json::Value>,
    {
        let resp = client.execute(request).await?;
        let (status, mut headers, body) = resp.into_parts_pub();
        let is_json = headers
            .get(super::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.contains("json"));
        if !status.is_success() || !is_json {
            return Ok(Response::from_parts(status, headers, body));
        }
        let unwrapped = ::serde_json::from_slice::<::serde_json::Value>(&body)
            .ok()
            .and_then(unwrap)
            .and_then(|inner| ::serde_json::to_vec(&inner).ok());
        match unwrapped {
            Some(b) => {
                headers.remove(super::header::CONTENT_LENGTH);
                Ok(Response::from_parts(status, headers, b))
            }
            None => Ok(Response::from_parts(status, headers, body)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::progenitor_client::QueryParam;
    use super::serialize::ToQuery;

    fn q<T: ::serde::Serialize>(name: &str, v: &T) -> String {
        QueryParam::new(name, v).to_query_string().unwrap()
    }

    #[test]
    fn array_repeats_key() {
        assert_eq!(
            q("library_id", &vec!["L", "M"]),
            "library_id=L&library_id=M"
        );
    }

    #[test]
    fn reserved_chars_encoded() {
        assert_eq!(q("search", &"a b&c=d/é"), "search=a%20b%26c%3Dd%2F%C3%A9");
    }

    #[test]
    fn empty_array_emits_nothing() {
        assert_eq!(q("library_id", &Vec::<String>::new()), "");
    }

    #[test]
    fn bool_renders_plain() {
        assert_eq!(q("unpaged", &true), "unpaged=true");
        assert_eq!(q("unpaged", &false), "unpaged=false");
    }

    #[test]
    fn option_none_omitted() {
        assert_eq!(q("page", &None::<i32>), "");
        assert_eq!(q("page", &Some(3)), "page=3");
    }

    #[test]
    fn nested_value_rejected() {
        let p = QueryParam::new("x", &vec![vec![1]]);
        assert!(p.to_query_string().is_err());
    }

    #[test]
    fn slice_joins_and_skips_empty() {
        let none: Option<i32> = None;
        let ps = [
            QueryParam::new("a", &vec!["1", "2"]),
            QueryParam::new("b", &none),
            QueryParam::new("c", &true),
        ];
        assert_eq!(ps.to_query_string().unwrap(), "a=1&a=2&c=true");
    }

    #[test]
    fn object_flattens_to_fields() {
        let v = ::serde_json::json!({"a": 1, "b": "x"});
        assert_eq!(q("ignored", &v), "a=1&b=x");
    }

    #[test]
    fn empty_string_kept() {
        assert_eq!(q("s", &""), "s=");
    }

    #[test]
    fn null_in_array_skipped() {
        assert_eq!(q("k", &vec![Some(1), None]), "k=1");
    }

    #[test]
    fn nested_error_single_prefix() {
        let e = QueryParam::new("x", &vec![vec![1]])
            .to_query_string()
            .unwrap_err();
        assert_eq!(e.matches("query param").count(), 1, "{e}");
    }

    #[test]
    fn query_error_fails_build() {
        let rb = super::reqwest::Client::new()
            .get("http://h/p")
            .query(&QueryParam::new("x", &vec![vec![1]]));
        assert!(rb.build().is_err());
    }
}
