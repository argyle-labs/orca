// JSON-RPC envelopes are opaque Value at the wire — mirroring the sibling allows.
#![allow(clippy::disallowed_types)]

//! Wire protocol for `pod/subscribe` (slice B).
//!
//! Pure framing/serialization layer. Works over any
//! `AsyncRead + AsyncWrite + Unpin` stream so it's exercisable via
//! `tokio::io::duplex` in unit tests — no TLS pair required. The TLS shim
//! that wraps this for real pod connections is slice C.
//!
//! Wire shape:
//! ```text
//! client → server:  Request      { method = METHOD,        params = SubscribeParams }
//! server → client:  Response     { result = SubscribeOk }   // accepted
//!                OR Response     { error  = ErrorObject }   // rejected; conn ends
//! server → client:  Notification { method = EVENT_METHOD,  params = EventFrame }  // streamed
//! ```
//!
//! Heartbeat + adaptive cadence are slice D.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use utils::framing::{read_frame, write_frame};
use utils::jsonrpc::{ErrorObject, Message, Notification, Request, Response};

use super::subscribe::{HostStatusEvent, subscribe_host_status};
use super::subscribe_demand;

pub const METHOD: &str = "pod/subscribe";
pub const EVENT_METHOD: &str = "pod/subscribe.event";
pub const HEARTBEAT_METHOD: &str = "pod/subscribe.heartbeat";

/// Client-side cadence for heartbeat frames. Sized to land well inside
/// the server's [`subscribe_demand::DEMAND_WINDOW`] so a single dropped
/// heartbeat doesn't flip the publisher into slow mode.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize, Deserialize)]
pub struct SubscribeParams {
    /// Canonical topic string — e.g. `"host:peer.abc123:status"`.
    pub topic: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SubscribeOk {
    pub topic: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EventFrame {
    pub peer_id: String,
    pub snapshot_at_unix: i64,
    pub payload: String,
}

/// Format the canonical topic string for a host's status stream.
pub fn host_status_topic(peer_id: &str) -> String {
    format!("host:{peer_id}:status")
}

/// Parse a host-status topic and return the owner peer_id.
pub fn parse_host_status_topic(topic: &str) -> Result<&str> {
    let rest = topic
        .strip_prefix("host:")
        .with_context(|| format!("topic missing 'host:' prefix: {topic}"))?;
    let peer_id = rest
        .strip_suffix(":status")
        .with_context(|| format!("topic missing ':status' suffix: {topic}"))?;
    anyhow::ensure!(!peer_id.is_empty(), "topic has empty peer_id: {topic}");
    Ok(peer_id)
}

/// Server-side session: read the subscribe request, validate that the
/// requested topic is owned by this daemon, then bidirectionally forward
/// matching bus events as Notifications while processing client
/// heartbeats. Takes the stream by value so [`tokio::io::split`] can be
/// applied for the bidirectional phase.
pub async fn serve_session<S>(mut stream: S, own_peer_id: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let raw = read_frame(&mut stream)
        .await
        .context("read subscribe request")?;
    let msg: Message = serde_json::from_slice(&raw).context("parse subscribe request")?;
    let request = match msg {
        Message::Request(r) => r,
        _ => {
            anyhow::bail!("first frame must be a Request");
        }
    };
    serve_session_with_request(stream, request, own_peer_id).await
}

/// Variant of [`serve_session`] for callers that already parsed the first
/// frame as a `Request` (e.g. the pod listener dispatcher, which peeks the
/// method to decide whether to take the streaming path).
pub async fn serve_session_with_request<S>(
    mut stream: S,
    request: Request,
    own_peer_id: &str,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let id = request.id.clone();

    let topic_peer_id = match validate_subscribe(&request, own_peer_id) {
        Ok(p) => p,
        Err(e) => {
            let resp = Response::err(id, ErrorObject::invalid_params(&e.to_string()));
            let bytes = serde_json::to_vec(&resp).context("serialize error response")?;
            _ = write_frame(&mut stream, &bytes).await;
            return Err(e);
        }
    };

    let ok = SubscribeOk {
        topic: host_status_topic(&topic_peer_id),
    };
    let ok_value = serde_json::to_value(&ok).context("serialize SubscribeOk value")?;
    let resp = Response::ok(id, ok_value);
    let bytes = serde_json::to_vec(&resp).context("serialize SubscribeOk frame")?;
    write_frame(&mut stream, &bytes)
        .await
        .context("write SubscribeOk")?;

    let (mut read_half, mut write_half) = tokio::io::split(stream);
    let mut rx = subscribe_host_status();
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    if ev.peer_id != topic_peer_id {
                        // Bus is global; filter out other producers.
                        continue;
                    }
                    let frame = EventFrame {
                        peer_id: ev.peer_id,
                        snapshot_at_unix: ev.snapshot_at_unix,
                        payload: ev.payload,
                    };
                    let params_value =
                        serde_json::to_value(&frame).context("serialize EventFrame")?;
                    let notif = Notification::new(EVENT_METHOD, Some(params_value));
                    let nbytes = serde_json::to_vec(&notif).context("serialize event notif")?;
                    if write_frame(&mut write_half, &nbytes).await.is_err() {
                        return Ok(());
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!("pod/subscribe session lagged by {n} events; continuing");
                }
                Err(RecvError::Closed) => return Ok(()),
            },
            frame = read_frame(&mut read_half) => match frame {
                Ok(bytes) => {
                    if is_heartbeat_frame(&bytes) {
                        subscribe_demand::touch();
                    }
                    // Silently ignore non-heartbeat client frames.
                }
                Err(_) => return Ok(()), // client closed
            },
        }
    }
}

