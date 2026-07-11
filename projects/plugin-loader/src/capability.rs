//! Daemon-side capability host — the counterpart to a subprocess plugin's
//! capability sink (`plugin_toolkit::serve`).
//!
//! When the supervisor reads a [`Cap`](plugin_proto::Frame::Cap) frame from a
//! plugin, it calls [`handle_cap`] to execute the request against orca's own
//! services and returns the result as a
//! [`CapResult`](plugin_proto::Frame::CapResult). A plugin thus delegates DB /
//! secret access (and, later, HTTP / transport) instead of linking its own —
//! the whole point of the thin-plugin model.
//!
//! These route to the SAME pooled executors the in-process toolkit falls back
//! to (`db::plugin_tables::exec_db_op_pooled` / `db::secrets::exec_secret_op_pooled`),
//! so a tool behaves identically whether its plugin is loaded in-process or run
//! as a subprocess.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, anyhow};
use plugin_toolkit::abi::{
    DbOp, HttpRequest, HttpResponse, HttpStreamChunk, HttpStreamRequest, SecretOp,
};
use plugin_toolkit::serde_json::{self, Value};

/// Capability names the daemon serves. Advertised in the handshake `Welcome`.
/// `http.stream` is the streaming sibling of `http.request`: same request shape,
/// but the response body is relayed chunk-by-chunk instead of buffered.
pub const CAPABILITIES: &[&str] = &["db.op", "secret.op", "http.request", "http.stream"];

/// Whether `cap` is a STREAMING capability — one the supervisor drives through
/// [`handle_cap_stream`] (emitting `CapStreamChunk`/`CapStreamEnd`) rather than
/// the one-shot [`handle_cap`] (`CapResult`).
pub fn is_streaming_cap(cap: &str) -> bool {
    cap == "http.stream"
}

/// A small dedicated runtime for capability I/O (`http.request`). `handle_cap`
/// is synchronous and runs on the supervisor's blocking invoke thread (see
/// `plugin_loader::dispatch`, which drives plugin invokes via `spawn_blocking`),
/// never on a daemon async worker — so blocking on this runtime is safe and
/// keeps capability HTTP off the main scheduler.
fn cap_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build capability I/O runtime")
    })
}

/// The daemon's shared HTTP client for capability requests.
fn http_client() -> &'static utils::http::Client {
    static CLIENT: OnceLock<utils::http::Client> = OnceLock::new();
    CLIENT.get_or_init(utils::http::Client::new)
}

/// Execute one capability request. `args` is the op payload the plugin sent
/// (a serialized [`DbOp`] / [`SecretOp`] / [`HttpRequest`]); the returned `Value`
/// is the reply the supervisor wraps into a `CapResult`.
pub fn handle_cap(cap: &str, args: Value) -> Result<Value> {
    match cap {
        "db.op" => {
            let op: DbOp =
                serde_json::from_value(args).map_err(|e| anyhow!("db.op: bad op payload: {e}"))?;
            let reply = db::plugin_tables::exec_db_op_pooled(&op)?;
            Ok(serde_json::to_value(reply)?)
        }
        "secret.op" => {
            let op: SecretOp = serde_json::from_value(args)
                .map_err(|e| anyhow!("secret.op: bad op payload: {e}"))?;
            let reply = db::secrets::exec_secret_op_pooled(&op)?;
            Ok(serde_json::to_value(reply)?)
        }
        "http.request" => {
            let req: HttpRequest = serde_json::from_value(args)
                .map_err(|e| anyhow!("http.request: bad request payload: {e}"))?;
            let reply = exec_http(req)?;
            Ok(serde_json::to_value(reply)?)
        }
        other => Err(anyhow!("unknown capability '{other}'")),
    }
}

