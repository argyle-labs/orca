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
//! to (`db::plugin_tables::exec_db_op_pooled` / `secrets::exec_secret_op_pooled`),
//! so a tool behaves identically whether its plugin is loaded in-process or run
//! as a subprocess.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Result, anyhow};
use plugin_toolkit::abi::{
    AgentRegistration, DbOp, HttpRequest, HttpResponse, HttpStreamChunk, HttpStreamRequest,
    SecretOp, WsClose, WsOpen, WsRecv, WsSend,
};
use plugin_toolkit::serde_json::{self, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::Instrument;

/// Capability names the daemon serves. Advertised in the handshake `Welcome`.
/// `http.stream` is the streaming sibling of `http.request`: same request shape,
/// but the response body is relayed chunk-by-chunk instead of buffered.
/// `ws.*` broker a WebSocket session (see [`crate::ws`]).
pub const CAPABILITIES: &[&str] = &[
    "db.op",
    "secret.op",
    "http.request",
    "http.stream",
    "ws.open",
    "ws.recv",
    "ws.send",
    "ws.close",
    "agents.register",
    API_CALL,
    plugin_toolkit::abi::HTTP_BODY_B64,
];

pub const API_CALL: &str = "api.call";

/// Whether `cap` is a STREAMING capability — one the supervisor drives through
/// [`handle_cap_stream`] (emitting `CapStreamChunk`/`CapStreamEnd`) rather than
/// the one-shot [`handle_cap`] (`CapResult`).
pub fn is_streaming_cap(cap: &str) -> bool {
    cap == "http.stream"
}

/// A small dedicated runtime for capability I/O (`http.request`), keeping
/// capability HTTP off the main scheduler. `handle_cap` is synchronous and runs
/// on the supervisor's blocking invoke thread; work reaches this runtime only
/// through [`run_on_cap_runtime`] / `spawn`, never `block_on`.
pub(crate) fn cap_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build capability I/O runtime")
    })
}

/// Run `fut` on [`cap_runtime`] and wait for it on a std channel. Unlike
/// `block_on`, this is sound from inside another runtime's context: a stray
/// call from an async worker blocks that worker instead of panicking (#798).
pub(crate) fn run_on_cap_runtime<F>(fut: F) -> Result<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    cap_runtime().spawn(
        async move {
            tx.send(fut.await).ok();
        }
        .instrument(tracing::Span::current()),
    );
    rx.recv()
        .map_err(|_| anyhow!("capability I/O task panicked"))
}

/// An agents provider name belongs to `principal` when it is the plugin id or its
/// owner-qualified form (`argyle-labs/agents` for plugin `agents`). Provider
/// names key the registry, so this stops one plugin replacing another's roster.
pub(crate) fn agent_provider_owned(name: &str, principal: &str) -> bool {
    name == principal
        || name
            .rsplit_once('/')
            .is_some_and(|(owner, id)| !owner.is_empty() && id == principal)
}

pub fn handle_api_call(
    args: Value,
    principal: &str,
    inv: Option<&spec::surfaces::Invocation>,
) -> Result<Value> {
    let inv = inv
        .cloned()
        .ok_or_else(|| anyhow!("api.call requires an authenticated caller"))?;
    let call: plugin_proto::ApiCall =
        serde_json::from_value(args).map_err(|e| anyhow!("api.call: bad payload: {e}"))?;
    let principal = principal.to_string();
    let outcome = run_on_cap_runtime(async move {
        spec::surfaces::call(
            &principal,
            call.surface.as_deref(),
            &call.op,
            &call.endpoint,
            call.args,
            &inv,
        )
        .await
    })??;
    api_call_reply(outcome)
}

#[derive(Debug)]
pub struct PlannedCall(pub plugin_proto::ApiCallPlan);

impl std::fmt::Display for PlannedCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "api.call planned, not executed: {} {}",
            self.0.method, self.0.url
        )
    }
}

impl std::error::Error for PlannedCall {}

