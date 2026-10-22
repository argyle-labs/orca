//! Host-brokered WebSocket sessions behind the `ws.*` capabilities.
//!
//! The daemon owns the socket and TLS; a plugin holds only a session id. Each
//! session is a task on the capability runtime that relays outbound commands
//! and queues inbound messages for `ws.recv`. Sessions are keyed by the owning
//! plugin's principal, so a plugin can never reach another plugin's session.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use plugin_toolkit::abi::{WsChunk, WsClose, WsOpen, WsPayload, WsRecv, WsSend};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio_tungstenite::{WebSocketStream, client_async_with_config};
use utils::http::redact_url;

use crate::capability::{cap_runtime, run_on_cap_runtime};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest one `ws.*` call may block the plugin's invoke thread.
const MAX_WAIT: Duration = Duration::from_secs(30);

/// A peer that stops reading for this long ends the session.
#[cfg(not(test))]
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);

const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Inbound messages / bytes a plugin may leave unread before its session is
/// dropped, bounding host memory without ever stalling the socket pump. The
/// byte bound grows to two max-size messages for sessions that raise the cap.
const MAX_BACKLOG: usize = 1024;
const MAX_BACKLOG_BYTES: usize = 8 << 20;

/// Largest inbound message (and frame: some peers, e.g. Home Assistant, send
/// multi-MiB replies as one unfragmented frame) unless the plugin asks for more.
const DEFAULT_MESSAGE: usize = 1 << 20;
const MAX_MESSAGE: usize = 16 << 20;

const MAX_SESSIONS: usize = 16;

/// Unread bytes one plugin's sessions may hold together, and all plugins'
/// sessions daemon-wide. A session whose message would overrun either is
/// dropped; an open that cannot fit two max-size messages is refused.
#[cfg(not(test))]
const PLUGIN_BUDGET: usize = 64 << 20;
#[cfg(test)]
const PLUGIN_BUDGET: usize = 8 << 20;
const DAEMON_BUDGET: usize = 256 << 20;

static DAEMON_USED: AtomicUsize = AtomicUsize::new(0);

/// Close codes a plugin may send (RFC 6455 §7.4; 1004-1006 and 1015 are
/// reserved for the endpoint, never sent on the wire).
fn valid_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

/// Handshake headers the host owns: the WebSocket upgrade itself and
/// hop-by-hop headers.
const RESERVED_HEADERS: &[&str] = &[
    "host",
    "upgrade",
    "connection",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "content-length",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

enum Cmd {
    Send(Message, oneshot::Sender<Result<()>>),
    Close(u16, oneshot::Sender<()>),
}

type Inbound = Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Result<WsChunk>>>>;

struct Backlog {
    count: AtomicUsize,
    bytes: AtomicUsize,
    max_bytes: usize,
    plugin: Arc<AtomicUsize>,
}

impl Backlog {
    fn new(max_message: usize, plugin: Arc<AtomicUsize>) -> Self {
        Self {
            count: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            max_bytes: MAX_BACKLOG_BYTES.max(2 * max_message),
            plugin,
        }
    }

    /// Account one queued message; false once any bound is exceeded. Bytes
    /// stay charged until popped or the backlog drops.
    fn push(&self, len: usize) -> bool {
        let count = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        let bytes = self.bytes.fetch_add(len, Ordering::Relaxed) + len;
        let plugin = self.plugin.fetch_add(len, Ordering::Relaxed) + len;
        let daemon = DAEMON_USED.fetch_add(len, Ordering::Relaxed) + len;
        count <= MAX_BACKLOG
            && bytes <= self.max_bytes
            && plugin <= PLUGIN_BUDGET
            && daemon <= DAEMON_BUDGET
    }

    fn pop(&self, len: usize) {
        self.count.fetch_sub(1, Ordering::Relaxed);
        self.bytes.fetch_sub(len, Ordering::Relaxed);
        self.plugin.fetch_sub(len, Ordering::Relaxed);
        DAEMON_USED.fetch_sub(len, Ordering::Relaxed);
    }
}

impl Drop for Backlog {
    fn drop(&mut self) {
        let bytes = *self.bytes.get_mut();
        self.plugin.fetch_sub(bytes, Ordering::Relaxed);
        DAEMON_USED.fetch_sub(bytes, Ordering::Relaxed);
    }
}

fn fits(used: &AtomicUsize, limit: usize, need: usize) -> bool {
    limit.saturating_sub(used.load(Ordering::Relaxed)) >= need
}

fn chunk_len(chunk: &WsChunk) -> usize {
    match chunk {
        WsChunk::Text { text } => text.len(),
        WsChunk::Binary { bytes } => bytes.len(),
        WsChunk::Open { .. } => 0,
    }
}

struct Session {
    cmds: mpsc::UnboundedSender<Cmd>,
    inbound: Inbound,
    backlog: Arc<Backlog>,
    max_message: usize,
    pump: JoinHandle<()>,
}

/// One plugin's sessions. Ids are per plugin, so an id names nothing outside
/// its owner.
#[derive(Default)]
struct Owned {
    next: u64,
    sessions: HashMap<u64, Session>,
    budget: Arc<AtomicUsize>,
}

fn sessions() -> &'static Mutex<HashMap<String, Owned>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Owned>>> = OnceLock::new();
    SESSIONS.get_or_init(Default::default)
}