/// Perform an [`HttpRequest`] on the daemon's single HTTP/TLS stack and return
/// the response for any status (a delegating plugin sees 4xx/5xx verbatim).
fn exec_http(req: HttpRequest) -> Result<HttpResponse> {
    let mut builder = http_client()
        .request_str(&req.method, &req.url)
        .map_err(|e| anyhow!("http.request: {e}"))?
        .insecure(req.insecure);
    for (k, v) in &req.headers {
        builder = builder.header(k, v);
    }
    if !req.body.is_empty() {
        // The plugin's own Content-Type header (relayed above) applies; the
        // capability passes the byte body through verbatim.
        builder = builder.raw_body(req.body);
    }
    if let Some(ms) = req.timeout_ms {
        builder = builder.timeout(Duration::from_millis(ms));
    }
    let resp = cap_runtime()
        .block_on(builder.send_raw())
        .map_err(|e| anyhow!("http.request: {e}"))?;
    Ok(HttpResponse {
        status: resp.status,
        headers: resp.headers.into_iter().collect(),
        body: resp.body,
    })
}

/// Execute one STREAMING capability request, invoking `on_chunk` for each chunk
/// as it is produced. `on_chunk(seq, data)` is the supervisor's frame-writer: it
/// emits a `CapStreamChunk{ id, seq, data }`. `seq` starts at 0 (the stream
/// head) and increments per body chunk. Returns `Ok(())` on a clean end (the
/// supervisor then writes `CapStreamEnd{ ok: true }`) or `Err` on a mid-stream
/// failure (the supervisor writes `CapStreamEnd{ ok: false, error }`).
///
/// If `on_chunk` returns `Err` (the plugin aborted / the socket write failed),
/// consumption stops immediately and that error propagates.
pub fn handle_cap_stream(
    cap: &str,
    args: Value,
    on_chunk: &mut dyn FnMut(u64, Value) -> Result<()>,
) -> Result<()> {
    match cap {
        "http.stream" => {
            let req: HttpStreamRequest = serde_json::from_value(args)
                .map_err(|e| anyhow!("http.stream: bad request payload: {e}"))?;
            exec_http_stream(req, on_chunk)
        }
        other => Err(anyhow!("unknown streaming capability '{other}'")),
    }
}

/// Drive an [`HttpStreamRequest`] on the daemon's HTTP stack, relaying the
/// status and headers as `seq == 0` ([`HttpStreamChunk::Head`]) and each body
/// byte-slice as `seq >= 1` ([`HttpStreamChunk::Body`]). Never buffers the
/// whole body.
fn exec_http_stream(
    req: HttpStreamRequest,
    on_chunk: &mut dyn FnMut(u64, Value) -> Result<()>,
) -> Result<()> {
    let mut builder = http_client()
        .request_str(&req.method, &req.url)
        .map_err(|e| anyhow!("http.stream: {e}"))?
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

    cap_runtime().block_on(async move {
        let resp = builder
            .send_stream()
            .await
            .map_err(|e| anyhow!("http.stream: {e}"))?;
        // seq 0: the head (status + headers), before any body byte.
        let head = HttpStreamChunk::Head {
            status: resp.status(),
            headers: resp
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };
        on_chunk(0, serde_json::to_value(head)?)?;

        let mut body = Box::pin(resp.bytes_stream());
        let mut seq = 1u64;
        while let Some(item) = plugin_toolkit::stream::next(&mut body).await {
            let bytes = item.map_err(|e| anyhow!("http.stream: body: {e}"))?;
            let chunk = HttpStreamChunk::Body { bytes };
            on_chunk(seq, serde_json::to_value(chunk)?)?;
            seq += 1;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_toolkit::serde_json::json;

    #[test]
    fn unknown_capability_errors() {
        let err = handle_cap("bogus.cap", json!({})).unwrap_err().to_string();
        assert!(err.contains("unknown capability"), "got: {err}");
    }

    #[test]
    fn http_request_rejects_malformed_payload() {
        // Missing required `url`/`method` fails at deserialization — pure
        // routing/validation, no network.
        let err = handle_cap("http.request", json!({"method": "GET"}))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("http.request: bad request payload"),
            "got: {err}"
        );
    }

    #[test]
    fn malformed_op_payload_errors_before_execution() {
        // A db.op whose payload isn't a valid DbOp fails at deserialization —
        // no db needed, so this is a pure routing/validation check.
        let err = handle_cap("db.op", json!({"not": "a valid op"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("db.op: bad op payload"), "got: {err}");
    }

    #[test]
    fn capabilities_list_is_advertised() {
        assert!(CAPABILITIES.contains(&"db.op"));
        assert!(CAPABILITIES.contains(&"secret.op"));
        assert!(CAPABILITIES.contains(&"http.request"));
    }
}