fn api_call_reply(outcome: spec::surfaces::Outcome) -> Result<Value> {
    match outcome {
        spec::surfaces::Outcome::Response(value) => Ok(value),
        spec::surfaces::Outcome::Plan(plan) => {
            Err(PlannedCall(serde_json::from_value(serde_json::to_value(plan)?)?).into())
        }
    }
}

const MAX_BODY_CEILING: u64 = 64 * 1024 * 1024;

fn clamp_max_body(requested: Option<u64>) -> Option<usize> {
    requested.map(|n| n.min(MAX_BODY_CEILING) as usize)
}

const BODY_BUDGET_BYTES: u32 = 256 * 1024 * 1024;
const BODY_BUDGET_WAIT: Duration = Duration::from_secs(30);
const PLUGIN_BODY_SHARE_BYTES: u32 = MAX_BODY_CEILING as u32;

fn body_budget() -> Arc<Semaphore> {
    static BUDGET: OnceLock<Arc<Semaphore>> = OnceLock::new();
    BUDGET
        .get_or_init(|| Arc::new(Semaphore::new(BODY_BUDGET_BYTES as usize)))
        .clone()
}

fn plugin_body_share(principal: &str) -> Arc<Semaphore> {
    static SHARES: OnceLock<std::sync::Mutex<std::collections::HashMap<String, Arc<Semaphore>>>> =
        OnceLock::new();
    SHARES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(principal.to_string())
        .or_insert_with(|| Arc::new(Semaphore::new(PLUGIN_BODY_SHARE_BYTES as usize)))
        .clone()
}

async fn reserve_body_budget(
    budget: Arc<Semaphore>,
    bytes: usize,
    wait: Duration,
) -> Result<OwnedSemaphorePermit> {
    let total = u32::try_from(budget.available_permits()).unwrap_or(u32::MAX);
    let bytes = bytes.min(BODY_BUDGET_BYTES as usize) as u32;
    match tokio::time::timeout(wait, budget.acquire_many_owned(bytes)).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(anyhow!("http.request: in-flight body budget closed")),
        Err(_) => Err(anyhow!(
            "http.request: in-flight body budget exhausted; waited {}s for {bytes} bytes ({total} were free)",
            wait.as_secs()
        )),
    }
}

async fn reserve_plugin_body_budget(
    share: Arc<Semaphore>,
    global: Arc<Semaphore>,
    bytes: usize,
    wait: Duration,
) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
    let deadline = tokio::time::Instant::now() + wait;
    let bytes = bytes.min(PLUGIN_BODY_SHARE_BYTES as usize);
    let own = reserve_body_budget(share, bytes, wait).await?;
    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
    let all = reserve_body_budget(global, bytes, left).await?;
    Ok((own, all))
}