fn table() -> std::sync::MutexGuard<'static, HashMap<String, Owned>> {
    sessions().lock().unwrap_or_else(|p| p.into_inner())
}

thread_local! {
    static INVOKE_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Run `f` with `ws.*` waits on this thread capped at `deadline`, the
/// enclosing invoke's ceiling.
pub(crate) fn with_invoke_deadline<R>(deadline: Instant, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Instant>);
    impl Drop for Restore {
        fn drop(&mut self) {
            INVOKE_DEADLINE.set(self.0);
        }
    }
    let _restore = Restore(INVOKE_DEADLINE.replace(Some(deadline)));
    f()
}

fn wait_limit(requested_ms: Option<u64>) -> Duration {
    let wait = requested_ms
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis)
        .min(MAX_WAIT);
    match INVOKE_DEADLINE.get() {
        Some(deadline) => wait.min(deadline.saturating_duration_since(Instant::now())),
        None => wait,
    }
}

fn message_limit(requested: Option<u64>) -> usize {
    requested.map_or(DEFAULT_MESSAGE, |n| {
        usize::try_from(n).unwrap_or(usize::MAX).min(MAX_MESSAGE)
    })
}

fn session_limit(principal: &str) -> anyhow::Error {
    anyhow!("ws.open: plugin '{principal}' already holds {MAX_SESSIONS} sessions; close one first")
}

/// Open a WebSocket for `principal` and register its session.
pub(crate) fn open(req: WsOpen, principal: &str) -> Result<WsChunk> {
    let max_message = message_limit(req.max_message_bytes);
    let budget = {
        let mut table = table();
        let owned = table.entry(principal.to_string()).or_default();
        if owned.sessions.len() >= MAX_SESSIONS {
            return Err(session_limit(principal));
        }
        Arc::clone(&owned.budget)
    };
    let need = 2 * max_message;
    if !fits(&budget, PLUGIN_BUDGET, need) || !fits(&DAEMON_USED, DAEMON_BUDGET, need) {
        bail!(
            "ws.open: not enough unread-message budget for {need} bytes; read or close a session first"
        );
    }
    let wait = wait_limit(req.timeout_ms);
    let url = redact_url(&req.url);
    let ws = run_on_cap_runtime(async move { timeout(wait, connect(req, max_message)).await })?
        .map_err(|_| anyhow!("ws.open: '{url}' timed out after {wait:?}"))??;

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let backlog = Arc::new(Backlog::new(max_message, budget));
    let pump = cap_runtime().spawn(pump(ws, cmd_rx, in_tx, Arc::clone(&backlog)));
    let session = Session {
        cmds: cmd_tx,
        inbound: Arc::new(tokio::sync::Mutex::new(in_rx)),
        backlog,
        max_message,
        pump,
    };

    let mut table = table();
    let owned = table.entry(principal.to_string()).or_default();
    if owned.sessions.len() >= MAX_SESSIONS {
        drop(table);
        session.pump.abort();
        return Err(session_limit(principal));
    }
    owned.next += 1;
    let id = owned.next;
    owned.sessions.insert(id, session);
    Ok(WsChunk::Open { session: id })
}

