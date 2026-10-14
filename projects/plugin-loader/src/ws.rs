//! Host-brokered WebSocket sessions behind the `ws.*` capabilities.
//!
//! The daemon owns the socket and TLS; a plugin holds only a session id. Each
//! session is a task on the capability runtime that relays outbound commands
//! and queues inbound messages for `ws.recv`. Sessions are keyed by the owning
//! plugin's principal, so a plugin can never reach another plugin's session.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use plugin_toolkit::abi::{WsChunk, WsClose, WsOpen, WsPayload, WsRecv, WsSend};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio_tungstenite::{WebSocketStream, client_async};

use crate::capability::run_on_cap_runtime;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Inbound messages a plugin may leave unread before its session is dropped,
/// bounding host memory without ever stalling the socket pump.
const MAX_BACKLOG: usize = 1024;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

enum Cmd {
    Send(Message, oneshot::Sender<Result<()>>),
    Close(u16, oneshot::Sender<()>),
}

type Inbound = Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Result<WsChunk>>>>;

struct Session {
    cmds: mpsc::UnboundedSender<Cmd>,
    inbound: Inbound,
    backlog: Arc<AtomicUsize>,
}

type Key = (String, u64);

fn sessions() -> &'static Mutex<HashMap<Key, Session>> {
    static SESSIONS: OnceLock<Mutex<HashMap<Key, Session>>> = OnceLock::new();
    SESSIONS.get_or_init(Default::default)
}

fn table() -> std::sync::MutexGuard<'static, HashMap<Key, Session>> {
    sessions().lock().unwrap_or_else(|p| p.into_inner())
}

/// Open a WebSocket for `principal` and register its session.
pub(crate) fn open(req: WsOpen, principal: &str) -> Result<WsChunk> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let timeout = req
        .timeout_ms
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
    let url = req.url.clone();
    let ws = run_on_cap_runtime(async move { tokio::time::timeout(timeout, connect(req)).await })?
        .map_err(|_| anyhow!("ws.open: '{url}' timed out after {timeout:?}"))??;

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let backlog = Arc::new(AtomicUsize::new(0));
    crate::capability::cap_runtime().spawn(pump(ws, cmd_rx, in_tx, Arc::clone(&backlog)));

    let session = NEXT.fetch_add(1, Ordering::Relaxed);
    table().insert(
        (principal.to_string(), session),
        Session {
            cmds: cmd_tx,
            inbound: Arc::new(tokio::sync::Mutex::new(in_rx)),
            backlog,
        },
    );
    Ok(WsChunk::Open { session })
}

/// Next inbound message; `None` once the peer closed cleanly. A dropped
/// connection is an error. Either ends the session.
pub(crate) fn recv(req: WsRecv, principal: &str) -> Result<Option<WsChunk>> {
    let (inbound, backlog) = with_session(principal, req.session, "ws.recv", |s| {
        (Arc::clone(&s.inbound), Arc::clone(&s.backlog))
    })?;
    let timeout = req
        .timeout_ms
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
    let got = run_on_cap_runtime(async move {
        tokio::time::timeout(timeout, async { inbound.lock().await.recv().await }).await
    })?;
    match got {
        Err(_) => bail!("ws.recv: no message within {timeout:?}"),
        Ok(Some(Ok(chunk))) => {
            backlog.fetch_sub(1, Ordering::Relaxed);
            Ok(Some(chunk))
        }
        Ok(Some(Err(e))) => {
            remove(principal, req.session);
            Err(e)
        }
        Ok(None) => {
            remove(principal, req.session);
            Ok(None)
        }
    }
}

