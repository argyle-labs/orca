//! TCP + mTLS plugin host — accepts connections from local and remote plugins.
//!
//! Listens on `0.0.0.0:<APP_PLUGIN_PORT>` (default 12002). Requires client
//! certificates signed by the orca CA. Dispatches JSON-RPC 2.0 frames.
//!
// JSON-RPC dispatcher between host and plugins; HashMap/Value are protocol-level passthrough.
#![allow(clippy::disallowed_types)]
//! Phase A methods:
//!   orca/hello  — version + capability handshake. Returns `full` when all
//!                 required and optional methods are supported, `degraded`
//!                 when only optional methods are missing, and `rejected`
//!                 when the server version is below `core_min_required` or
//!                 a required method is unavailable.

use anyhow::{Context, Result};
use orca_sdk::framing::{read_frame, write_frame};
use orca_sdk::jsonrpc::{ErrorObject, Message, Request, Response};
use orca_sdk::pki;
use orca_sdk::tools::{
    PLUGINS_LIST_METHOD, PeerInfo, PluginsListResult, TOOLS_CALL_METHOD, TOOLS_DECLARE_METHOD,
    TOOLS_INVOKE_METHOD, ToolCallParams, ToolInvokeParams, ToolInvokeResult, ToolsDeclareParams,
    ToolsDeclareResult,
};
use orca_sdk::transport::{
    CONTEXT_EVENT_METHOD, ContextEvent, ContextPublishParams, ContextSubscribeParams,
    ContextSubscribeResult, ContextUnsubscribeParams, HelloParams, HelloResult, TypedValue,
    TypesDeclareParams, TypesDeclareResult,
};
use rustls::ServerConfig;
use rustls::server::WebPkiClientVerifier;
use serde_json::json;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

/// Tuple sent across a context's broadcast channel: `(context_id, value)`.
type ContextEnvelope = (String, TypedValue);
type ContextChannels = HashMap<String, broadcast::Sender<ContextEnvelope>>;

/// In-memory registry of named contexts. Each context has an associated
/// broadcast channel that fans `TypedValue` events out to all current
/// subscribers. Contexts are created lazily on first publish or subscribe.
#[derive(Clone, Default)]
pub struct ContextRegistry {
    inner: Arc<StdMutex<ContextChannels>>,
}

impl ContextRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the broadcast sender for `context_id`, creating it if needed.
    /// The tuple sent is `(context_id, value)` so subscriber tasks can label
    /// outgoing events without round-tripping through the registry.
    fn channel(&self, context_id: &str) -> broadcast::Sender<ContextEnvelope> {
        let mut map = self.inner.lock().unwrap();
        if let Some(tx) = map.get(context_id) {
            return tx.clone();
        }
        let (tx, _) = broadcast::channel(256);
        map.insert(context_id.to_string(), tx.clone());
        tx
    }
}

// ── Outbound call infrastructure ─────────────────────────────────────────────

/// Pending outbound requests we've sent to the plugin and are waiting on
/// a Response for. Keyed by JSON-RPC id.
type Pending = Arc<StdMutex<HashMap<u64, oneshot::Sender<Response>>>>;

/// Handle external code (e.g. the MCP tool registry bridge) holds to talk
/// to a connected plugin. Cloning is cheap — it shares the underlying
/// channel + pending demux. Dropped when the plugin disconnects.
#[derive(Clone)]
pub struct ConnHandle {
    plugin_id: String,
    /// Plugin version announced in `orca/hello.plugin_version`. Empty when
    /// the peer didn't declare one (legacy SDK clients).
    plugin_version: String,
    /// Outbound JSON-RPC frames the writer half of the connection drains.
    outbound: mpsc::UnboundedSender<serde_json::Value>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

impl ConnHandle {
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub fn plugin_version(&self) -> &str {
        &self.plugin_version
    }

    /// Send a JSON-RPC request to the plugin and await the matching
    /// response. Times out after `timeout` to avoid leaking pending
    /// entries when a plugin handler hangs.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<Response> {
        let id = self.next_id.fetch_add(1, AtomicOrdering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let req = Request::new(id, method, Some(params));
        let envelope = serde_json::to_value(&req).context("serialize outbound request")?;
        if self.outbound.send(envelope).is_err() {
            self.pending.lock().unwrap().remove(&id);
            anyhow::bail!("plugin '{}' is no longer connected", self.plugin_id);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => {
                anyhow::bail!("plugin '{}' disconnected before response", self.plugin_id)
            }
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                anyhow::bail!(
                    "plugin '{}' did not respond to {} within {:?}",
                    self.plugin_id,
                    method,
                    timeout
                )
            }
        }
    }

    /// Convenience wrapper around `orca/tools.call`. Returns the opaque
    /// JSON inside `ToolCallResult.result`, or surfaces the JSON-RPC error.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        let params = ToolCallParams {
            name: name.into(),
            arguments,
        };
        let resp = self
            .call(TOOLS_CALL_METHOD, serde_json::to_value(&params)?, timeout)
            .await?;
        if let Some(err) = resp.error {
            anyhow::bail!("tool '{name}' failed: code={} {}", err.code, err.message);
        }
        let result = resp
            .result
            .context("tools.call returned neither result nor error")?;
        // ToolCallResult wraps the tool's payload at "result".
        Ok(result
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }
}