/// Next inbound message; `None` once the peer closed cleanly. A dropped
/// connection is an error. Either ends the session.
pub(crate) fn recv(req: WsRecv, principal: &str) -> Result<Option<WsChunk>> {
    let (inbound, backlog) = with_session(principal, req.session, "ws.recv", |s| {
        (Arc::clone(&s.inbound), Arc::clone(&s.backlog))
    })?;
    let wait = wait_limit(req.timeout_ms);
    let got = run_on_cap_runtime(async move {
        timeout(wait, async { inbound.lock().await.recv().await }).await
    })?;
    match got {
        Err(_) => bail!("ws.recv: no message within {wait:?}"),
        Ok(Some(Ok(chunk))) => {
            backlog.pop(chunk_len(&chunk));
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
    let (cmds, max_message) = with_session(principal, req.session, "ws.send", |s| {
        (s.cmds.clone(), s.max_message)
    })?;
    let msg = match req.payload {
        WsPayload::Text(text) => Message::text(text),
        WsPayload::Binary(bytes) => Message::binary(bytes),
    };
    if msg.len() > max_message {
        bail!(
            "ws.send: {} bytes exceeds the session's {max_message}-byte message limit",
            msg.len()
        );
    }
    let closed = || anyhow!("ws.send: session {} is closed", req.session);
    let (ack, done) = oneshot::channel();
    cmds.send(Cmd::Send(msg, ack)).map_err(|_| closed())?;
    let wait = wait_limit(None);
    let result = match run_on_cap_runtime(async move { timeout(wait, done).await })? {
        Err(_) => bail!("ws.send: no ack within {wait:?}"),
        Ok(Err(_)) => Err(closed()),
        Ok(Ok(result)) => result,
    };
    // The pump closes its command queue when the send ended the session.
    if result.is_err() && cmds.is_closed() {
        remove(principal, req.session);
    }
    result
}

pub(crate) fn close(req: WsClose, principal: &str) -> Result<()> {
    let code = req.code.unwrap_or(1000);
    if !valid_close_code(code) {
        bail!("ws.close: close code {code} is not allowed");
    }
    let session = remove(principal, req.session)
        .ok_or_else(|| anyhow!("ws.close: no open session {}", req.session))?;
    let (ack, done) = oneshot::channel();
    if session.cmds.send(Cmd::Close(code, ack)).is_ok() {
        let pump = session.pump;
        run_on_cap_runtime(async move {
            if timeout(CLOSE_TIMEOUT, done).await.is_err() {
                pump.abort();
            }
        })?;
    }
    Ok(())
}

/// Drop every session `principal` holds; called when its process exits.
pub fn drop_sessions(principal: &str) {
    let Some(owned) = table().remove(principal) else {
        return;
    };
    for (_, session) in owned.sessions {
        let Session { cmds, mut pump, .. } = session;
        // A dropped command queue makes the pump send a close frame; abort it
        // if that does not finish in time.
        drop(cmds);
        cap_runtime().spawn(async move {
            if timeout(CLOSE_TIMEOUT, &mut pump).await.is_err() {
                pump.abort();
            }
        });
    }
}

#[cfg(test)]
pub(crate) fn session_count(principal: &str) -> usize {
    table().get(principal).map_or(0, |o| o.sessions.len())
}

#[cfg(test)]
pub(crate) struct IdleSession(mpsc::UnboundedReceiver<Cmd>);

#[cfg(test)]
impl IdleSession {
    pub(crate) fn dropped(&mut self) -> bool {
        matches!(
            self.0.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        )
    }
}

/// Register a session with no socket behind it.
#[cfg(test)]
pub(crate) fn insert_idle_session(principal: &str) -> IdleSession {
    let (cmds, cmd_rx) = mpsc::unbounded_channel();
    let (_, in_rx) = mpsc::unbounded_channel();
    let pump = cap_runtime().spawn(std::future::pending());
    let mut table = table();
    let owned = table.entry(principal.to_string()).or_default();
    owned.next += 1;
    let id = owned.next;
    let backlog = Arc::new(Backlog::new(DEFAULT_MESSAGE, Arc::clone(&owned.budget)));
    owned.sessions.insert(
        id,
        Session {
            cmds,
            inbound: Arc::new(tokio::sync::Mutex::new(in_rx)),
            backlog,
            max_message: DEFAULT_MESSAGE,
            pump,
        },
    );
    IdleSession(cmd_rx)
}

fn remove(principal: &str, session: u64) -> Option<Session> {
    table().get_mut(principal)?.sessions.remove(&session)
}

fn with_session<R>(
    principal: &str,
    session: u64,
    op: &str,
    f: impl FnOnce(&Session) -> R,
) -> Result<R> {
    table()
        .get(principal)
        .and_then(|o| o.sessions.get(&session))
        .map(f)
        .ok_or_else(|| anyhow!("{op}: no open session {session}"))
}

async fn connect(req: WsOpen, max_message: usize) -> Result<WebSocketStream<Box<dyn Io>>> {
    let mut request = req
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| anyhow!("ws.open: bad URL '{}': {e}", redact_url(&req.url)))?;
    for (k, v) in &req.headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| anyhow!("ws.open: bad header name '{k}': {e}"))?;
        if RESERVED_HEADERS.contains(&name.as_str()) {
            bail!("ws.open: header '{k}' is set by the host");
        }
        let value = HeaderValue::from_str(v)
            .map_err(|e| anyhow!("ws.open: bad header value for '{k}': {e}"))?;
        request.headers_mut().append(name, value);
    }
    let uri = request.uri().clone();
    let tls = match uri.scheme_str() {
        Some("wss") => true,
        Some("ws") => false,
        _ => bail!(
            "ws.open: '{}' is not a ws:// or wss:// URL",
            redact_url(&req.url)
        ),
    };
    let host = uri
        .host()
        .ok_or_else(|| anyhow!("ws.open: '{}' has no host", redact_url(&req.url)))?
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
    let config = WebSocketConfig::default()
        .max_message_size(Some(max_message))
        .max_frame_size(Some(max_message));
    let (ws, _) = client_async_with_config(request, io, Some(config))
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
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &signature_algorithms())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &signature_algorithms())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        signature_algorithms().supported_schemes()
    }
}

