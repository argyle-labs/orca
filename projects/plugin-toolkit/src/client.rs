//! Orca-owned HTTP client seam — the plugin-facing surface for host-delegated
//! HTTP.
//!
//! A thin subprocess plugin makes HTTP requests through orca's runtime instead
//! of linking reqwest/rustls/hyper (the single largest source of plugin size).
//! This module gives the plugin author orca's OWN request/response/stream types
//! — [`Request`], [`Response`], [`ByteStream`], [`EventStream`] — and a
//! [`Client`] seam with buffered ([`Client::send`]) and streaming
//! ([`Client::stream`]) verbs. The plugin never names reqwest, `bytes_stream`,
//! or `futures`'s `StreamExt`: those are internal, driven host-side over the
//! `http.request` / `http.stream` capabilities and reached here through
//! [`crate::capsink`]. See [[plugins-stay-thin]].
//!
//! The [`crate::abi`] `HttpRequest`/`HttpStreamChunk` wire types are an internal
//! detail — a plugin builds a [`Request`] and reads a [`Response`], and the
//! conversion to/from the wire shape happens inside this module.

use std::collections::VecDeque;

use anyhow::{Result, anyhow, bail};

use crate::abi::{HttpRequest, HttpResponse, HttpStreamChunk, HttpStreamRequest};
use crate::capsink;

/// An HTTP request a plugin builds against orca's runtime. Orca-owned: the
/// plugin never constructs a reqwest request. Build with [`Request::new`] +
/// the builder methods, then hand to [`Client::send`] / [`Client::stream`].
#[derive(Debug, Clone)]
pub struct Request {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout_ms: Option<u64>,
    insecure: bool,
}

impl Request {
    /// A request with an uppercase `method` (`GET`, `POST`, …) to `url`.
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout_ms: None,
            insecure: false,
        }
    }

    /// Add one header (repeats preserved, in insertion order).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Set the raw request body. The caller's own `Content-Type` header applies.
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// Serialize `value` as JSON into the body and set `Content-Type: application/json`.
    pub fn json<T: serde::Serialize>(mut self, value: &T) -> Result<Self> {
        self.body = serde_json::to_vec(value)?;
        self.headers
            .push(("content-type".into(), "application/json".into()));
        Ok(self)
    }

    /// Per-request timeout in milliseconds. `None` (the default) uses core's.
    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    /// Skip TLS verification (self-signed upstreams).
    pub fn insecure(mut self, on: bool) -> Self {
        self.insecure = on;
        self
    }

    fn to_wire(&self) -> HttpRequest {
        HttpRequest {
            method: self.method.clone(),
            url: self.url.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
            timeout_ms: self.timeout_ms,
            insecure: self.insecure,
        }
    }

    fn to_stream_wire(&self) -> HttpStreamRequest {
        HttpStreamRequest {
            method: self.method.clone(),
            url: self.url.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
            timeout_ms: self.timeout_ms,
            insecure: self.insecure,
        }
    }
}

/// A buffered HTTP response. Carries the response for ANY status — core does not
/// treat 4xx/5xx as an error, so the plugin applies its own status semantics.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// True for a 2xx status.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as UTF-8 (lossy).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Deserialize the body as JSON into `T`.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_slice(&self.body)?)
    }

    fn from_wire(w: HttpResponse) -> Self {
        Self {
            status: w.status,
            headers: w.headers,
            body: w.body,
        }
    }
}

/// An orca-owned stream of response body byte-slices, produced by
/// [`Client::stream`]. Owns its own [`next`](ByteStream::next) — the plugin
/// pulls chunks without naming `futures`'s `StreamExt` or reqwest's
/// `bytes_stream`. The head (status + headers) is available immediately via
/// [`status`](ByteStream::status) / [`headers`](ByteStream::headers); body
/// slices arrive from [`next`](ByteStream::next).
///
/// The stream is drained synchronously host-side and buffered here in arrival
/// order, so `next()` is a synchronous pop — a subprocess plugin has no reactor
/// of its own, matching the serial capability contract.
pub struct ByteStream {
    status: u16,
    headers: Vec<(String, String)>,
    chunks: VecDeque<Vec<u8>>,
}

impl ByteStream {
    /// The response status (available before any body chunk).
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The response headers (available before any body chunk).
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// True for a 2xx status.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The next body byte-slice in wire order, or `None` once the body ends.
    /// The orca-owned equivalent of draining reqwest's `bytes_stream()`.
    // Deliberately not `Iterator::next`: implementing `Iterator` would leak a
    // std/futures streaming abstraction to plugins, which is exactly what this
    // orca-owned seam exists to prevent. The name is intentional.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Vec<u8>> {
        self.chunks.pop_front()
    }
}

/// One Server-Sent Event parsed from an [`EventStream`]: its `event` type (the
/// SSE `event:` field, empty if unset) and `data` payload (concatenated `data:`
/// lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub event: String,
    pub data: String,
}

/// An orca-owned Server-Sent Events stream layered over a [`ByteStream`]. Parses
/// the byte body into discrete [`Event`]s (splitting on the SSE blank-line record
/// separator), so a plugin consumes an SSE upstream (model token streams, live
/// log feeds) without an SSE crate. Owns its own [`next`](EventStream::next).
pub struct EventStream {
    inner: ByteStream,
    buf: String,
}

impl EventStream {
    /// The response status (available before any event).
    pub fn status(&self) -> u16 {
        self.inner.status()
    }