/// Process-wide registry of currently-connected plugins. The plugin host
/// registers a `ConnHandle` on `orca/hello`; external code (MCP layer,
/// schedulers, etc.) looks up plugins by id to invoke their tools.
#[derive(Clone, Default)]
pub struct PluginRegistry {
    inner: Arc<StdMutex<HashMap<String, ConnHandle>>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, handle: ConnHandle) {
        self.inner
            .lock()
            .unwrap()
            .insert(handle.plugin_id.clone(), handle);
    }

    pub fn unregister(&self, plugin_id: &str) {
        self.inner.lock().unwrap().remove(plugin_id);
    }

    pub fn get(&self, plugin_id: &str) -> Option<ConnHandle> {
        self.inner.lock().unwrap().get(plugin_id).cloned()
    }

    pub fn connected_ids(&self) -> Vec<String> {
        self.inner.lock().unwrap().keys().cloned().collect()
    }

    /// Snapshot of every connected peer's `(id, version)`. Powers
    /// `orca/plugins.list`.
    pub fn connected_peers(&self) -> Vec<PeerInfo> {
        self.inner
            .lock()
            .unwrap()
            .values()
            .map(|h| PeerInfo {
                id: h.plugin_id.clone(),
                version: h.plugin_version.clone(),
            })
            .collect()
    }
}

/// Process-global handle to the plugin registry. Set by `start()` so HTTP
/// handlers and the MCP bridge — which don't otherwise have a reference to
/// it — can reach connected plugins without threading the value through every
/// router state struct.
static GLOBAL_REGISTRY: OnceLock<PluginRegistry> = OnceLock::new();

/// Returns the process-global PluginRegistry. None until `plugin_host::start`
/// has been called (e.g. unit tests that don't spin up the host).
pub fn global() -> Option<PluginRegistry> {
    GLOBAL_REGISTRY.get().cloned()
}

/// Install `registry` as the process-global. Idempotent: subsequent calls are
/// silently ignored so duplicate `start()` invocations (dev rebuild paths) are
/// safe.
fn install_global(registry: PluginRegistry) -> PluginRegistry {
    let _ = GLOBAL_REGISTRY.set(registry.clone());
    GLOBAL_REGISTRY.get().cloned().unwrap_or(registry)
}

/// Start the plugin host in a background task. Returns immediately.
/// If the PKI directory doesn't contain a CA, logs a warning and skips the host.
/// `plugin_registry` is shared so callers (e.g. the MCP bridge) can look up
/// connected plugins by id and invoke their tools.
pub fn start(
    pki_dir: &Path,
    port: u16,
    plugin_registry: PluginRegistry,
) -> tokio::task::JoinHandle<()> {
    let pki_dir = pki_dir.to_owned();
    let plugin_registry = install_global(plugin_registry);
    tokio::spawn(async move {
        if let Err(e) = run(&pki_dir, port, plugin_registry).await {
            warn!("[plugin-host] failed to start: {e:#}");
        }
    })
}

async fn run(pki_dir: &Path, port: u16, plugin_registry: PluginRegistry) -> Result<()> {
    let (listener, acceptor, addr) = bind(pki_dir, port).await?;
    info!("[plugin-host] listening on {addr} (mTLS)");
    serve(listener, acceptor, ContextRegistry::new(), plugin_registry).await
}

/// Bind a TCP listener and build the mTLS acceptor for the plugin host.
/// Returns the listener, acceptor, and the actual bound address (useful when
/// `port == 0` lets the OS pick an ephemeral port).
pub async fn bind(pki_dir: &Path, port: u16) -> Result<(TcpListener, TlsAcceptor, SocketAddr)> {
    let acceptor = build_acceptor(pki_dir)?;

    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("plugin host bind {addr}"))?;
    let local = listener.local_addr().context("listener.local_addr")?;
    Ok((listener, acceptor, local))
}

/// Run the accept loop until the listener errors. Each connection is handled
/// in its own task. The supplied `registry` is shared across all connections
/// so a publish on one connection fans out to subscribers on others.
pub async fn serve(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    registry: ContextRegistry,
    plugins: PluginRegistry,
) -> Result<()> {
    loop {
        match listener.accept().await {
            Ok((tcp, peer)) => {
                let acceptor = acceptor.clone();
                let registry = registry.clone();
                let plugins = plugins.clone();
                tokio::spawn(async move {
                    match acceptor.accept(tcp).await {
                        Ok(tls) => {
                            let sni = tls
                                .get_ref()
                                .1
                                .server_name()
                                .map(str::to_string)
                                .unwrap_or_default();

                            // Bootstrap SNI: pre-join channel. No client cert
                            // ever — the whole point is that the joiner has
                            // no cert yet. Trust is established at the next
                            // layer via pinned pubkey + pairing code.
                            if sni == pki::POD_BOOTSTRAP_SAN {
                                if let Err(e) =
                                    crate::pod::handle_pod_bootstrap_connection(tls, peer).await
                                {
                                    warn!("[plugin-host] {peer} bootstrap connection error: {e:#}");
                                }
                                return;
                            }

                            // Every other surface requires a verified client cert.
                            let peer_cn = match extract_peer_cn(&tls).ok() {
                                Some(cn) => cn,
                                None => {
                                    warn!(
                                        "[plugin-host] {peer} connection lacks peer cert (sni={sni})"
                                    );
                                    return;
                                }
                            };

                            if sni == pki::POD_SERVER_SAN {
                                if let Err(e) =
                                    crate::pod::handle_pod_connection(tls, peer_cn).await
                                {
                                    warn!("[plugin-host] {peer} pod connection error: {e:#}");
                                }
                                return;
                            }

                            if let Err(e) = handle_connection(tls, registry, plugins, peer_cn).await
                            {
                                warn!("[plugin-host] {peer} connection error: {e:#}");
                            }
                        }
                        Err(e) => warn!("[plugin-host] {peer} TLS accept failed: {e}"),
                    }
                });
            }
            Err(e) => warn!("[plugin-host] accept error: {e}"),
        }
    }
}

