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
                headers: HeaderMap::new(),
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
    }
    impl ClientBuilder {
        pub fn new() -> Self {
            ClientBuilder::default()
        }
        pub fn danger_accept_invalid_certs(mut self, on: bool) -> Self {
            self.insecure = on;
            self
        }
        pub fn build(self) -> Result<Client> {
            Ok(Client {
                insecure: self.insecure,
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

/// Query/form serialization helper — turns progenitor's `QueryParam` slices and
/// form structs into a urlencoded string via serde_urlencoded-free manual join.
pub mod serialize {
    /// Anything the shim can render into a `k=v&…` string. Implemented for the
    /// `QueryParam` slices progenitor passes to `.query(&[...])`.
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
        value: String,
    }
    impl<'a> QueryParam<'a> {
        pub fn new<T: ::serde::Serialize>(name: &'a str, value: &T) -> Self {
            // Scalars render as their plain string; anything else via JSON.
            let value = match ::serde_json::to_value(value) {
                Ok(::serde_json::Value::String(s)) => s,
                Ok(::serde_json::Value::Null) => String::new(),
                Ok(v) => v.to_string(),
                Err(_) => String::new(),
            };
            QueryParam { name, value }
        }
    }
    impl super::serialize::ToQuery for [QueryParam<'_>] {
        fn to_query_string(&self) -> Result<String, String> {
            Ok(self
                .iter()
                .filter(|p| !p.value.is_empty())
                .map(|p| format!("{}={}", urlencode(p.name), urlencode(&p.value)))
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
        crate::urlencoding::encode(s).into_owned()
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
        crate::urlencoding::encode(pc).into_owned()
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
    use super::reqwest::{Client, Request, Response, Result};

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