fn signature_algorithms() -> rustls::crypto::WebPkiSupportedAlgorithms {
    rustls::crypto::ring::default_provider().signature_verification_algorithms
}

/// Own one socket: run commands, queue inbound data. Pings are answered by
/// tungstenite while the stream is polled; they never reach the plugin.
async fn pump(
    mut ws: WebSocketStream<Box<dyn Io>>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    inbound: mpsc::UnboundedSender<Result<WsChunk>>,
    backlog: Arc<Backlog>,
) {
    use futures::{SinkExt, StreamExt};
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Send(msg, ack)) => match timeout(WRITE_TIMEOUT, ws.send(msg)).await {
                    Ok(sent) => {
                        _ = ack.send(sent.map_err(|e| anyhow!("ws.send: {e}")));
                    }
                    Err(_) => {
                        let ended = || anyhow!("ws.send: peer stalled for {WRITE_TIMEOUT:?}; session ended");
                        cmds.close();
                        _ = inbound.send(Err(ended()));
                        _ = ack.send(Err(ended()));
                        return;
                    }
                },
                Some(Cmd::Close(code, ack)) => {
                    let frame = CloseFrame {
                        code: CloseCode::from(code),
                        reason: "".into(),
                    };
                    _ = timeout(CLOSE_TIMEOUT, ws.close(Some(frame))).await;
                    _ = ack.send(());
                    return;
                }
                None => {
                    _ = timeout(CLOSE_TIMEOUT, ws.close(None)).await;
                    return;
                }
            },
            frame = ws.next() => {
                let chunk = match frame {
                    Some(Ok(Message::Text(text))) => WsChunk::Text { text: text.to_string() },
                    Some(Ok(Message::Binary(bytes))) => WsChunk::Binary { bytes: Bytes::into(bytes) },
                    Some(Ok(Message::Close(_))) => {
                        // Flushes the queued close reply.
                        _ = timeout(CLOSE_TIMEOUT, ws.close(None)).await;
                        return;
                    }
                    None => return,
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        _ = inbound.send(Err(anyhow!("ws: connection dropped: {e}")));
                        return;
                    }
                };
                if !backlog.push(chunk_len(&chunk)) {
                    _ = inbound.send(Err(anyhow!(
                        "ws: plugin left over {MAX_BACKLOG} messages or {} bytes unread; session dropped",
                        backlog.max_bytes
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
        /// Send `count` binary messages of `size` bytes, then idle.
        Flood { count: usize, size: usize },
        /// Complete the handshake, then never read.
        Stall,
    }

    /// WebSocket server on loopback accepting any number of connections. The
    /// receiver yields the close code each client sent, if any.
    fn serve(peer: Peer, tls: bool) -> (String, std::sync::mpsc::Receiver<Option<u16>>) {
        serve_with(peer, tls.then(|| tls_acceptor(false)))
    }

    fn serve_with(
        peer: Peer,
        acceptor: Option<tokio_rustls::TlsAcceptor>,
    ) -> (String, std::sync::mpsc::Receiver<Option<u16>>) {
        let tls = acceptor.is_some();
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
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                loop {
                    let (tcp, _) = listener.accept().await.unwrap();
                    let (tx, acceptor) = (tx.clone(), acceptor.clone());
                    tokio::spawn(async move {
                        let io: Box<dyn Io> = match acceptor {
                            Some(acceptor) => match acceptor.accept(tcp).await {
                                Ok(stream) => Box::new(stream),
                                Err(_) => return,
                            },
                            None => Box::new(tcp),
                        };
                        if let Ok(ws) = tokio_tungstenite::accept_async(io).await {
                            run_peer(ws, peer, tx).await;
                        }
                    });
                }
            });
        });
        let scheme = if tls { "wss" } else { "ws" };
        (format!("{scheme}://{addr}/"), rx)
    }

    fn server(peer: Peer) -> (String, std::sync::mpsc::Receiver<Option<u16>>) {
        serve(peer, false)
    }

    #[derive(Debug)]
    struct FixedCert(Arc<rustls::sign::CertifiedKey>);

    impl rustls::server::ResolvesServerCert for FixedCert {
        fn resolve(
            &self,
            _: rustls::server::ClientHello<'_>,
        ) -> Option<Arc<rustls::sign::CertifiedKey>> {
            Some(self.0.clone())
        }
    }

    fn tls_acceptor(impostor: bool) -> tokio_rustls::TlsAcceptor {
        utils::http::ensure_crypto_provider();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let signing_key = if impostor {
            rcgen::KeyPair::generate().unwrap()
        } else {
            cert.signing_key
        };
        let key = rustls::pki_types::PrivateKeyDer::try_from(signing_key.serialize_der()).unwrap();
        let signer = rustls::crypto::ring::default_provider()
            .key_provider
            .load_private_key(key)
            .unwrap();
        let ck = rustls::sign::CertifiedKey::new(vec![cert.cert.der().clone()], signer);
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(FixedCert(Arc::new(ck))));
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    }

    async fn run_peer(
        mut ws: WebSocketStream<Box<dyn Io>>,
        peer: Peer,
        tx: std::sync::mpsc::Sender<Option<u16>>,
    ) {
        use futures::{SinkExt, StreamExt};
        match peer {
            Peer::GreetThenClose => {
                ws.send(Message::text("hello")).await.unwrap();
                ws.close(None).await.ok();
                return;
            }
            Peer::Flood { count, size } => {
                for _ in 0..count {
                    if ws.send(Message::binary(vec![7u8; size])).await.is_err() {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
                return;
            }
            Peer::Stall => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                return;
            }
            Peer::Echo | Peer::Vanish => {}
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
            assert!(err.contains("no open session"), "{cap}: {err}");
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
    fn open_errors_omit_url_query() {
        for url in [
            "http://127.0.0.1:1/x?token=s3cr3t",
            "not a url?token=s3cr3t",
        ] {
            let err = handle_cap("ws.open", json!({"url": url}), "ws-bad-q")
                .unwrap_err()
                .to_string();
            assert!(!err.contains("s3cr3t"), "{url}: {err}");
        }
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

    /// Receive until the session errors; panics if it never does.
    fn recv_until_error(session: u64, principal: &str) -> String {
        for _ in 0..=MAX_BACKLOG + 1 {
            if let Err(e) = recv(session, principal) {
                return e.to_string();
            }
        }
        panic!("session never errored");
    }

    #[test]
    fn unread_message_count_overflow_drops_the_session() {
        let (url, _closed) = server(Peer::Flood {
            count: MAX_BACKLOG + 10,
            size: 1,
        });
        let s = open(&url, "ws-flood-count");
        std::thread::sleep(Duration::from_secs(1));
        let err = recv_until_error(s, "ws-flood-count");
        assert!(err.contains("unread"), "got: {err}");
        assert_eq!(session_count("ws-flood-count"), 0);
    }

    #[test]
    fn unread_byte_overflow_drops_the_session() {
        let (url, _closed) = server(Peer::Flood {
            count: 50,
            size: 200_000,
        });
        let s = open(&url, "ws-flood-bytes");
        std::thread::sleep(Duration::from_secs(1));
        let err = recv_until_error(s, "ws-flood-bytes");
        assert!(err.contains("unread"), "got: {err}");
    }

    #[test]
    fn oversize_message_is_refused() {
        let (url, _closed) = server(Peer::Flood {
            count: 1,
            size: DEFAULT_MESSAGE * 2,
        });
        let s = open(&url, "ws-oversize");
        let err = recv(s, "ws-oversize").unwrap_err().to_string();
        assert!(err.contains("connection dropped"), "got: {err}");
        assert_eq!(session_count("ws-oversize"), 0);
    }

    #[test]
    fn raised_message_cap_accepts_a_large_single_frame() {
        let (url, _closed) = server(Peer::Flood {
            count: 1,
            size: DEFAULT_MESSAGE * 2,
        });
        let reply = handle_cap(
            "ws.open",
            json!({"url": url, "max_message_bytes": 4 << 20}),
            "ws-large",
        )
        .unwrap();
        let WsChunk::Open { session } = serde_json::from_value(reply).unwrap() else {
            panic!("expected Open");
        };
        let msg: WsChunk = serde_json::from_value(recv(session, "ws-large").unwrap()).unwrap();
        assert_eq!(chunk_len(&msg), DEFAULT_MESSAGE * 2);
    }

    #[test]
    fn message_cap_is_defaulted_and_clamped() {
        assert_eq!(message_limit(None), DEFAULT_MESSAGE);
        assert_eq!(message_limit(Some(4 << 20)), 4 << 20);
        assert_eq!(message_limit(Some(64 << 20)), MAX_MESSAGE);
        assert_eq!(message_limit(Some(u64::MAX)), MAX_MESSAGE);
        let budget = Arc::new(AtomicUsize::new(0));
        assert_eq!(
            Backlog::new(MAX_MESSAGE, Arc::clone(&budget)).max_bytes,
            2 * MAX_MESSAGE
        );
        assert_eq!(
            Backlog::new(DEFAULT_MESSAGE, budget).max_bytes,
            MAX_BACKLOG_BYTES
        );
    }

    fn budget_used(principal: &str) -> usize {
        table()
            .get(principal)
            .map_or(0, |o| o.budget.load(Ordering::Relaxed))
    }

    /// Open a flood session and wait until all `count` messages are queued.
    fn open_flooded(count: usize, size: usize, principal: &str) -> u64 {
        let (url, _closed) = server(Peer::Flood { count, size });
        let before = budget_used(principal);
        let s = open(&url, principal);
        let deadline = Instant::now() + Duration::from_secs(5);
        while budget_used(principal) < before + count * size && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        s
    }

    #[test]
    fn plugin_budget_overrun_drops_the_offending_session() {
        let p = "ws-budget-two";
        let first = open_flooded(25, 200_000, p);
        let (url, _closed) = server(Peer::Flood {
            count: 25,
            size: 200_000,
        });
        let second = open(&url, p);
        let err = recv_until_error(second, p);
        assert!(err.contains("unread"), "got: {err}");
        for _ in 0..25 {
            recv(first, p).unwrap();
        }
        assert_eq!(session_count(p), 1);
    }

    #[test]
    fn open_is_refused_until_budget_is_released() {
        let p = "ws-budget-open";
        let s = open_flooded(35, 200_000, p);
        let (url, _closed) = server(Peer::Echo);
        let err = handle_cap("ws.open", json!({"url": url}), p)
            .unwrap_err()
            .to_string();
        assert!(err.contains("budget"), "got: {err}");
        for _ in 0..35 {
            recv(s, p).unwrap();
        }
        assert_eq!(budget_used(p), 0);
        let reused = open(&url, p);
        handle_cap("ws.close", json!({"session": reused}), p).unwrap();

        let s = open_flooded(35, 200_000, p);
        handle_cap("ws.close", json!({"session": s}), p).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while budget_used(p) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(budget_used(p), 0);
        open(&url, p);
    }

    #[test]
    fn send_larger_than_the_message_cap_is_refused() {
        let (url, _closed) = server(Peer::Echo);
        let s = open(&url, "ws-send-cap");
        let text = "x".repeat(DEFAULT_MESSAGE + 1);
        let err = handle_cap(
            "ws.send",
            json!({"session": s, "text": text}),
            "ws-send-cap",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("message limit"), "got: {err}");
        assert_eq!(session_count("ws-send-cap"), 1);
    }

    #[test]
    fn reserved_close_codes_are_refused() {
        let (url, _closed) = server(Peer::Echo);
        let s = open(&url, "ws-close-code");
        for code in [999, 1004, 1005, 1006, 1015, 1016, 2999, 5000] {
            let err = handle_cap(
                "ws.close",
                json!({"session": s, "code": code}),
                "ws-close-code",
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("not allowed"), "{code}: {err}");
        }
        assert_eq!(session_count("ws-close-code"), 1);
        for code in [1000, 1003, 1007, 1014, 3000, 4999] {
            assert!(valid_close_code(code), "{code}");
        }
    }

    #[test]
    fn sessions_per_plugin_are_capped() {
        let (url, _closed) = server(Peer::Echo);
        let ids: Vec<u64> = (0..MAX_SESSIONS).map(|_| open(&url, "ws-cap")).collect();
        let err = handle_cap("ws.open", json!({"url": url}), "ws-cap")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already holds"), "got: {err}");
        assert!(open(&url, "ws-cap-other") > 0);
        handle_cap("ws.close", json!({"session": ids[0]}), "ws-cap").unwrap();
        open(&url, "ws-cap");
        assert_eq!(session_count("ws-cap"), MAX_SESSIONS);
    }

    #[test]
    fn stalled_peer_on_send_ends_the_session() {
        let (url, _closed) = server(Peer::Stall);
        let s = open(&url, "ws-stall");
        let chunk = "A".repeat(200_000);
        let err = (0..500)
            .find_map(|_| {
                handle_cap(
                    "ws.send",
                    json!({"session": s, "binary": chunk}),
                    "ws-stall",
                )
                .err()
            })
            .expect("send never stalled")
            .to_string();
        assert!(err.contains("stalled"), "got: {err}");
        assert_eq!(session_count("ws-stall"), 0);
    }

    #[test]
    fn reserved_headers_are_refused() {
        for name in ["Host", "upgrade", "Sec-WebSocket-Key", "Transfer-Encoding"] {
            let err = handle_cap(
                "ws.open",
                json!({"url": "ws://127.0.0.1:1/", "headers": [[name, "x"]]}),
                "ws-headers",
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("is set by the host"), "{name}: {err}");
        }
        let (url, _closed) = server(Peer::Echo);
        let reply = handle_cap(
            "ws.open",
            json!({"url": url, "headers": [
                ["Authorization", "Bearer t"],
                ["Sec-WebSocket-Protocol", "chat"],
            ]}),
            "ws-headers",
        );
        // The echo server negotiates no subprotocol, so only a handshake
        // error (not a header refusal) is acceptable.
        if let Err(e) = reply {
            assert!(!e.to_string().contains("set by the host"), "got: {e}");
        }
    }

    #[test]
    fn wss_insecure_refuses_a_server_without_the_cert_private_key() {
        let (url, _closed) = serve_with(Peer::Echo, Some(tls_acceptor(true)));
        let err = handle_cap(
            "ws.open",
            json!({"url": url, "insecure": true}),
            "ws-tls-impostor",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("TLS handshake"), "got: {err}");
    }

    #[test]
    fn wss_insecure_accepts_a_self_signed_server() {
        let (url, _closed) = serve(Peer::Echo, true);
        let reply = handle_cap("ws.open", json!({"url": url, "insecure": true}), "ws-tls").unwrap();
        let WsChunk::Open { session } = serde_json::from_value(reply).unwrap() else {
            panic!("expected Open");
        };
        handle_cap(
            "ws.send",
            json!({"session": session, "text": "tls"}),
            "ws-tls",
        )
        .unwrap();
        assert_eq!(
            recv(session, "ws-tls").unwrap(),
            json!({"t": "text", "text": "tls"})
        );
        let err = handle_cap("ws.open", json!({"url": url}), "ws-tls-strict")
            .unwrap_err()
            .to_string();
        assert!(err.contains("TLS handshake"), "got: {err}");
    }

    #[test]
    fn recv_wait_is_clamped_to_the_invoke_deadline() {
        let (url, _closed) = server(Peer::Echo);
        let s = open(&url, "ws-deadline");
        let started = Instant::now();
        let err = with_invoke_deadline(Instant::now() + Duration::from_millis(100), || {
            recv(s, "ws-deadline")
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("no message within"), "got: {err}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(wait_limit(Some(u64::MAX)), MAX_WAIT);
    }
}