/// Build the mTLS acceptor. Always serves the plugin CA's server cert under
/// SNI `core.orca.local` (and as the default when no SNI is presented). If a
/// mesh CA exists (i.e. `orca pod init` has run), additionally serves a
/// mesh-CA-signed cert under SNI `pod.orca.local` and trusts the mesh CA for
/// inbound client certs. Trust anchors are the union of both CAs so the
/// connection dispatcher can identify which surface the caller wants by SNI.
fn build_acceptor(pki_dir: &Path) -> Result<TlsAcceptor> {
    use rustls::crypto::CryptoProvider;
    use rustls::sign::CertifiedKey;

    let plugin_bundle = pki::load_server(pki_dir).context("load plugin server TLS bundle")?;
    let (plugin_chain, plugin_key) =
        pki::parse_cert_and_key(&plugin_bundle.cert_pem, &plugin_bundle.key_pem)?;
    let plugin_signing = CryptoProvider::get_default()
        .context("no rustls CryptoProvider installed")?
        .key_provider
        .load_private_key(plugin_key)
        .context("load plugin private key")?;
    let plugin_ck = Arc::new(CertifiedKey::new(plugin_chain, plugin_signing));

    // Trust store starts with the plugin CA, optionally extended with the mesh CA.
    let mut roots = pki::ca_root_store(&plugin_bundle.ca_cert_pem)?;

    if pki::mesh_ca_cert_path(pki_dir).exists() {
        let cur_ca = std::fs::read_to_string(pki::mesh_ca_cert_path(pki_dir))
            .context("read current mesh CA cert")?;
        let prev_ca = std::fs::read_to_string(pki::mesh_ca_previous_cert_path(pki_dir)).ok();
        use rustls_pemfile::certs;
        for der in certs(&mut cur_ca.as_bytes()) {
            roots.add(der.context("parsing current mesh CA cert")?)?;
        }
        if let Some(p) = &prev_ca {
            for der in certs(&mut p.as_bytes()) {
                roots.add(der.context("parsing previous mesh CA cert")?)?;
            }
            info!("[plugin-host] mesh CA two-slot overlap active (previous CA still trusted)");
        } else {
            info!("[plugin-host] mesh CA detected — pod SNI surface active");
        }
    }

    // Eagerly create the bootstrap cert+key so its file is on disk; the
    // resolver still reads from disk per-handshake. (Bootstrap rarely
    // rotates but the read path is the same for uniformity.)
    pki::load_or_init_bootstrap_cert(pki_dir).context("init bootstrap TLS cert")?;

    let resolver = Arc::new(HotReloadResolver {
        pki_dir: pki_dir.to_path_buf(),
        plugin_ck,
    });

    // The bootstrap SNI accepts no client cert; everything else now requires
    // a valid cert. The connection dispatcher (above) bypasses cert extraction
    // entirely for POD_BOOTSTRAP_SAN, so we can use `allow_unauthenticated()`
    // at the TLS layer and rely on the SNI-routing branch to gate trust.
    let client_cert_verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .allow_unauthenticated()
        .build()
        .context("build client cert verifier")?;

    let server_config = ServerConfig::builder()
        .with_client_cert_verifier(client_cert_verifier)
        .with_cert_resolver(resolver);

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

/// Hot-reload TLS cert resolver. For the pod + bootstrap SNIs we read cert+key
/// PEM from disk on every handshake — this is how seamless leaf-cert rotation
/// works: `pki::atomic_write_pem` does a tmp-write + rename(2), so a reader
/// either sees the old or the new file but never a half-written one. Cost is
/// microseconds per handshake (Ed25519 parse is trivial); benefit is zero
/// in-process cache to invalidate when rotation fires.
///
/// The legacy plugin SNI (`core.orca.local`) keeps the cached path — plugin
/// PKI is a separate trust system from the pod mesh and doesn't yet have a
/// rotation story. It gets re-read on daemon restart.
#[derive(Debug)]
struct HotReloadResolver {
    pki_dir: std::path::PathBuf,
    plugin_ck: Arc<rustls::sign::CertifiedKey>,
}

impl rustls::server::ResolvesServerCert for HotReloadResolver {
    fn resolve(
        &self,
        client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let sni = client_hello.server_name()?;
        match sni {
            "core.orca.local" => Some(self.plugin_ck.clone()),
            s if s == pki::POD_SERVER_SAN => self.load_pod_server_ck().ok().map(Arc::new),
            s if s == pki::POD_BOOTSTRAP_SAN => self.load_bootstrap_ck().ok().map(Arc::new),
            _ => None,
        }
    }
}

impl HotReloadResolver {
    fn load_pod_server_ck(&self) -> Result<rustls::sign::CertifiedKey> {
        let cert_pem = std::fs::read_to_string(pki::mesh_server_cert_path(&self.pki_dir))
            .context("read mesh server cert")?;
        let key_pem = std::fs::read_to_string(pki::mesh_server_key_path(&self.pki_dir))
            .context("read mesh server key")?;
        Self::build_ck(&cert_pem, &key_pem)
    }

    fn load_bootstrap_ck(&self) -> Result<rustls::sign::CertifiedKey> {
        let cert_pem = std::fs::read_to_string(pki::bootstrap_cert_path(&self.pki_dir))
            .context("read bootstrap cert")?;
        let key_pem = std::fs::read_to_string(pki::bootstrap_key_path(&self.pki_dir))
            .context("read bootstrap key")?;
        Self::build_ck(&cert_pem, &key_pem)
    }

    fn build_ck(cert_pem: &str, key_pem: &str) -> Result<rustls::sign::CertifiedKey> {
        use rustls::crypto::CryptoProvider;
        let (chain, key) = pki::parse_cert_and_key(cert_pem, key_pem)?;
        let signing = CryptoProvider::get_default()
            .context("no rustls CryptoProvider installed")?
            .key_provider
            .load_private_key(key)
            .context("load private key")?;
        Ok(rustls::sign::CertifiedKey::new(chain, signing))
    }
}

/// Pull the Subject CN out of the leaf cert presented by the peer during
/// the mTLS handshake. `WebPkiClientVerifier` has already validated the
/// chain, so we only need to read the CN; we do not re-verify the cert.
fn extract_peer_cn(tls: &tokio_rustls::server::TlsStream<tokio::net::TcpStream>) -> Result<String> {
    let (_, conn) = tls.get_ref();
    let certs = conn
        .peer_certificates()
        .context("peer presented no client cert (mTLS misconfigured?)")?;
    let leaf = certs.first().context("peer cert chain empty")?;
    pki::peer_common_name(leaf.as_ref())
}

/// Per-connection state carried across frames. Set by `orca/hello`; read by
/// every subsequent method that needs the plugin's identity.
struct ConnState {
    /// Subject CN from the peer's leaf cert. Authoritative — the plugin's
    /// claim in `orca/hello` must match this exactly.
    peer_cn: String,
    plugin_id: Option<String>,
    registry: ContextRegistry,
    /// Process-wide registry the connection registers itself with on hello
    /// and unregisters from on drop, so external code can dispatch
    /// `tools/call` to this plugin via [`PluginRegistry::get`].
    plugins: PluginRegistry,
    /// Outbound JSON-RPC frames (notifications + host-initiated requests)
    /// the writer half of the connection drains.
    notify_tx: mpsc::UnboundedSender<serde_json::Value>,
    /// Pending host→plugin calls awaiting their Response. Shared with the
    /// [`ConnHandle`] handed to external callers.
    pending: Pending,
    /// Outbound JSON-RPC id allocator, shared with the [`ConnHandle`].
    next_outbound_id: Arc<AtomicU64>,
    /// Active subscriptions; aborting the JoinHandle stops forwarding events.
    subscriptions: HashMap<String, tokio::task::JoinHandle<()>>,
}

impl Drop for ConnState {
    fn drop(&mut self) {
        for (_, handle) in self.subscriptions.drain() {
            handle.abort();
        }
        if let Some(id) = self.plugin_id.as_deref() {
            self.plugins.unregister(id);
        }
    }
}

async fn handle_connection(
    tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    registry: ContextRegistry,
    plugins: PluginRegistry,
    peer_cn: String,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(tls);
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
    let next_outbound_id = Arc::new(AtomicU64::new(1));
    let mut state = ConnState {
        peer_cn,
        plugin_id: None,
        registry,
        plugins,
        notify_tx,
        pending: pending.clone(),
        next_outbound_id: next_outbound_id.clone(),
        subscriptions: HashMap::new(),
    };

    loop {
        tokio::select! {
            biased;

            // Outgoing frames — notifications from subscription pumps and
            // host-initiated requests from external callers (e.g. ConnHandle::call).
            // Flush first so events don't queue up behind a slow client.
            Some(envelope) = notify_rx.recv() => {
                let bytes = serde_json::to_vec(&envelope)?;
                write_frame(&mut writer, &bytes).await?;
            }

            // Incoming frames.
            frame = read_frame(&mut reader) => {
                let frame = match frame {
                    Ok(f) => f,
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("unexpected end of file") || msg.contains("early eof") {
                            break;
                        }
                        return Err(e);
                    }
                };
                let response = dispatch(&mut state, &frame);
                if response.is_null() {
                    continue;
                }
                let response_bytes = serde_json::to_vec(&response)?;
                write_frame(&mut writer, &response_bytes).await?;
            }
        }
    }

    // Fail any in-flight outbound calls so awaiting tasks don't hang.
    pending.lock().unwrap().clear();
    Ok(())
}