/// True iff the bytes parse as a `Notification` whose method is
/// [`HEARTBEAT_METHOD`]. Bad/unrelated frames silently return false so
/// the server treats unknown traffic as "ignore, but don't disconnect".
pub fn is_heartbeat_frame(bytes: &[u8]) -> bool {
    matches!(
        serde_json::from_slice::<Message>(bytes),
        Ok(Message::Notification(n)) if n.method == HEARTBEAT_METHOD
    )
}

/// Validate a subscribe Request and return the topic's peer_id.
/// Pure helper, no I/O — easy to test branch-by-branch.
fn validate_subscribe(request: &Request, own_peer_id: &str) -> Result<String> {
    anyhow::ensure!(
        request.method == METHOD,
        "unexpected method '{}' (expected '{METHOD}')",
        request.method
    );
    let params: SubscribeParams = match &request.params {
        Some(v) => serde_json::from_value(v.clone()).context("parse SubscribeParams")?,
        None => anyhow::bail!("subscribe requires params"),
    };
    let peer_id = parse_host_status_topic(&params.topic)?.to_string();
    // Compare on the bare machine key: a subscriber whose pod_peers row still
    // carries a legacy `peer.<id>` CN would otherwise be rejected even though
    // it is asking for OUR topic. Identity is the machine key, never the
    // prefixed form (see feedback_no_id_prefixes / locality-is-a-flag).
    anyhow::ensure!(
        crate::machine_key(&peer_id) == crate::machine_key(own_peer_id),
        "topic peer_id '{peer_id}' is not owned by this daemon ('{own_peer_id}')"
    );
    // Return the bare key so the caller keys host_status rows uniformly.
    Ok(crate::machine_key(&peer_id).to_string())
}

/// Client-side: send the subscribe request, await the ack, then forward
/// streamed events into `tx` while a background task sends heartbeats at
/// [`HEARTBEAT_INTERVAL`]. Exits cleanly when the consumer drops `tx` or
/// the server closes the stream; the heartbeat task is aborted on exit.
pub async fn run_client<S>(
    mut stream: S,
    topic_peer_id: &str,
    tx: mpsc::Sender<HostStatusEvent>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let params = SubscribeParams {
        topic: host_status_topic(topic_peer_id),
    };
    let params_value = serde_json::to_value(&params).context("serialize SubscribeParams")?;
    let req = Request::new(1, METHOD, Some(params_value));
    let bytes = serde_json::to_vec(&req).context("serialize subscribe request")?;
    write_frame(&mut stream, &bytes)
        .await
        .context("write subscribe request")?;

    let raw = read_frame(&mut stream)
        .await
        .context("read subscribe ack")?;
    let msg: Message = serde_json::from_slice(&raw).context("parse subscribe ack")?;
    let response = match msg {
        Message::Response(r) => r,
        _ => anyhow::bail!("expected Response ack, got another frame type"),
    };
    if let Some(err) = response.error {
        anyhow::bail!("subscribe rejected: {}", err.message);
    }

    let (mut read_half, write_half) = tokio::io::split(stream);
    let heartbeat_task = tokio::spawn(send_heartbeats(write_half, HEARTBEAT_INTERVAL));

    let result = run_event_loop(&mut read_half, tx).await;
    heartbeat_task.abort();
    result
}