/// Execute one capability request on behalf of `principal` — the authoritative
/// plugin id bound to this session's socket (see
/// [`supervisor::PluginProcess`](crate::supervisor::PluginProcess)). `args` is
/// the op payload the plugin sent (a serialized [`DbOp`] / [`SecretOp`] /
/// [`HttpRequest`]); the returned `Value` is the reply the supervisor wraps into
/// a `CapResult`. `db.op` / `secret.op` are confined to `principal`'s namespace.
pub fn handle_cap(cap: &str, args: Value, principal: &str) -> Result<Value> {
    match cap {
        "db.op" => {
            let op: DbOp =
                serde_json::from_value(args).map_err(|e| anyhow!("db.op: bad op payload: {e}"))?;
            let reply = db::pool::with_pooled_or_open(|conn| {
                db::plugin_db_policy::exec_db_op_policed(conn, &op, principal)
            })?;
            Ok(serde_json::to_value(reply)?)
        }
        "secret.op" => {
            let op: SecretOp = serde_json::from_value(args)
                .map_err(|e| anyhow!("secret.op: bad op payload: {e}"))?;
            let reply = secrets::exec_secret_op_pooled(
                &secrets::Principal::Plugin(principal.to_string()),
                &op,
            )?;
            Ok(serde_json::to_value(reply)?)
        }
        "http.request" => {
            let req: HttpRequest = serde_json::from_value(args)
                .map_err(|e| anyhow!("http.request: bad request payload: {e}"))?;
            let max_body = clamp_max_body(req.max_body_bytes);
            let body_b64 = req.body_b64;
            let reserve = max_body.unwrap_or(utils::http::MAX_RESPONSE_BYTES);
            let share = plugin_body_share(principal);
            let mut builder = utils::http::shared()
                .raw_request(utils::http::RawRequest {
                    method: req.method,
                    url: req.url,
                    headers: req.headers,
                    body: req.body,
                    timeout_ms: req.timeout_ms,
                    insecure: req.insecure,
                })
                .map_err(|e| anyhow!("http.request: {e}"))?;
            if let Some(n) = max_body {
                builder = builder.max_body(n);
            }
            let resp = run_on_cap_runtime(async move {
                let _permits =
                    reserve_plugin_body_budget(share, body_budget(), reserve, BODY_BUDGET_WAIT)
                        .await?;
                builder
                    .send_raw()
                    .await
                    .map_err(|e| anyhow!("http.request: {e}"))
            })??;
            let reply = HttpResponse {
                status: resp.status,
                headers: resp.headers.into_iter().collect(),
                body: resp.body,
            };
            Ok(if body_b64 {
                serde_json::to_value(reply)?
            } else {
                serde_json::to_value(reply.legacy())?
            })
        }
        "ws.open" => {
            let req: WsOpen = serde_json::from_value(args)
                .map_err(|e| anyhow!("ws.open: bad request payload: {e}"))?;
            Ok(serde_json::to_value(crate::ws::open(req, principal)?)?)
        }
        "ws.recv" => {
            let req: WsRecv = serde_json::from_value(args)
                .map_err(|e| anyhow!("ws.recv: bad request payload: {e}"))?;
            Ok(serde_json::to_value(crate::ws::recv(req, principal)?)?)
        }
        "ws.send" => {
            let req: WsSend = serde_json::from_value(args)
                .map_err(|e| anyhow!("ws.send: bad request payload: {e}"))?;
            crate::ws::send(req, principal)?;
            Ok(Value::Null)
        }
        "ws.close" => {
            let req: WsClose = serde_json::from_value(args)
                .map_err(|e| anyhow!("ws.close: bad request payload: {e}"))?;
            crate::ws::close(req, principal)?;
            Ok(Value::Null)
        }
        "agents.register" => {
            let reg: AgentRegistration = serde_json::from_value(args)
                .map_err(|e| anyhow!("agents.register: bad payload: {e}"))?;
            if !agent_provider_owned(&reg.name, principal) {
                return Err(anyhow!(
                    "agents.register: plugin '{principal}' may not register provider '{}'; the provider name must be '{principal}' or '<owner>/{principal}'",
                    reg.name
                ));
            }
            agents::register_from_json(
                reg.name,
                &reg.agents_json,
                &reg.hooks_json,
                &reg.skills_json,
                &reg.commands_json,
                &reg.prompt_fragments_json,
            );
            Ok(Value::Null)
        }
        other => Err(anyhow!("unknown capability '{other}'")),
    }
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
    let builder = utils::http::shared()
        .raw_request(utils::http::RawRequest {
            method: req.method,
            url: req.url,
            headers: req.headers,
            body: req.body,
            timeout_ms: req.timeout_ms,
            insecure: req.insecure,
        })
        .map_err(|e| anyhow!("http.stream: {e}"))?;

    // `on_chunk` is borrowed and not `Send`, so the stream is driven on the cap
    // runtime and each chunk is relayed back to run `on_chunk` on this thread.
    // The bounded channel gives backpressure; a consumer that stalls past the
    // plugin idle timeout, or drops the receiver, ends the task.
    let send_timeout = dispatch::plugin_idle_timeout();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Value>>(16);
    cap_runtime().spawn(
        async move {
            let send = |item| send_within(&tx, item, send_timeout);
            let resp = match builder.send_stream().await {
                Ok(resp) => resp,
                Err(e) => {
                    send(Err(anyhow!("http.stream: {e}"))).await;
                    return;
                }
            };
            // seq 0: the head (status + headers), before any body byte.
            let head = HttpStreamChunk::Head {
                status: resp.status(),
                headers: resp
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            };
            if !send(serde_json::to_value(head).map_err(Into::into)).await {
                return;
            }
            let mut body = Box::pin(resp.bytes_stream());
            while let Some(item) = plugin_toolkit::stream::next(&mut body).await {
                let chunk = item
                    .map_err(|e| anyhow!("http.stream: body: {e}"))
                    .and_then(|bytes| {
                        serde_json::to_value(HttpStreamChunk::Body { bytes }).map_err(Into::into)
                    });
                let failed = chunk.is_err();
                if !send(chunk).await || failed {
                    return;
                }
            }
        }
        .instrument(tracing::Span::current()),
    );
    // `futures` block_on needs no reactor, so this is sound on any thread.
    let mut seq = 0u64;
    while let Some(chunk) = futures::executor::block_on(rx.recv()) {
        on_chunk(seq, chunk?)?;
        seq += 1;
    }
    Ok(())
}