fn dispatch(state: &mut ConnState, frame: &[u8]) -> serde_json::Value {
    let msg: Message = match serde_json::from_slice(frame) {
        Ok(m) => m,
        Err(e) => {
            return json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": format!("parse error: {e}") }
            });
        }
    };

    match msg {
        Message::Request(req) => {
            let id = req.id.clone();
            match req.method.as_str() {
                "orca/hello" => handle_hello(state, id, req.params),
                "orca/types.declare" => handle_types_declare(state, id, req.params),
                "orca/context.publish" => handle_context_publish(state, id, req.params),
                "orca/context.subscribe" => handle_context_subscribe(state, id, req.params),
                "orca/context.unsubscribe" => handle_context_unsubscribe(state, id, req.params),
                m if m == TOOLS_DECLARE_METHOD => handle_tools_declare(state, id, req.params),
                m if m == TOOLS_INVOKE_METHOD => handle_tools_invoke(state, id, req.params),
                m if m == PLUGINS_LIST_METHOD => handle_plugins_list(state, id),
                other => {
                    serde_json::to_value(Response::err(id, ErrorObject::method_not_found(other)))
                        .expect("Response serializes")
                }
            }
        }
        Message::Response(resp) => {
            // Reply to a host-initiated call (e.g. tools/call we sent).
            // Route to the matching oneshot in `state.pending`.
            if let Some(id) = resp.id.as_u64() {
                let tx = state.pending.lock().unwrap().remove(&id);
                if let Some(tx) = tx {
                    let _ = tx.send(resp);
                }
            }
            json!(null)
        }
        // Notifications have no id — don't respond.
        Message::Notification(_) => json!(null),
    }
}

/// Methods the plugin host implements. Plugins announce their required and
/// optional method dependencies in `orca/hello`; we use this set to decide
/// whether the connection is `full`, `degraded`, or `rejected`.
pub const SUPPORTED_METHODS: &[&str] = &[
    "orca/hello",
    "orca/types.declare",
    "orca/context.publish",
    "orca/context.subscribe",
    "orca/context.unsubscribe",
    TOOLS_DECLARE_METHOD,
    TOOLS_CALL_METHOD,
    TOOLS_INVOKE_METHOD,
    PLUGINS_LIST_METHOD,
];

const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