    /// The next parsed SSE event, or `None` once the stream ends. Accumulates
    /// byte chunks until a complete `\n\n`-terminated record is available.
    // Deliberately not `Iterator::next`: see `ByteStream::next` — the orca-owned
    // stream surface must not expose a std/futures trait to plugins.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Event> {
        loop {
            if let Some(evt) = parse_one_event(&mut self.buf) {
                return Some(evt);
            }
            match self.inner.next() {
                Some(bytes) => self.buf.push_str(&String::from_utf8_lossy(&bytes)),
                None => {
                    // Flush a trailing record with no final blank line.
                    if !self.buf.trim().is_empty()
                        && let Some(evt) = parse_record(&std::mem::take(&mut self.buf))
                    {
                        return Some(evt);
                    }
                    return None;
                }
            }
        }
    }
}

/// Split one complete `\n\n`-terminated record off the front of `buf` and parse
/// it, or `None` if no complete record is buffered yet.
fn parse_one_event(buf: &mut String) -> Option<Event> {
    let sep = buf.find("\n\n")?;
    let record: String = buf.drain(..sep + 2).collect();
    parse_record(&record)
}

/// Parse one SSE record (a run of `field: value` lines) into an [`Event`].
/// Returns `None` for a comment-only / empty record.
fn parse_record(record: &str) -> Option<Event> {
    let mut event = String::new();
    let mut data_lines: Vec<&str> = Vec::new();
    for line in record.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => event = value.to_string(),
            "data" => data_lines.push(value),
            _ => {}
        }
    }
    if event.is_empty() && data_lines.is_empty() {
        return None;
    }
    Some(Event {
        event,
        data: data_lines.join("\n"),
    })
}

/// The orca-owned HTTP client seam. Stateless — every request is delegated to
/// orca's runtime over a capability, so a plugin holds no connection pool of its
/// own. Buffered requests go through `http.request`; streaming requests through
/// `http.stream`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Client;

impl Client {
    /// A client seam. Cheap — holds no state.
    pub fn new() -> Self {
        Self
    }

    /// Convenience: a buffered `GET`.
    pub fn get(&self, url: impl Into<String>) -> Result<Response> {
        self.send(Request::new("GET", url))
    }

    /// Convenience: a buffered `POST` with a JSON body.
    pub fn post_json<T: serde::Serialize>(
        &self,
        url: impl Into<String>,
        body: &T,
    ) -> Result<Response> {
        self.send(Request::new("POST", url).json(body)?)
    }

    /// Send a request and buffer the whole response. Routes over the
    /// `http.request` capability; errors if no capability sink is installed
    /// (i.e. not running as an orca subprocess).
    pub fn send(&self, req: Request) -> Result<Response> {
        let wire = capsink::http_request(&req.to_wire())?;
        Ok(Response::from_wire(wire))
    }

    /// Send a request and return an orca-owned [`ByteStream`] over the response
    /// body — the body is NOT buffered host-side. Routes over the `http.stream`
    /// capability. Use for large downloads and long-lived feeds. Errors if no
    /// streaming capability sink is installed.
    pub fn stream(&self, req: Request) -> Result<ByteStream> {
        let op = serde_json::to_string(&req.to_stream_wire())?;
        let mut status: u16 = 0;
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut chunks: VecDeque<Vec<u8>> = VecDeque::new();

        let routed = capsink::cap_route_stream("http.stream", &op, &mut |_seq, chunk_json| {
            let chunk: HttpStreamChunk = serde_json::from_str(&chunk_json)
                .map_err(|e| format!("http.stream: bad chunk: {e}"))?;
            match chunk {
                HttpStreamChunk::Head {
                    status: s,
                    headers: h,
                } => {
                    status = s;
                    headers = h;
                }
                HttpStreamChunk::Body { bytes } => chunks.push_back(bytes),
            }
            Ok(())
        });

        match routed {
            Some(res) => res?,
            None => bail!(
                "http.stream capability unavailable: this plugin is not running as an orca subprocess"
            ),
        }
        if status == 0 {
            return Err(anyhow!("http.stream: stream ended before a head chunk"));
        }
        Ok(ByteStream {
            status,
            headers,
            chunks,
        })
    }

    /// Send a request and return an orca-owned [`EventStream`] that parses the
    /// response body as Server-Sent Events. Layered over [`stream`](Client::stream).
    pub fn events(&self, req: Request) -> Result<EventStream> {
        Ok(EventStream {
            inner: self.stream(req)?,
            buf: String::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_records() {
        let mut buf = "event: token\ndata: hello\n\ndata: world\n\n".to_string();
        let a = parse_one_event(&mut buf).unwrap();
        assert_eq!(
            a,
            Event {
                event: "token".into(),
                data: "hello".into()
            }
        );
        let b = parse_one_event(&mut buf).unwrap();
        assert_eq!(
            b,
            Event {
                event: String::new(),
                data: "world".into()
            }
        );
        assert!(parse_one_event(&mut buf).is_none());
    }

    #[test]
    fn multiline_data_is_joined() {
        let evt = parse_record("data: a\ndata: b\n").unwrap();
        assert_eq!(evt.data, "a\nb");
    }

    #[test]
    fn byte_stream_next_pops_in_order() {
        let mut s = ByteStream {
            status: 200,
            headers: vec![],
            chunks: VecDeque::from(vec![vec![1u8], vec![2u8]]),
        };
        assert_eq!(s.next(), Some(vec![1u8]));
        assert_eq!(s.next(), Some(vec![2u8]));
        assert_eq!(s.next(), None);
    }

    #[test]
    fn event_stream_parses_across_chunk_boundaries() {
        // A record split across two body chunks must still parse.
        let inner = ByteStream {
            status: 200,
            headers: vec![],
            chunks: VecDeque::from(vec![b"data: hel".to_vec(), b"lo\n\n".to_vec()]),
        };
        let mut es = EventStream {
            inner,
            buf: String::new(),
        };
        assert_eq!(es.next().unwrap().data, "hello");
        assert!(es.next().is_none());
    }
}
