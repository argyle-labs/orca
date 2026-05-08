//! TCP + mTLS plugin host — accepts connections from local and remote plugins.
//!
//! Listens on `0.0.0.0:<APP_PLUGIN_PORT>` (default 12002). Requires client
//! certificates signed by the orca CA. Dispatches JSON-RPC 2.0 frames.
//!
//! Phase A methods:
//!   orca/hello  — version + capability handshake. Returns `full` when all
//!                 required and optional methods are supported, `degraded`
//!                 when only optional methods are missing, and `rejected`
//!                 when the server version is below `core_min_required` or
//!                 a required method is unavailable.

use anyhow::{Context, Result};
use orca_sdk::framing::{read_frame, write_frame};
use orca_sdk::jsonrpc::{ErrorObject, Message, Response};
use orca_sdk::pki;
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
use std::sync::{Arc, Mutex as StdMutex};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
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

/// Start the plugin host in a background task. Returns immediately.
/// If the PKI directory doesn't contain a CA, logs a warning and skips the host.
pub fn start(pki_dir: &Path, port: u16) -> tokio::task::JoinHandle<()> {
    let pki_dir = pki_dir.to_owned();
    tokio::spawn(async move {
        if let Err(e) = run(&pki_dir, port).await {
            warn!("[plugin-host] failed to start: {e:#}");
        }
    })
}

async fn run(pki_dir: &Path, port: u16) -> Result<()> {
    let (listener, acceptor, addr) = bind(pki_dir, port).await?;
    info!("[plugin-host] listening on {addr} (mTLS)");
    serve(listener, acceptor, ContextRegistry::new()).await
}

/// Bind a TCP listener and build the mTLS acceptor for the plugin host.
/// Returns the listener, acceptor, and the actual bound address (useful when
/// `port == 0` lets the OS pick an ephemeral port).
pub async fn bind(pki_dir: &Path, port: u16) -> Result<(TcpListener, TlsAcceptor, SocketAddr)> {
    let server_bundle = pki::load_server(pki_dir).context("load server TLS bundle")?;
    let acceptor = build_acceptor(&server_bundle)?;

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
) -> Result<()> {
    loop {
        match listener.accept().await {
            Ok((tcp, peer)) => {
                let acceptor = acceptor.clone();
                let registry = registry.clone();
                tokio::spawn(async move {
                    match acceptor.accept(tcp).await {
                        Ok(tls) => {
                            if let Err(e) = handle_connection(tls, registry).await {
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

fn build_acceptor(bundle: &pki::NodeBundle) -> Result<TlsAcceptor> {
    let (cert_chain, private_key) = pki::parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem)?;
    let ca_root_store = Arc::new(pki::ca_root_store(&bundle.ca_cert_pem)?);

    let client_cert_verifier = WebPkiClientVerifier::builder(ca_root_store)
        .build()
        .context("build client cert verifier")?;

    let server_config = ServerConfig::builder()
        .with_client_cert_verifier(client_cert_verifier)
        .with_single_cert(cert_chain, private_key)
        .context("build server TLS config")?;

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

/// Per-connection state carried across frames. Set by `orca/hello`; read by
/// every subsequent method that needs the plugin's identity.
struct ConnState {
    plugin_id: Option<String>,
    registry: ContextRegistry,
    /// Notifications produced by handlers (and by subscription pumps) that
    /// the writer half of the connection should serialize and send.
    notify_tx: mpsc::UnboundedSender<serde_json::Value>,
    /// Active subscriptions; aborting the JoinHandle stops forwarding events.
    subscriptions: HashMap<String, tokio::task::JoinHandle<()>>,
}

impl Drop for ConnState {
    fn drop(&mut self) {
        for (_, handle) in self.subscriptions.drain() {
            handle.abort();
        }
    }
}

async fn handle_connection(
    tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    registry: ContextRegistry,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(tls);
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let mut state = ConnState {
        plugin_id: None,
        registry,
        notify_tx,
        subscriptions: HashMap::new(),
    };

    loop {
        tokio::select! {
            biased;

            // Outgoing notifications (from subscription pumps) — flush first
            // so events don't queue up indefinitely behind a slow client.
            Some(notif) = notify_rx.recv() => {
                let bytes = serde_json::to_vec(&notif)?;
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
                other => {
                    serde_json::to_value(Response::err(id, ErrorObject::method_not_found(other)))
                        .expect("Response serializes")
                }
            }
        }
        // Notifications have no id — don't respond.
        Message::Notification(_) | Message::Response(_) => json!(null),
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
    state.plugin_id = Some(params.plugin_id.clone());

    let supported: Vec<String> = SUPPORTED_METHODS.iter().map(|s| s.to_string()).collect();
    let reject = |reason: String| HelloResult {
        server_version: SERVER_VERSION.to_string(),
        ok: false,
        status: "rejected".into(),
        methods: supported.clone(),
        reason: Some(reason),
    };

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
                if missing_optional.is_empty() {
                    HelloResult {
                        server_version: SERVER_VERSION.to_string(),
                        ok: true,
                        status: "full".into(),
                        methods: supported,
                        reason: None,
                    }
                } else {
                    HelloResult {
                        server_version: SERVER_VERSION.to_string(),
                        ok: true,
                        status: "degraded".into(),
                        methods: supported,
                        reason: Some(format!(
                            "optional methods unavailable: {missing_optional:?}"
                        )),
                    }
                }
            }
        }
    };

    let value = serde_json::to_value(&result).expect("HelloResult serializes");
    serde_json::to_value(Response::ok(id, value)).expect("Response serializes")
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
        if let Err(e) = db::upsert_plugin_type(
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

/// Compare two dotted-numeric versions (e.g. "0.1.0" vs "0.2.0").
/// Pre-release / build metadata segments are not supported — returns Err.
fn compare_semver(a: &str, b: &str) -> Result<Ordering> {
    fn parse(v: &str) -> Result<Vec<u64>> {
        if v.contains('-') || v.contains('+') {
            anyhow::bail!("pre-release/build metadata not supported");
        }
        v.split('.')
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
        assert!(compare_semver("0.1.0-rc1", "0.1.0").is_err());
        assert!(compare_semver("not-a-version", "0.1.0").is_err());
    }
}