fn handle_hello(
    state: &mut ConnState,
    id: serde_json::Value,
    params: Option<serde_json::Value>,
) -> serde_json::Value {
    let params: HelloParams = match params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params("orca/hello requires params"),
            ))
            .expect("Response serializes");
        }
    };

    info!(
        "[plugin-host] hello from plugin '{}' (sdk {}, flavor {:?})",
        params.plugin_id, params.sdk_version, params.flavor
    );

    let supported: Vec<String> = SUPPORTED_METHODS.iter().map(|s| s.to_string()).collect();
    let reject = |reason: String| HelloResult {
        server_version: SERVER_VERSION.to_string(),
        ok: false,
        status: "rejected".into(),
        methods: supported.clone(),
        reason: Some(reason),
    };

    // 0. Identity gate. The plugin_id claimed in hello must match the CN
    // of the cert this connection presented. Otherwise any plugin holding a
    // CA-signed cert could impersonate any other plugin's id.
    if params.plugin_id != state.peer_cn {
        warn!(
            "[plugin-host] hello rejected: claimed plugin_id '{}' != peer cert CN '{}'",
            params.plugin_id, state.peer_cn
        );
        let result = reject(format!(
            "plugin_id '{}' does not match peer cert CN '{}'",
            params.plugin_id, state.peer_cn
        ));
        let value = serde_json::to_value(&result).expect("HelloResult serializes");
        return serde_json::to_value(Response::ok(id, value)).expect("Response serializes");
    }
    state.plugin_id = Some(params.plugin_id.clone());

    // Make this connection reachable to external code (MCP bridge, etc.)
    // by registering a handle the registry hands out by plugin_id.
    state.plugins.register(ConnHandle {
        plugin_id: params.plugin_id.clone(),
        plugin_version: params.plugin_version.clone(),
        outbound: state.notify_tx.clone(),
        pending: state.pending.clone(),
        next_id: state.next_outbound_id.clone(),
    });

    // 1. Version gate.
    let result = match compare_semver(SERVER_VERSION, &params.core_min_required) {
        Err(e) => reject(format!(
            "invalid core_min_required '{}': {e}",
            params.core_min_required
        )),
        Ok(Ordering::Less) => reject(format!(
            "server {SERVER_VERSION} < core_min_required {}",
            params.core_min_required
        )),
        Ok(_) => {
            // 2. Required methods must all be supported.
            let missing_required: Vec<String> = params
                .methods_required
                .iter()
                .filter(|m| !SUPPORTED_METHODS.contains(&m.as_str()))
                .cloned()
                .collect();
            if !missing_required.is_empty() {
                reject(format!("missing required methods: {missing_required:?}"))
            } else {
                // 3. Optional methods missing → degraded.
                let missing_optional: Vec<String> = params
                    .methods_optional
                    .iter()
                    .filter(|m| !SUPPORTED_METHODS.contains(&m.as_str()))
                    .cloned()
                    .collect();
                // 4. Peer plugin dependencies. Required misses → reject.
                // Optional misses → degraded with reason.
                let connected = state.plugins.connected_peers();
                let missing_required_plugins =
                    unsatisfied_plugin_deps(&params.plugins_required, &connected);
                let missing_optional_plugins =
                    unsatisfied_plugin_deps(&params.plugins_optional, &connected);

                if !missing_required_plugins.is_empty() {
                    reject(format!(
                        "missing required plugin deps: {missing_required_plugins:?}"
                    ))
                } else if missing_optional.is_empty() && missing_optional_plugins.is_empty() {
                    HelloResult {
                        server_version: SERVER_VERSION.to_string(),
                        ok: true,
                        status: "full".into(),
                        methods: supported,
                        reason: None,
                    }
                } else {
                    let mut reasons = Vec::new();
                    if !missing_optional.is_empty() {
                        reasons.push(format!(
                            "optional methods unavailable: {missing_optional:?}"
                        ));
                    }
                    if !missing_optional_plugins.is_empty() {
                        reasons.push(format!(
                            "optional plugin deps unavailable: {missing_optional_plugins:?}"
                        ));
                    }
                    HelloResult {
                        server_version: SERVER_VERSION.to_string(),
                        ok: true,
                        status: "degraded".into(),
                        methods: supported,
                        reason: Some(reasons.join("; ")),
                    }
                }
            }
        }
    };

    let value = serde_json::to_value(&result).expect("HelloResult serializes");
    serde_json::to_value(Response::ok(id, value)).expect("Response serializes")
}

/// Parse a single plugin-dep entry. Accepts `"id"` or `"id>=min_version"`.
/// Returns `(id, optional_min_version)`. Min-version is parsed but not
/// validated as semver here — `compare_semver` does that during evaluation.
fn parse_dep_entry(entry: &str) -> (String, Option<String>) {
    if let Some((id, min)) = entry.split_once(">=") {
        (id.trim().to_string(), Some(min.trim().to_string()))
    } else {
        (entry.trim().to_string(), None)
    }
}

/// Returns the entries from `wanted` that are NOT satisfied by `connected`.
/// An entry is satisfied when a connected peer with the matching id exists
/// AND its declared version is `>=` any min-version constraint.
fn unsatisfied_plugin_deps(wanted: &[String], connected: &[PeerInfo]) -> Vec<String> {
    let mut unsatisfied = Vec::new();
    for entry in wanted {
        let (want_id, want_min) = parse_dep_entry(entry);
        let peer = match connected.iter().find(|p| p.id == want_id) {
            Some(p) => p,
            None => {
                unsatisfied.push(entry.clone());
                continue;
            }
        };
        if let Some(min) = want_min {
            // Empty version on the peer side means "unknown" — accept it
            // rather than failing closed; legacy SDK clients don't announce
            // a version. Once everyone announces, callers can tighten this.
            if peer.version.is_empty() {
                continue;
            }
            match compare_semver(&peer.version, &min) {
                Ok(Ordering::Less) => {
                    unsatisfied.push(format!("{}>={} (have {})", want_id, min, peer.version))
                }
                Ok(_) => {}
                Err(_) => unsatisfied.push(format!(
                    "{}>={} (peer has unparseable version '{}')",
                    want_id, min, peer.version
                )),
            }
        }
    }
    unsatisfied
}