/// Send `item`, giving up after `timeout`. `false` means the consumer is gone
/// or stalled and the producer should stop.
async fn send_within(
    tx: &tokio::sync::mpsc::Sender<Result<Value>>,
    item: Result<Value>,
    timeout: Duration,
) -> bool {
    match tokio::time::timeout(timeout, tx.send(item)).await {
        Ok(sent) => sent.is_ok(),
        Err(_) => {
            tracing::warn!("http.stream: consumer stalled past {timeout:?}; ending stream");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_toolkit::serde_json::json;

    #[test]
    fn api_call_plan_is_an_error_never_a_value() {
        let plan = spec::surfaces::Plan {
            method: "POST".into(),
            url: "https://h/x".into(),
            query: vec![],
            body: Value::Null,
            endpoint: "main".into(),
            route: "https://h".into(),
        };
        let err = api_call_reply(spec::surfaces::Outcome::Plan(plan)).unwrap_err();
        let planned = err.downcast_ref::<PlannedCall>().expect("typed plan");
        assert_eq!(planned.0.method, "POST");
        assert_eq!(planned.0.endpoint, "main");
        assert_eq!(
            api_call_reply(spec::surfaces::Outcome::Response(json!({"a": 1}))).unwrap(),
            json!({"a": 1})
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cap_io_is_sound_from_inside_a_runtime() {
        assert_eq!(run_on_cap_runtime(async { 7 }).unwrap(), 7);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_send_gives_up_on_a_stalled_consumer() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        assert!(send_within(&tx, Ok(json!(1)), Duration::from_millis(50)).await);
        assert!(!send_within(&tx, Ok(json!(2)), Duration::from_millis(50)).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_send_stops_once_the_consumer_is_gone() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        assert!(!send_within(&tx, Ok(json!(1)), Duration::from_secs(5)).await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_stream_relays_from_inside_a_runtime() {
        let url = oneshot_http(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello");
        let mut body = Vec::new();
        let mut seqs = Vec::new();
        let mut sink = |seq: u64, v: Value| -> Result<()> {
            seqs.push(seq);
            if let HttpStreamChunk::Body { bytes } = serde_json::from_value(v)? {
                body.extend_from_slice(&bytes);
            }
            Ok(())
        };
        handle_cap_stream(
            "http.stream",
            json!({"method": "GET", "url": url}),
            &mut sink,
        )
        .expect("stream succeeds");
        assert_eq!(body, b"hello");
        assert_eq!(seqs.first().copied(), Some(0));
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "seqs: {seqs:?}");
    }

    #[test]
    fn unknown_capability_errors() {
        let err = handle_cap("bogus.cap", json!({}), "p")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown capability"), "got: {err}");
    }

    #[test]
    fn agents_register_refuses_foreign_provider_name() {
        let err = handle_cap(
            "agents.register",
            json!({"name": "victim-xyz", "hooks_json": "[]"}),
            "attacker-xyz",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("may not register provider 'victim-xyz'"),
            "got: {err}"
        );
        assert!(
            agents::registry::providers()
                .iter()
                .all(|p| p.name() != "victim-xyz")
        );
    }

    #[test]
    fn agent_provider_owned_accepts_id_and_owner_qualified_id() {
        assert!(agent_provider_owned("agents", "agents"));
        assert!(agent_provider_owned("argyle-labs/agents", "agents"));
        assert!(!agent_provider_owned("argyle-labs/agents", "evil"));
        assert!(!agent_provider_owned("/agents", "agents"));
        assert!(!agent_provider_owned("agents-x", "agents"));
    }

    #[test]
    fn agents_register_accepts_own_provider_name() {
        handle_cap(
            "agents.register",
            json!({"name": "self-xyz", "agents_json": r#"[{"name":"self-agent-xyz","body":"b","origin":"spoofed"}]"#}),
            "self-xyz",
        )
        .unwrap();
        let agent = agents::compose_agents()
            .into_iter()
            .find(|a| a.name == "self-agent-xyz")
            .unwrap();
        assert_eq!(agent.origin, "self-xyz");
        agents::deregister_provider("self-xyz");
    }

    #[test]
    fn http_request_rejects_malformed_payload() {
        // Missing required `url`/`method` fails at deserialization — pure
        // routing/validation, no network.
        let err = handle_cap("http.request", json!({"method": "GET"}), "p")
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
        let err = handle_cap("db.op", json!({"not": "a valid op"}), "p")
            .unwrap_err()
            .to_string();
        assert!(err.contains("db.op: bad op payload"), "got: {err}");
    }

    #[test]
    fn secret_op_refuses_foreign_name() {
        let dir = tempfile::tempdir().unwrap();
        let err = db::with_thread_db_path(&dir.path().join("secret-op.db"), || {
            handle_cap(
                "secret.op",
                serde_json::to_value(SecretOp::Get {
                    name: "jellyfin.api_key".into(),
                })
                .unwrap(),
                "proxmox",
            )
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("outside its namespace"), "got: {err}");
    }

    #[test]
    fn capabilities_list_is_advertised() {
        assert!(CAPABILITIES.contains(&"db.op"));
        assert!(CAPABILITIES.contains(&"secret.op"));
        assert!(CAPABILITIES.contains(&"http.request"));
        assert!(CAPABILITIES.contains(&"ws.open"));
        assert!(!is_streaming_cap("ws.open"));
    }

    #[test]
    fn only_http_stream_is_streaming() {
        assert!(is_streaming_cap("http.stream"));
        assert!(!is_streaming_cap("http.request"));
        assert!(!is_streaming_cap("db.op"));
        assert!(!is_streaming_cap("secret.op"));
        assert!(!is_streaming_cap("agents.register"));
        assert!(!is_streaming_cap("bogus"));
    }

    #[test]
    fn secret_op_rejects_malformed_payload() {
        // A secret.op whose payload isn't a valid SecretOp fails at
        // deserialization before any secret store is touched — pure routing.
        let err = handle_cap("secret.op", json!({"not": "a valid op"}), "p")
            .unwrap_err()
            .to_string();
        assert!(err.contains("secret.op: bad op payload"), "got: {err}");
    }

    #[test]
    fn agents_register_rejects_malformed_payload() {
        // A wrongly-typed field fails at deserialization before anything is
        // registered — pure routing/validation.
        let err = handle_cap("agents.register", json!({"name": 123}), "p")
            .unwrap_err()
            .to_string();
        assert!(err.contains("agents.register: bad payload"), "got: {err}");
    }

    #[test]
    fn stream_unknown_capability_errors() {
        let mut sink = |_seq: u64, _v: Value| Ok(());
        let err = handle_cap_stream("bogus.cap", json!({}), &mut sink)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown streaming capability"), "got: {err}");
    }

    /// Spawn a one-shot HTTP/1.1 server on an ephemeral loopback port that
    /// replies to the first connection with `response` (raw bytes, including
    /// status line + headers + body) and then closes. Returns the bound
    /// `http://127.0.0.1:PORT/` base URL. The server thread self-terminates
    /// after serving one request.
    fn oneshot_http(response: &'static [u8]) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                // Drain the request line/headers so the client's write completes.
                let mut buf = [0u8; 1024];
                if sock.read(&mut buf).is_ok()
                    && sock.write_all(response).is_ok()
                    && sock.flush().is_ok()
                {
                    // served
                }
            }
        });
        format!("http://{addr}/")
    }

    #[test]
    fn http_request_relays_status_headers_and_body() {
        let url =
            oneshot_http(b"HTTP/1.1 201 Created\r\nContent-Length: 5\r\nX-Test: yes\r\n\r\nhello");
        let reply = handle_cap("http.request", json!({"method": "GET", "url": url}), "p")
            .expect("request succeeds");
        let resp: HttpResponse = serde_json::from_value(reply).expect("decode HttpResponse");
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body, b"hello");
        assert!(
            resp.headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("x-test") && v == "yes"),
            "missing relayed header: {:?}",
            resp.headers
        );
    }

    #[test]
    fn http_request_replies_legacy_array_to_old_plugin() {
        let url = oneshot_http(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi");
        let reply = handle_cap("http.request", json!({"method": "GET", "url": url}), "p").unwrap();
        assert_eq!(reply["body"], json!([104, 105]));
    }

    #[test]
    fn http_request_replies_base64_when_plugin_advertises() {
        let url = oneshot_http(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi");
        let reply = handle_cap(
            "http.request",
            json!({"method": "GET", "url": url, "body_b64": true}),
            "p",
        )
        .unwrap();
        assert_eq!(reply["body"], json!("aGk="));
        let resp: HttpResponse = serde_json::from_value(reply).unwrap();
        assert_eq!(resp.body, b"hi");
    }

    #[test]
    fn http_request_accepts_base64_and_legacy_request_body() {
        for body in [json!("aGk="), json!([104, 105])] {
            let req: HttpRequest =
                serde_json::from_value(json!({"method": "POST", "url": "http://x/", "body": body}))
                    .unwrap();
            assert_eq!(req.body, b"hi");
        }
    }

    #[test]
    fn capabilities_advertise_base64_body() {
        assert!(CAPABILITIES.contains(&plugin_toolkit::abi::HTTP_BODY_B64));
    }

    #[test]
    fn http_request_relays_error_status_verbatim() {
        // A 4xx is returned as a normal response, not an Err — a delegating
        // plugin sees the status verbatim.
        let url = oneshot_http(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        let reply = handle_cap("http.request", json!({"method": "GET", "url": url}), "p")
            .expect("request succeeds even for 4xx");
        let resp: HttpResponse = serde_json::from_value(reply).expect("decode HttpResponse");
        assert_eq!(resp.status, 404);
    }

    fn oversized_response(len: usize) -> &'static [u8] {
        let mut out = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n\r\n").into_bytes();
        out.resize(out.len() + len, b'x');
        Box::leak(out.into_boxed_slice())
    }

    #[test]
    fn http_request_over_default_cap_fails() {
        let url = oneshot_http(oversized_response(9 * 1024 * 1024));
        let err = handle_cap("http.request", json!({"method": "GET", "url": url}), "p")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&format!("exceeded {} bytes", 8 * 1024 * 1024)),
            "got: {err}"
        );
    }

    #[test]
    fn http_request_raised_cap_succeeds() {
        let len = 9 * 1024 * 1024;
        let url = oneshot_http(oversized_response(len));
        let reply = handle_cap(
            "http.request",
            json!({"method": "GET", "url": url, "max_body_bytes": 16 * 1024 * 1024}),
            "p",
        )
        .expect("request succeeds under raised cap");
        let resp: HttpResponse = serde_json::from_value(reply).expect("decode HttpResponse");
        assert_eq!(resp.body.len(), len);
    }

    #[test]
    fn http_request_max_body_clamped_to_ceiling() {
        assert_eq!(clamp_max_body(None), None);
        assert_eq!(clamp_max_body(Some(16)), Some(16));
        assert_eq!(
            clamp_max_body(Some(MAX_BODY_CEILING * 4)),
            Some(MAX_BODY_CEILING as usize)
        );
    }

    #[test]
    fn http_request_max_body_above_ceiling_is_clamped_end_to_end() {
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_CEILING + 1
        );
        let url = oneshot_http(Box::leak(header.into_bytes().into_boxed_slice()));
        let err = handle_cap(
            "http.request",
            json!({"method": "GET", "url": url, "max_body_bytes": u64::MAX}),
            "p",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(&format!("exceeded {MAX_BODY_CEILING} bytes")),
            "got: {err}"
        );
    }

    #[test]
    fn body_budget_blocks_until_released_then_times_out() {
        let budget = Arc::new(Semaphore::new(100));
        run_on_cap_runtime(async move {
            let held = reserve_body_budget(budget.clone(), 80, Duration::from_millis(50))
                .await
                .unwrap();
            let err = reserve_body_budget(budget.clone(), 40, Duration::from_millis(50))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("in-flight body budget exhausted"),
                "got: {err}"
            );
            let waiter = tokio::spawn(reserve_body_budget(
                budget.clone(),
                40,
                Duration::from_secs(5),
            ));
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(held);
            let _reserved = waiter.await.unwrap().unwrap();
        })
        .unwrap();
    }

    #[test]
    fn body_budget_allows_concurrent_reservations_within_total() {
        let budget = Arc::new(Semaphore::new(100));
        run_on_cap_runtime(async move {
            let a = reserve_body_budget(budget.clone(), 50, Duration::from_millis(50));
            let b = reserve_body_budget(budget.clone(), 50, Duration::from_millis(50));
            let (a, b) = tokio::join!(a, b);
            let (_a, _b) = (a.unwrap(), b.unwrap());
            assert_eq!(budget.available_permits(), 0);
        })
        .unwrap();
    }

    #[test]
    fn plugin_share_caps_one_plugin_without_starving_others() {
        let global = Arc::new(Semaphore::new(100));
        let hog = Arc::new(Semaphore::new(40));
        let other = Arc::new(Semaphore::new(40));
        run_on_cap_runtime(async move {
            let wait = Duration::from_millis(50);
            let _a = reserve_plugin_body_budget(hog.clone(), global.clone(), 40, wait)
                .await
                .unwrap();
            let err = reserve_plugin_body_budget(hog.clone(), global.clone(), 10, wait)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("exhausted"), "got: {err}");
            let _b = reserve_plugin_body_budget(other.clone(), global.clone(), 40, wait)
                .await
                .unwrap();
            assert_eq!(global.available_permits(), 20);
        })
        .unwrap();
    }

    #[test]
    fn plugin_shares_are_per_principal() {
        assert!(Arc::ptr_eq(
            &plugin_body_share("a"),
            &plugin_body_share("a")
        ));
        assert!(!Arc::ptr_eq(
            &plugin_body_share("a"),
            &plugin_body_share("b")
        ));
    }

    #[test]
    fn http_request_releases_budget_on_error() {
        for _ in 0..3 {
            handle_cap(
                "http.request",
                json!({"method": "GET", "url": "http://127.0.0.1:1/", "max_body_bytes": MAX_BODY_CEILING, "timeout_ms": 2000}),
                "p",
            )
            .unwrap_err();
        }
        let url = oneshot_http(oversized_response(9 * 1024 * 1024));
        handle_cap("http.request", json!({"method": "GET", "url": url}), "p").unwrap_err();
        let full = BODY_BUDGET_BYTES as usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while body_budget().available_permits() < full && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(body_budget().available_permits(), full);
        assert_eq!(
            plugin_body_share("p").available_permits(),
            PLUGIN_BODY_SHARE_BYTES as usize
        );
    }

    #[test]
    fn http_request_payload_without_max_body_deserializes() {
        let req: HttpRequest =
            serde_json::from_value(json!({"method": "GET", "url": "http://x/"})).expect("decode");
        assert_eq!(req.max_body_bytes, None);
    }

    #[test]
    fn http_request_bad_url_errors() {
        // A syntactically invalid URL fails when the client builds the request,
        // surfacing an "http.request:" prefixed error — no network reached.
        let err = handle_cap(
            "http.request",
            json!({"method": "GET", "url": "not a valid url"}),
            "p",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("http.request:"), "got: {err}");
    }

    #[test]
    fn http_request_connection_refused_errors() {
        // Port 1 on loopback refuses; the send fails and the error is surfaced.
        let err = handle_cap(
            "http.request",
            json!({"method": "GET", "url": "http://127.0.0.1:1/", "timeout_ms": 2000}),
            "p",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("http.request:"), "got: {err}");
    }

    #[test]
    fn http_stream_relays_head_then_body_chunks() {
        let url = oneshot_http(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-S: 1\r\n\r\nworld");
        let mut seqs: Vec<u64> = Vec::new();
        let mut status: Option<u16> = None;
        let mut body: Vec<u8> = Vec::new();
        {
            let mut sink = |seq: u64, v: Value| -> Result<()> {
                seqs.push(seq);
                let chunk: HttpStreamChunk = serde_json::from_value(v)?;
                match chunk {
                    HttpStreamChunk::Head { status: s, .. } => status = Some(s),
                    HttpStreamChunk::Body { bytes } => body.extend_from_slice(&bytes),
                }
                Ok(())
            };
            handle_cap_stream(
                "http.stream",
                json!({"method": "GET", "url": url}),
                &mut sink,
            )
            .expect("stream succeeds");
        }
        // seq 0 is always the head; body chunks follow with seq >= 1.
        assert_eq!(seqs.first().copied(), Some(0));
        assert_eq!(status, Some(200));
        assert_eq!(body, b"world");
        assert!(seqs.iter().skip(1).all(|&s| s >= 1), "seqs: {seqs:?}");
    }

    #[test]
    fn http_stream_propagates_sink_error() {
        // If the on_chunk sink returns Err (e.g. the plugin aborted), consumption
        // stops and that error propagates out of handle_cap_stream.
        let url = oneshot_http(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc");
        let mut sink = |_seq: u64, _v: Value| -> Result<()> { Err(anyhow!("sink boom")) };
        let err = handle_cap_stream(
            "http.stream",
            json!({"method": "GET", "url": url}),
            &mut sink,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("sink boom"), "got: {err}");
    }

    #[test]
    fn http_stream_bad_url_errors() {
        let mut sink = |_seq: u64, _v: Value| -> Result<()> { Ok(()) };
        let err = handle_cap_stream(
            "http.stream",
            json!({"method": "GET", "url": "not a valid url"}),
            &mut sink,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("http.stream:"), "got: {err}");
    }

    #[test]
    fn stream_rejects_malformed_payload() {
        // http.stream with a payload missing method/url fails at deserialization
        // before any network I/O — the on_chunk sink is never invoked.
        let mut called = false;
        let mut sink = |_seq: u64, _v: Value| {
            called = true;
            Ok(())
        };
        let err = handle_cap_stream("http.stream", json!({"method": "GET"}), &mut sink)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("http.stream: bad request payload"),
            "got: {err}"
        );
        assert!(!called, "sink must not fire on a payload error");
    }
}