pub(crate) fn send(req: WsSend, principal: &str) -> Result<()> {
    let cmds = with_session(principal, req.session, "ws.send", |s| s.cmds.clone())?;
    let msg = match req.payload {
        WsPayload::Text(text) => Message::text(text),
        WsPayload::Binary(bytes) => Message::binary(bytes),
    };
    let (ack, done) = oneshot::channel();
    cmds.send(Cmd::Send(msg, ack))
        .map_err(|_| anyhow!("ws.send: session {} is closed", req.session))?;
    run_on_cap_runtime(done)?.map_err(|_| anyhow!("ws.send: session {} is closed", req.session))?
}

pub(crate) fn close(req: WsClose, principal: &str) -> Result<()> {
    with_session(principal, req.session, "ws.close", |_| ())?;
    let Some(session) = remove(principal, req.session) else {
        return Ok(());
    };
    let (ack, done) = oneshot::channel();
    if session
        .cmds
        .send(Cmd::Close(req.code.unwrap_or(1000), ack))
        .is_ok()
    {
        _ = run_on_cap_runtime(done)?;
    }
    Ok(())
}

/// Drop every session `principal` holds; called when its process exits.
pub fn drop_sessions(principal: &str) {
    table().retain(|(owner, _), _| owner != principal);
}

#[cfg(test)]
pub(crate) fn session_count(principal: &str) -> usize {
    table()
        .keys()
        .filter(|(owner, _)| owner == principal)
        .count()
}

fn remove(principal: &str, session: u64) -> Option<Session> {
    table().remove(&(principal.to_string(), session))
}

fn with_session<R>(
    principal: &str,
    session: u64,
    op: &str,
    f: impl FnOnce(&Session) -> R,
) -> Result<R> {
    let table = table();
    if let Some(s) = table.get(&(principal.to_string(), session)) {
        return Ok(f(s));
    }
    if table.keys().any(|(_, id)| *id == session) {
        bail!(
            "{op}: plugin '{principal}' may not use session {session}: it belongs to another plugin"
        );
    }
    bail!("{op}: no open session {session}")
}

async fn connect(req: WsOpen) -> Result<WebSocketStream<Box<dyn Io>>> {
    let mut request = req
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| anyhow!("ws.open: bad URL '{}': {e}", req.url))?;
    for (k, v) in &req.headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| anyhow!("ws.open: bad header name '{k}': {e}"))?;
        let value = HeaderValue::from_str(v)
            .map_err(|e| anyhow!("ws.open: bad header value for '{k}': {e}"))?;
        request.headers_mut().append(name, value);
    }
    let uri = request.uri().clone();
    let tls = match uri.scheme_str() {
        Some("wss") => true,
        Some("ws") => false,
        _ => bail!("ws.open: '{}' is not a ws:// or wss:// URL", req.url),
    };
    let host = uri
        .host()
        .ok_or_else(|| anyhow!("ws.open: '{}' has no host", req.url))?
        .trim_matches(['[', ']'])
        .to_string();
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| anyhow!("ws.open: connect {host}:{port}: {e}"))?;
    let io: Box<dyn Io> = if tls {
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|e| anyhow!("ws.open: bad TLS server name '{host}': {e}"))?;
        let stream = tokio_rustls::TlsConnector::from(tls_config(req.insecure)?)
            .connect(name, tcp)
            .await
            .map_err(|e| anyhow!("ws.open: TLS handshake with {host}: {e}"))?;
        Box::new(stream)
    } else {
        Box::new(tcp)
    };
    let (ws, _) = client_async(request, io)
        .await
        .map_err(|e| anyhow!("ws.open: handshake: {e}"))?;
    Ok(ws)
}