fn handle_types_declare(
    state: &ConnState,
    id: serde_json::Value,
    params: Option<serde_json::Value>,
) -> serde_json::Value {
    let plugin_id = match state.plugin_id.as_deref() {
        Some(p) => p,
        None => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params("orca/types.declare requires prior orca/hello"),
            ))
            .expect("Response serializes");
        }
    };

    let params: TypesDeclareParams = match params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params("orca/types.declare requires { types: [...] }"),
            ))
            .expect("Response serializes");
        }
    };

    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::internal(&format!("open db: {e}")),
            ))
            .expect("Response serializes");
        }
    };

    let mut accepted = Vec::with_capacity(params.types.len());
    for decl in &params.types {
        if decl.type_name.is_empty() {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params("type_name must not be empty"),
            ))
            .expect("Response serializes");
        }
        let schema_str = match serde_json::to_string(&decl.schema) {
            Ok(s) => s,
            Err(e) => {
                return serde_json::to_value(Response::err(
                    id,
                    ErrorObject::invalid_params(&format!(
                        "schema for '{}' is not serializable: {e}",
                        decl.type_name
                    )),
                ))
                .expect("Response serializes");
            }
        };
        if let Err(e) = db::plugin_types::upsert(
            &conn,
            plugin_id,
            &decl.type_name,
            &decl.schema_version,
            &schema_str,
            decl.sensitivity.as_str(),
        ) {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::internal(&format!(
                    "upsert plugin_type {plugin_id}.{}: {e}",
                    decl.type_name
                )),
            ))
            .expect("Response serializes");
        }
        accepted.push(format!("{plugin_id}.{}", decl.type_name));
    }

    info!(
        "[plugin-host] types.declare from '{plugin_id}' accepted {} type(s)",
        accepted.len()
    );

    let result = TypesDeclareResult { accepted };
    let value = serde_json::to_value(&result).expect("TypesDeclareResult serializes");
    serde_json::to_value(Response::ok(id, value)).expect("Response serializes")
}

// ── tools.declare handler ─────────────────────────────────────────────────────

fn handle_tools_declare(
    state: &ConnState,
    id: serde_json::Value,
    params: Option<serde_json::Value>,
) -> serde_json::Value {
    let plugin_id = match state.plugin_id.as_deref() {
        Some(p) => p,
        None => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params(&format!(
                    "{TOOLS_DECLARE_METHOD} requires prior orca/hello"
                )),
            ))
            .expect("Response serializes");
        }
    };

    let params: ToolsDeclareParams = match params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params(&format!(
                    "{TOOLS_DECLARE_METHOD} requires {{ tools: [...] }}"
                )),
            ))
            .expect("Response serializes");
        }
    };

    // Per-tool validation. Empty name or unserializable schema is an
    // invalid_params error; the host won't accept a partial set.
    let mut rows: Vec<(String, String, String, String)> = Vec::with_capacity(params.tools.len());
    let mut accepted = Vec::with_capacity(params.tools.len());
    for decl in &params.tools {
        if decl.name.trim().is_empty() {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params("tool.name must not be empty"),
            ))
            .expect("Response serializes");
        }
        let schema_str = match serde_json::to_string(&decl.input_schema) {
            Ok(s) => s,
            Err(e) => {
                return serde_json::to_value(Response::err(
                    id,
                    ErrorObject::invalid_params(&format!(
                        "input_schema for '{}' is not serializable: {e}",
                        decl.name
                    )),
                ))
                .expect("Response serializes");
            }
        };
        rows.push((
            decl.name.clone(),
            decl.description.clone(),
            schema_str,
            decl.sensitivity.as_str().to_string(),
        ));
        accepted.push(format!("{plugin_id}.{}", decl.name));
    }

    let mut conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::internal(&format!("open db: {e}")),
            ))
            .expect("Response serializes");
        }
    };
    if let Err(e) = db::plugin_tools::replace(&mut conn, plugin_id, &rows) {
        return serde_json::to_value(Response::err(
            id,
            ErrorObject::internal(&format!("replace_plugin_tools {plugin_id}: {e}")),
        ))
        .expect("Response serializes");
    }

    info!(
        "[plugin-host] tools.declare from '{plugin_id}' accepted {} tool(s)",
        accepted.len()
    );

    let result = ToolsDeclareResult { accepted };
    let value = serde_json::to_value(&result).expect("ToolsDeclareResult serializes");
    serde_json::to_value(Response::ok(id, value)).expect("Response serializes")
}

// ── tools.invoke + plugins.list handlers ──────────────────────────────────────