/// Background task: send a heartbeat notification every `interval` until
/// the write half errors (server closed) or the task is aborted.
async fn send_heartbeats<W>(mut write_half: W, interval: Duration)
where
    W: AsyncWrite + Unpin,
{
    let notif = Notification::new(HEARTBEAT_METHOD, None);
    let bytes = match serde_json::to_vec(&notif) {
        Ok(b) => b,
        Err(_) => return,
    };
    loop {
        tokio::time::sleep(interval).await;
        if write_frame(&mut write_half, &bytes).await.is_err() {
            return;
        }
    }
}

async fn run_event_loop<R>(read_half: &mut R, tx: mpsc::Sender<HostStatusEvent>) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    loop {
        let raw = match read_frame(read_half).await {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        let msg: Message = serde_json::from_slice(&raw).context("parse event frame")?;
        let notif = match msg {
            Message::Notification(n) => n,
            _ => continue,
        };
        if notif.method != EVENT_METHOD {
            continue;
        }
        let params = notif.params.unwrap_or(Value::Null);
        let frame: EventFrame = serde_json::from_value(params).context("parse EventFrame")?;
        let ev = HostStatusEvent {
            peer_id: frame.peer_id,
            snapshot_at_unix: frame.snapshot_at_unix,
            payload: frame.payload,
        };
        if tx.send(ev).await.is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
#[allow(unused_mut)]
mod tests {
    // `unused_mut` is allowed because several duplex bindings need `mut` only
    // inside a `tokio::spawn`-moved closure; clippy can't see across the move.
    use super::*;
    use crate::subscribe::publish_host_status;
    use std::time::Duration;

    fn req(method: &str, params: Option<Value>) -> Request {
        Request::new(1, method, params)
    }

    fn subscribe_params_value(topic: &str) -> Value {
        serde_json::to_value(SubscribeParams {
            topic: topic.into(),
        })
        .unwrap()
    }

    #[test]
    fn host_status_topic_round_trips() {
        let t = host_status_topic("abc");
        assert_eq!(t, "host:abc:status");
        assert_eq!(parse_host_status_topic(&t).unwrap(), "abc");
    }

    #[test]
    fn parse_topic_rejects_missing_prefix() {
        let e = parse_host_status_topic("abc:status").unwrap_err();
        assert!(e.to_string().contains("'host:' prefix"));
    }

    #[test]
    fn parse_topic_rejects_missing_suffix() {
        let e = parse_host_status_topic("host:abc:metrics").unwrap_err();
        assert!(e.to_string().contains("':status' suffix"));
    }

    #[test]
    fn parse_topic_rejects_empty_peer_id() {
        let e = parse_host_status_topic("host::status").unwrap_err();
        assert!(e.to_string().contains("empty peer_id"));
    }

    #[test]
    fn validate_rejects_wrong_method() {
        let r = req(
            "pod/ping",
            Some(subscribe_params_value("host:peer.x:status")),
        );
        let e = validate_subscribe(&r, "x").unwrap_err();
        assert!(e.to_string().contains("unexpected method"));
    }

    #[test]
    fn validate_rejects_missing_params() {
        let r = req(METHOD, None);
        let e = validate_subscribe(&r, "x").unwrap_err();
        assert!(e.to_string().contains("requires params"));
    }

    #[test]
    fn validate_rejects_unparseable_params() {
        let r = req(METHOD, Some(Value::String("not an object".into())));
        let e = validate_subscribe(&r, "x").unwrap_err();
        assert!(e.to_string().contains("SubscribeParams"));
    }

    #[test]
    fn validate_rejects_bad_topic() {
        let r = req(METHOD, Some(subscribe_params_value("not-a-topic")));
        let e = validate_subscribe(&r, "x").unwrap_err();
        assert!(e.to_string().contains("'host:' prefix"));
    }

    #[test]
    fn validate_rejects_foreign_peer_id() {
        let r = req(METHOD, Some(subscribe_params_value("host:other:status")));
        let e = validate_subscribe(&r, "self").unwrap_err();
        assert!(e.to_string().contains("not owned by this daemon"));
    }

    #[test]
    fn is_heartbeat_frame_recognizes_heartbeat_notification() {
        let bytes = serde_json::to_vec(&Notification::new(HEARTBEAT_METHOD, None)).unwrap();
        assert!(is_heartbeat_frame(&bytes));
    }

    #[test]
    fn is_heartbeat_frame_rejects_other_notifications() {
        let bytes = serde_json::to_vec(&Notification::new("pod/something.else", None)).unwrap();
        assert!(!is_heartbeat_frame(&bytes));
    }

    #[test]
    fn is_heartbeat_frame_rejects_non_notifications() {
        let bytes = serde_json::to_vec(&Request::new(1, HEARTBEAT_METHOD, None)).unwrap();
        assert!(!is_heartbeat_frame(&bytes));
    }

    #[test]
    fn is_heartbeat_frame_rejects_garbage() {
        assert!(!is_heartbeat_frame(b"not json at all"));
    }

    #[test]
    fn validate_accepts_matching_topic() {
        let r = req(METHOD, Some(subscribe_params_value("host:self:status")));
        let p = validate_subscribe(&r, "self").unwrap();
        assert_eq!(p, "self");
    }

    /// Drive serve_session + run_client over an in-memory duplex pipe.
    /// Covers: handshake, event forwarding, foreign-peer filter (`continue`
    /// branch), and the run_client → tx happy path.
    #[tokio::test]
    async fn end_to_end_subscribe_streams_matching_events() {
        let own = "e2e-happy";
        let (mut client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel::<HostStatusEvent>(8);

        let own_for_server = own.to_string();
        let server = tokio::spawn(async move {
            _ = serve_session(server_io, &own_for_server).await;
        });
        let own_for_client = own.to_string();
        let client = tokio::spawn(async move {
            _ = run_client(client_io, &own_for_client, tx).await;
        });

        // Let the handshake (subscribe → ack) complete before publishing.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Publish a foreign-peer event first to exercise the filter branch.
        publish_host_status(HostStatusEvent {
            peer_id: "e2e-foreign".into(),
            snapshot_at_unix: 1,
            payload: "ignored".into(),
        });
        // Then a matching event the client should receive.
        publish_host_status(HostStatusEvent {
            peer_id: own.into(),
            snapshot_at_unix: 99,
            payload: "good".into(),
        });

        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .expect("recv ok");
        assert_eq!(got.peer_id, own);
        assert_eq!(got.snapshot_at_unix, 99);
        assert_eq!(got.payload, "good");

        // Dropping the receiver makes run_client exit; dropping that side of
        // the duplex makes serve_session's next write fail and exit.
        drop(rx);
        _ = tokio::time::timeout(Duration::from_secs(2), client).await;
        _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    }

    /// End-to-end: client's auto-heartbeat reaches the server. Drive the
    /// loop with a short heartbeat interval so we don't have to wait 5s.
    #[tokio::test]
    async fn server_observes_client_heartbeats() {
        let own = "e2e-heartbeat";
        let (mut client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let (tx, _rx) = mpsc::channel::<HostStatusEvent>(8);

        let own_for_server = own.to_string();
        let server = tokio::spawn(async move {
            _ = serve_session(server_io, &own_for_server).await;
        });
        // Manually drive the subscribe handshake on the client side, then
        // inject a heartbeat frame so the test doesn't have to wait for
        // the production 5s interval.
        let driver = tokio::spawn(async move {
            let mut s = client_io;
            // Subscribe.
            let req = Request::new(
                1,
                METHOD,
                Some(subscribe_params_value("host:e2e-heartbeat:status")),
            );
            write_frame(&mut s, &serde_json::to_vec(&req).unwrap())
                .await
                .unwrap();
            // Read ack.
            let _ = read_frame(&mut s).await.unwrap();
            // Send a single heartbeat.
            let hb = Notification::new(HEARTBEAT_METHOD, None);
            write_frame(&mut s, &serde_json::to_vec(&hb).unwrap())
                .await
                .unwrap();
            // Keep the connection open briefly so the server has time to
            // observe and touch the demand counter.
            tokio::time::sleep(Duration::from_millis(50)).await;
            s
        });

        let before = crate::subscribe_demand::heartbeats_seen();
        let _client_io = driver.await.unwrap();
        // Give the server task a tick to process the heartbeat.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let after = crate::subscribe_demand::heartbeats_seen();
        assert!(
            after > before,
            "expected heartbeats_seen to advance; before={before} after={after}"
        );

        drop(tx);
        _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    }

    /// Server rejects a bad subscribe → client surfaces the rejection.
    /// Covers serve_session's error path AND run_client's `response.error` path.
    #[tokio::test]
    async fn end_to_end_subscribe_rejected_when_topic_invalid() {
        let (mut client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let (tx, _rx) = mpsc::channel::<HostStatusEvent>(8);

        let server = tokio::spawn(async move {
            // own_peer_id mismatches the client's request → rejection.
            serve_session(server_io, "server-owns-this").await
        });
        let client_err = run_client(client_io, "something-else", tx)
            .await
            .expect_err("client should see rejection");
        assert!(
            client_err.to_string().contains("subscribe rejected"),
            "got: {client_err}"
        );

        let server_res = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server task hung");
        assert!(server_res.unwrap().is_err(), "server should report error");
    }

    /// First frame from client is not a Request → serve_session bails.
    #[tokio::test]
    async fn serve_session_rejects_non_request_first_frame() {
        let (mut client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let notif = Notification::new("pod/ping", None);
        let bytes = serde_json::to_vec(&notif).unwrap();
        write_frame(&mut client_io, &bytes).await.unwrap();
        let err = serve_session(server_io, "any").await.unwrap_err();
        assert!(err.to_string().contains("first frame must be a Request"));
    }

    /// Server sends a Notification with the correct method but a payload that
    /// doesn't deserialize as `EventFrame` → run_client surfaces the error.
    /// Covers the `parse EventFrame` failure branch.
    #[tokio::test]
    async fn run_client_errors_on_malformed_event_payload() {
        let (mut client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let (tx, _rx) = mpsc::channel::<HostStatusEvent>(8);

        let server = tokio::spawn(async move {
            let raw = read_frame(&mut server_io).await.unwrap();
            let msg: Message = serde_json::from_slice(&raw).unwrap();
            let req = match msg {
                Message::Request(r) => r,
                _ => panic!("expected request"),
            };
            let ok = SubscribeOk {
                topic: host_status_topic("bad-payload"),
            };
            let resp = Response::ok(req.id, serde_json::to_value(&ok).unwrap());
            write_frame(&mut server_io, &serde_json::to_vec(&resp).unwrap())
                .await
                .unwrap();
            // Correct method, wrong shape (number where EventFrame expected).
            let notif = Notification::new(EVENT_METHOD, Some(Value::Number(7.into())));
            write_frame(&mut server_io, &serde_json::to_vec(&notif).unwrap())
                .await
                .unwrap();
        });

        let err = run_client(client_io, "bad-payload", tx)
            .await
            .expect_err("expected EventFrame parse error");
        assert!(err.to_string().contains("EventFrame"), "got: {err}");
        _ = server.await;
    }

    /// Notification with no params → `unwrap_or(Value::Null)` branch, followed
    /// by EventFrame parse error.
    #[tokio::test]
    async fn run_client_handles_event_notification_with_no_params() {
        let (mut client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let (tx, _rx) = mpsc::channel::<HostStatusEvent>(8);

        let server = tokio::spawn(async move {
            let raw = read_frame(&mut server_io).await.unwrap();
            let msg: Message = serde_json::from_slice(&raw).unwrap();
            let req = match msg {
                Message::Request(r) => r,
                _ => panic!("expected request"),
            };
            let ok = SubscribeOk {
                topic: host_status_topic("no-params"),
            };
            let resp = Response::ok(req.id, serde_json::to_value(&ok).unwrap());
            write_frame(&mut server_io, &serde_json::to_vec(&resp).unwrap())
                .await
                .unwrap();
            let notif = Notification::new(EVENT_METHOD, None);
            write_frame(&mut server_io, &serde_json::to_vec(&notif).unwrap())
                .await
                .unwrap();
        });

        let err = run_client(client_io, "no-params", tx)
            .await
            .expect_err("expected EventFrame parse error from Null");
        assert!(err.to_string().contains("EventFrame"), "got: {err}");
        _ = server.await;
    }

    /// Garbage bytes mid-stream → run_client surfaces the parse error.
    #[tokio::test]
    async fn run_client_errors_on_invalid_event_frame_bytes() {
        let (mut client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let (tx, _rx) = mpsc::channel::<HostStatusEvent>(8);

        let server = tokio::spawn(async move {
            let raw = read_frame(&mut server_io).await.unwrap();
            let msg: Message = serde_json::from_slice(&raw).unwrap();
            let req = match msg {
                Message::Request(r) => r,
                _ => panic!("expected request"),
            };
            let ok = SubscribeOk {
                topic: host_status_topic("garbage"),
            };
            let resp = Response::ok(req.id, serde_json::to_value(&ok).unwrap());
            write_frame(&mut server_io, &serde_json::to_vec(&resp).unwrap())
                .await
                .unwrap();
            // Not valid JSON.
            write_frame(&mut server_io, b"not json").await.unwrap();
        });

        let err = run_client(client_io, "garbage", tx)
            .await
            .expect_err("expected parse error");
        assert!(err.to_string().contains("parse event frame"), "got: {err}");
        _ = server.await;
    }

    /// Client receives a non-Response first frame → run_client bails.
    #[tokio::test]
    async fn run_client_rejects_non_response_ack() {
        let (mut client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let (tx, _rx) = mpsc::channel::<HostStatusEvent>(8);

        // Client side will send subscribe request first; we read & discard,
        // then send a Notification back (wrong type for ack).
        let server = tokio::spawn(async move {
            let _ = read_frame(&mut server_io).await.unwrap();
            let notif = Notification::new("pod/ping", None);
            let bytes = serde_json::to_vec(&notif).unwrap();
            write_frame(&mut server_io, &bytes).await.unwrap();
        });

        let err = run_client(client_io, "x", tx).await.unwrap_err();
        assert!(err.to_string().contains("expected Response ack"));
        _ = server.await;
    }

    /// run_client must ignore Notifications whose method isn't EVENT_METHOD,
    /// and ignore non-Notification frames mid-stream, then process the next
    /// valid event.
    #[tokio::test]
    async fn run_client_skips_unrelated_frames_then_processes_event() {
        let (mut client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel::<HostStatusEvent>(8);

        let server = tokio::spawn(async move {
            // Read subscribe request.
            let raw = read_frame(&mut server_io).await.unwrap();
            let msg: Message = serde_json::from_slice(&raw).unwrap();
            let req = match msg {
                Message::Request(r) => r,
                _ => panic!("expected request"),
            };
            // Send ack.
            let ok = SubscribeOk {
                topic: host_status_topic("skip-test"),
            };
            let resp = Response::ok(req.id, serde_json::to_value(&ok).unwrap());
            write_frame(&mut server_io, &serde_json::to_vec(&resp).unwrap())
                .await
                .unwrap();
            // Send a non-Notification frame (a Request) → client should skip.
            let stray = Request::new(2, "pod/ping", None);
            write_frame(&mut server_io, &serde_json::to_vec(&stray).unwrap())
                .await
                .unwrap();
            // Send a Notification with the wrong method → client should skip.
            let wrong = Notification::new("pod/other.event", None);
            write_frame(&mut server_io, &serde_json::to_vec(&wrong).unwrap())
                .await
                .unwrap();
            // Send a real event → client should forward.
            let frame = EventFrame {
                peer_id: "skip-test".into(),
                snapshot_at_unix: 7,
                payload: "yes".into(),
            };
            let notif =
                Notification::new(EVENT_METHOD, Some(serde_json::to_value(&frame).unwrap()));
            write_frame(&mut server_io, &serde_json::to_vec(&notif).unwrap())
                .await
                .unwrap();
            // Close the stream so client exits via read_frame Err path.
            drop(server_io);
        });

        let client = tokio::spawn(async move {
            _ = run_client(client_io, "skip-test", tx).await;
        });

        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .expect("recv ok");
        assert_eq!(got.snapshot_at_unix, 7);
        assert_eq!(got.payload, "yes");

        _ = tokio::time::timeout(Duration::from_secs(2), server).await;
        _ = tokio::time::timeout(Duration::from_secs(2), client).await;
    }
}