/// The same trust policy as `utils::http`: platform roots, or no verification
/// when the plugin asked for `insecure`.
fn tls_config(insecure: bool) -> Result<Arc<rustls::ClientConfig>> {
    use rustls_platform_verifier::BuilderVerifierExt;
    utils::http::ensure_crypto_provider();
    let builder = rustls::ClientConfig::builder();
    let config = if insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
            .with_no_client_auth()
    } else {
        builder
            .with_platform_verifier()
            .map_err(|e| anyhow!("ws.open: TLS roots: {e}"))?
            .with_no_client_auth()
    };
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Own one socket: run commands, queue inbound data. Pings are answered by
/// tungstenite while the stream is polled; they never reach the plugin.
async fn pump(
    mut ws: WebSocketStream<Box<dyn Io>>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    inbound: mpsc::UnboundedSender<Result<WsChunk>>,
    backlog: Arc<AtomicUsize>,
) {
    use futures::{SinkExt, StreamExt};
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Send(msg, ack)) => {
                    _ = ack.send(ws.send(msg).await.map_err(|e| anyhow!("ws.send: {e}")));
                }
                Some(Cmd::Close(code, ack)) => {
                    _ = ws
                        .close(Some(CloseFrame {
                            code: CloseCode::from(code),
                            reason: "".into(),
                        }))
                        .await;
                    _ = ack.send(());
                    return;
                }
                None => {
                    _ = ws.close(None).await;
                    return;
                }
            },
            frame = ws.next() => {
                let chunk = match frame {
                    Some(Ok(Message::Text(text))) => WsChunk::Text { text: text.to_string() },
                    Some(Ok(Message::Binary(bytes))) => WsChunk::Binary { bytes: Bytes::into(bytes) },
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        _ = inbound.send(Err(anyhow!("ws: connection dropped: {e}")));
                        return;
                    }
                };
                if backlog.fetch_add(1, Ordering::Relaxed) >= MAX_BACKLOG {
                    _ = inbound.send(Err(anyhow!(
                        "ws: plugin left {MAX_BACKLOG} messages unread; session dropped"
                    )));
                    return;
                }
                if inbound.send(Ok(chunk)).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::handle_cap;
    use plugin_toolkit::serde_json::{self, Value, json};

    #[derive(Clone, Copy)]
    enum Peer {
        /// Echo text/binary; report the close code the client sends.
        Echo,
        /// Drop the TCP connection without a close handshake on first message.
        Vanish,
        /// Send one text message, then a clean close.
        GreetThenClose,
    }

    /// One-connection WebSocket server on loopback. The receiver yields the
    /// close code the client sent, if any.
    fn server(peer: Peer) -> (String, std::sync::mpsc::Receiver<Option<u16>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                use futures::{SinkExt, StreamExt};
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                if let Peer::GreetThenClose = peer {
                    ws.send(Message::text("hello")).await.unwrap();
                    ws.close(None).await.ok();
                    return;
                }
                while let Some(Ok(msg)) = ws.next().await {
                    match (peer, msg) {
                        (Peer::Vanish, _) => return,
                        (_, Message::Close(frame)) => {
                            tx.send(frame.map(|f| u16::from(f.code))).ok();
                            return;
                        }
                        (_, msg @ (Message::Text(_) | Message::Binary(_))) => {
                            ws.send(msg).await.unwrap();
                        }
                        _ => {}
                    }
                }
            });
        });
        (format!("ws://{addr}/"), rx)
    }

    fn open(url: &str, principal: &str) -> u64 {
        let reply = handle_cap("ws.open", json!({"url": url}), principal).unwrap();
        match serde_json::from_value(reply).unwrap() {
            WsChunk::Open { session } => session,
            other => panic!("expected Open, got {other:?}"),
        }
    }

    fn recv(session: u64, principal: &str) -> Result<Value> {
        handle_cap(
            "ws.recv",
            json!({"session": session, "timeout_ms": 5000}),
            principal,
        )
    }

    #[test]
    fn open_send_echo_then_close() {
        let (url, closed) = server(Peer::Echo);
        let s = open(&url, "ws-echo");
        handle_cap("ws.send", json!({"session": s, "text": "ping"}), "ws-echo").unwrap();
        assert_eq!(
            recv(s, "ws-echo").unwrap(),
            json!({"t": "text", "text": "ping"})
        );
        handle_cap(
            "ws.send",
            json!({"session": s, "binary": "AP8="}),
            "ws-echo",
        )
        .unwrap();
        assert_eq!(
            recv(s, "ws-echo").unwrap(),
            json!({"t": "binary", "bytes": "AP8="})
        );
        handle_cap("ws.close", json!({"session": s, "code": 4000}), "ws-echo").unwrap();
        assert_eq!(
            closed.recv_timeout(Duration::from_secs(5)).unwrap(),
            Some(4000)
        );
        let err = handle_cap("ws.send", json!({"session": s, "text": "x"}), "ws-echo")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no open session"), "got: {err}");
    }

    #[test]
    fn another_plugins_session_is_refused() {
        let (url, _closed) = server(Peer::Echo);
        let s = open(&url, "ws-owner");
        for (cap, args) in [
            ("ws.send", json!({"session": s, "text": "x"})),
            ("ws.recv", json!({"session": s})),
            ("ws.close", json!({"session": s})),
        ] {
            let err = handle_cap(cap, args, "ws-intruder")
                .unwrap_err()
                .to_string();
            assert!(err.contains("belongs to another plugin"), "{cap}: {err}");
        }
        handle_cap("ws.send", json!({"session": s, "text": "mine"}), "ws-owner").unwrap();
        assert_eq!(
            recv(s, "ws-owner").unwrap(),
            json!({"t": "text", "text": "mine"})
        );
    }

    #[test]
    fn bad_urls_error() {
        for url in ["not a url", "http://127.0.0.1:1/", "ws://127.0.0.1:1/"] {
            let err = handle_cap("ws.open", json!({"url": url}), "ws-bad")
                .unwrap_err()
                .to_string();
            assert!(err.starts_with("ws.open:"), "{url}: {err}");
        }
        assert_eq!(session_count("ws-bad"), 0);
    }

    #[test]
    fn peer_close_ends_the_session_cleanly() {
        let (url, _closed) = server(Peer::GreetThenClose);
        let s = open(&url, "ws-greet");
        assert_eq!(
            recv(s, "ws-greet").unwrap(),
            json!({"t": "text", "text": "hello"})
        );
        assert_eq!(recv(s, "ws-greet").unwrap(), Value::Null);
        assert_eq!(session_count("ws-greet"), 0);
    }

    #[test]
    fn peer_drop_is_an_error() {
        let (url, _closed) = server(Peer::Vanish);
        let s = open(&url, "ws-vanish");
        handle_cap("ws.send", json!({"session": s, "text": "x"}), "ws-vanish").unwrap();
        let err = recv(s, "ws-vanish").unwrap_err().to_string();
        assert!(err.contains("connection dropped"), "got: {err}");
        assert_eq!(session_count("ws-vanish"), 0);
    }

    #[test]
    fn plugin_exit_drops_its_sessions_only() {
        let (url_a, closed_a) = server(Peer::Echo);
        let (url_b, _closed_b) = server(Peer::Echo);
        open(&url_a, "ws-exiting");
        let kept = open(&url_b, "ws-staying");
        drop_sessions("ws-exiting");
        assert_eq!(session_count("ws-exiting"), 0);
        // Dropping the session closes its socket.
        assert!(closed_a.recv_timeout(Duration::from_secs(5)).is_ok());
        handle_cap(
            "ws.send",
            json!({"session": kept, "text": "x"}),
            "ws-staying",
        )
        .unwrap();
        assert!(recv(kept, "ws-staying").is_ok());
    }

    #[test]
    fn recv_times_out_without_ending_the_session() {
        let (url, _closed) = server(Peer::Echo);
        let s = open(&url, "ws-quiet");
        let err = handle_cap(
            "ws.recv",
            json!({"session": s, "timeout_ms": 50}),
            "ws-quiet",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no message within"), "got: {err}");
        assert_eq!(session_count("ws-quiet"), 1);
    }
}