/// Default per-call timeout when the caller doesn't override it via
/// `ToolInvokeParams.timeout_secs`. Mirrors the value used by the MCP HTTP
/// bridge so direct + indirect routes behave the same.
const DEFAULT_INVOKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Handle `orca/tools.invoke`. The caller plugin asks the host to dispatch
/// a tool to a peer. We resolve the fq_name in DB → look up the owning
/// plugin's `ConnHandle` → forward via `call_tool`. The peer's opaque result
/// is returned verbatim. Errors are surfaced as JSON-RPC errors so the
/// caller can act on them — peer-not-connected, tool-not-declared, etc.
fn handle_tools_invoke(
    state: &ConnState,
    id: serde_json::Value,
    params: Option<serde_json::Value>,
) -> serde_json::Value {
    if let Some(reject) = require_hello(state, &id, TOOLS_INVOKE_METHOD) {
        return reject;
    }
    let caller = state.plugin_id.clone().unwrap_or_default();

    let params: ToolInvokeParams = match params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params(&format!(
                    "{TOOLS_INVOKE_METHOD} requires {{ name, arguments? }}"
                )),
            ))
            .expect("Response serializes");
        }
    };

    let fq_name = params.name.clone();
    // Resolve fq_name → (peer_id, bare_tool_name) via DB. The DB is the
    // source of truth for what each plugin has declared via tools.declare.
    let row = match db::open_default().and_then(|c| db::plugin_tools::get(&c, &fq_name)) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params(&format!(
                    "tool '{fq_name}' is not declared by any connected plugin"
                )),
            ))
            .expect("Response serializes");
        }
        Err(e) => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::internal(&format!("lookup '{fq_name}': {e}")),
            ))
            .expect("Response serializes");
        }
    };

    // A plugin invoking its own tool would loop back through its own
    // tools.call — pointless and easy to mistake. Surface clearly.
    if row.plugin_id == caller {
        return serde_json::to_value(Response::err(
            id,
            ErrorObject::invalid_params(&format!(
                "plugin '{caller}' attempted to invoke its own tool '{fq_name}' via the host; \
                 call the local handler directly"
            )),
        ))
        .expect("Response serializes");
    }

    let peer = match state.plugins.get(&row.plugin_id) {
        Some(p) => p,
        None => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::internal(&format!(
                    "tool '{fq_name}' is declared but plugin '{}' is not currently connected",
                    row.plugin_id
                )),
            ))
            .expect("Response serializes");
        }
    };

    let timeout = params
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_INVOKE_TIMEOUT);
    let arguments = params.arguments;
    let bare_name = row.name.clone();
    let peer_id = row.plugin_id.clone();
    let caller_for_log = caller.clone();

    // Dispatch on a detached task: ConnHandle::call_tool is async and our
    // dispatch path is synchronous (so the read loop can keep moving).
    // The completion writes the response back over the caller's outbound
    // channel using the request id we captured.
    let outbound = state.notify_tx.clone();
    let id_for_task = id.clone();
    tokio::spawn(async move {
        let resp_value = match peer.call_tool(&bare_name, arguments, timeout).await {
            Ok(value) => {
                info!("[plugin-host] '{caller_for_log}' invoked '{peer_id}.{bare_name}' (ok)");
                let result = ToolInvokeResult { result: value };
                let v = serde_json::to_value(&result).expect("ToolInvokeResult serializes");
                serde_json::to_value(Response::ok(id_for_task, v)).expect("Response serializes")
            }
            Err(e) => {
                warn!(
                    "[plugin-host] '{caller_for_log}' invoke '{peer_id}.{bare_name}' failed: {e:#}"
                );
                serde_json::to_value(Response::err(
                    id_for_task,
                    ErrorObject::internal(&format!("invoke '{fq_name}': {e:#}")),
                ))
                .expect("Response serializes")
            }
        };
        let _ = outbound.send(resp_value);
    });

    // Returning null suppresses the normal sync-write path; the spawned
    // task writes the actual response when the peer call resolves.
    json!(null)
}

/// Handle `orca/plugins.list` — return every connected peer plus its
/// declared version. Plugins use this to fail fast on missing optional
/// deps and to discover live peers without polling the registry.
fn handle_plugins_list(state: &ConnState, id: serde_json::Value) -> serde_json::Value {
    if let Some(reject) = require_hello(state, &id, PLUGINS_LIST_METHOD) {
        return reject;
    }
    let result = PluginsListResult {
        peers: state.plugins.connected_peers(),
    };
    let v = serde_json::to_value(&result).expect("PluginsListResult serializes");
    serde_json::to_value(Response::ok(id, v)).expect("Response serializes")
}

// ── context.* handlers ────────────────────────────────────────────────────────

fn require_hello(
    state: &ConnState,
    id: &serde_json::Value,
    method: &str,
) -> Option<serde_json::Value> {
    if state.plugin_id.is_some() {
        None
    } else {
        Some(
            serde_json::to_value(Response::err(
                id.clone(),
                ErrorObject::invalid_params(&format!("{method} requires prior orca/hello")),
            ))
            .expect("Response serializes"),
        )
    }
}

fn handle_context_publish(
    state: &ConnState,
    id: serde_json::Value,
    params: Option<serde_json::Value>,
) -> serde_json::Value {
    if let Some(reject) = require_hello(state, &id, "orca/context.publish") {
        return reject;
    }
    let params: ContextPublishParams = match params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params("orca/context.publish requires { context_id, value }"),
            ))
            .expect("Response serializes");
        }
    };

    // Schema gate: if this type_id has been declared via orca/types.declare,
    // the published payload must conform to the registered JSON Schema.
    // Undeclared type_ids are allowed through — declaration is currently
    // opt-in. Once meerkat plugins land, strict-mode (declared-or-reject)
    // can be turned on.
    if let Err(reject) = validate_against_declared_schema(&id, &params.value) {
        return reject;
    }

    let tx = state.registry.channel(&params.context_id);
    // Ignore SendError when no current subscribers — message is just dropped.
    let _ = tx.send((params.context_id, params.value));
    serde_json::to_value(Response::ok(id, json!({ "ok": true }))).expect("Response serializes")
}

fn handle_context_subscribe(
    state: &mut ConnState,
    id: serde_json::Value,
    params: Option<serde_json::Value>,
) -> serde_json::Value {
    if let Some(reject) = require_hello(state, &id, "orca/context.subscribe") {
        return reject;
    }
    let params: ContextSubscribeParams = match params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params(
                    "orca/context.subscribe requires { context_id, type_filter? }",
                ),
            ))
            .expect("Response serializes");
        }
    };

    let subscription_id = uuid::Uuid::new_v4().to_string();
    let mut rx = state.registry.channel(&params.context_id).subscribe();
    let notify_tx = state.notify_tx.clone();
    let filter = params.type_filter;
    let sub_id_for_task = subscription_id.clone();

    let handle = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok((ctx_id, value)) => {
                    if !filter.is_empty() && !filter.iter().any(|t| t == &value.type_id) {
                        continue;
                    }
                    let event = ContextEvent {
                        subscription_id: sub_id_for_task.clone(),
                        context_id: ctx_id,
                        value,
                    };
                    let notif = json!({
                        "jsonrpc": "2.0",
                        "method": CONTEXT_EVENT_METHOD,
                        "params": event,
                    });
                    if notify_tx.send(notif).is_err() {
                        break; // connection closed
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    state.subscriptions.insert(subscription_id.clone(), handle);
    let result = ContextSubscribeResult { subscription_id };
    let value = serde_json::to_value(&result).expect("ContextSubscribeResult serializes");
    serde_json::to_value(Response::ok(id, value)).expect("Response serializes")
}

fn handle_context_unsubscribe(
    state: &mut ConnState,
    id: serde_json::Value,
    params: Option<serde_json::Value>,
) -> serde_json::Value {
    if let Some(reject) = require_hello(state, &id, "orca/context.unsubscribe") {
        return reject;
    }
    let params: ContextUnsubscribeParams = match params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params(
                    "orca/context.unsubscribe requires { subscription_id }",
                ),
            ))
            .expect("Response serializes");
        }
    };
    match state.subscriptions.remove(&params.subscription_id) {
        Some(handle) => {
            handle.abort();
            serde_json::to_value(Response::ok(id, json!({ "ok": true })))
                .expect("Response serializes")
        }
        None => serde_json::to_value(Response::err(
            id,
            ErrorObject::invalid_params(&format!(
                "unknown subscription_id '{}'",
                params.subscription_id
            )),
        ))
        .expect("Response serializes"),
    }
}

/// Look up the declared JSON Schema for `value.type_id` and validate the
/// payload against it. Returns:
///   * `Ok(())` if no declaration exists, or the payload conforms.
///   * `Err(response_value)` — a fully-built JSON-RPC error response — if a
///     declaration exists and validation fails, or if the DB or schema itself
///     can't be loaded.
fn validate_against_declared_schema(
    id: &serde_json::Value,
    value: &TypedValue,
) -> std::result::Result<(), serde_json::Value> {
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => {
            return Err(serde_json::to_value(Response::err(
                id.clone(),
                ErrorObject::internal(&format!("open db: {e}")),
            ))
            .expect("Response serializes"));
        }
    };

    let row = match db::plugin_types::get(&conn, &value.type_id) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()), // undeclared → allowed
        Err(e) => {
            return Err(serde_json::to_value(Response::err(
                id.clone(),
                ErrorObject::internal(&format!("lookup plugin_type {}: {e}", value.type_id)),
            ))
            .expect("Response serializes"));
        }
    };

    let schema: serde_json::Value = match serde_json::from_str(&row.schema_json) {
        Ok(s) => s,
        Err(e) => {
            return Err(serde_json::to_value(Response::err(
                id.clone(),
                ErrorObject::internal(&format!(
                    "stored schema for '{}' is not valid JSON: {e}",
                    value.type_id
                )),
            ))
            .expect("Response serializes"));
        }
    };

    let validator = match jsonschema::validator_for(&schema) {
        Ok(v) => v,
        Err(e) => {
            return Err(serde_json::to_value(Response::err(
                id.clone(),
                ErrorObject::internal(&format!(
                    "stored schema for '{}' is not a valid JSON Schema: {e}",
                    value.type_id
                )),
            ))
            .expect("Response serializes"));
        }
    };

    if let Err(err) = validator.validate(&value.payload) {
        let reason = format!(
            "payload for '{}' failed schema validation: {} at {}",
            value.type_id,
            err,
            err.instance_path()
        );
        return Err(serde_json::to_value(Response::err(
            id.clone(),
            ErrorObject::invalid_params(&reason),
        ))
        .expect("Response serializes"));
    }

    Ok(())
}

/// Compare two dotted-numeric versions (e.g. "0.1.0" vs "0.2.0").
/// Pre-release / build metadata segments are stripped before comparison —
/// the server's CARGO_PKG_VERSION often carries an "-rc.N" suffix between
/// stable cuts, but for handshake purposes "0.0.3-rc.3" satisfies any plugin
/// asking for "0.0.3" or below.
fn compare_semver(a: &str, b: &str) -> Result<Ordering> {
    fn parse(v: &str) -> Result<Vec<u64>> {
        // Drop "-prerelease" and "+build" suffixes — we only compare the
        // numeric core, which is enough for a "min required" gate.
        let core = v.split(['-', '+']).next().unwrap_or(v);
        core.split('.')
            .map(|p| {
                p.parse::<u64>()
                    .map_err(|e| anyhow::anyhow!("bad component '{p}': {e}"))
            })
            .collect()
    }
    let av = parse(a)?;
    let bv = parse(b)?;
    let len = av.len().max(bv.len());
    for i in 0..len {
        let l = av.get(i).copied().unwrap_or(0);
        let r = bv.get(i).copied().unwrap_or(0);
        match l.cmp(&r) {
            Ordering::Equal => continue,
            other => return Ok(other),
        }
    }
    Ok(Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_compare() {
        assert_eq!(compare_semver("0.1.0", "0.1.0").unwrap(), Ordering::Equal);
        assert_eq!(compare_semver("0.2.0", "0.1.9").unwrap(), Ordering::Greater);
        assert_eq!(compare_semver("0.1.0", "0.1.1").unwrap(), Ordering::Less);
        assert_eq!(compare_semver("1.0", "1.0.0").unwrap(), Ordering::Equal);
        assert_eq!(
            compare_semver("2.0.0", "1.99.99").unwrap(),
            Ordering::Greater
        );
        // Prerelease/build metadata is stripped, not rejected — the numeric
        // core wins. "0.1.0-rc1" compares equal to "0.1.0".
        assert_eq!(
            compare_semver("0.1.0-rc1", "0.1.0").unwrap(),
            Ordering::Equal
        );
        assert!(compare_semver("not-a-version", "0.1.0").is_err());
    }
}
