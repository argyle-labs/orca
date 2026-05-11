//! TCP + mTLS transport: plugin (client) side.
//!
//! A `TcpTransport` wraps a mutually-authenticated TLS stream over TCP. A
//! background reader task demultiplexes incoming frames:
//!   - Responses are routed to the matching `call()` future via a per-request
//!     `oneshot` channel.
//!   - Notifications are fanned out on a broadcast channel; callers subscribe
//!     via [`TcpTransport::notifications`] (or higher-level helpers like
//!     [`TcpTransport::subscribe_context`]).
//!
//! Multiple `call()`s may be in flight concurrently — each gets its own id
//! and waits on its own oneshot.

// RPC envelope frames mirror jsonrpc.rs — params/result are opaque Value at
// the TCP transport boundary.
#![allow(clippy::disallowed_types)]

use anyhow::{Context, Result, bail};
use rustls::pki_types::ServerName;
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{
    Arc, Mutex as StdMutex, Weak,
    atomic::{AtomicU64, Ordering},
};
use tokio::io::WriteHalf;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify, broadcast, oneshot};
use tokio_rustls::{TlsConnector, client::TlsStream};

use crate::framing::{read_frame, write_frame};
use crate::jsonrpc::{ErrorObject, Message, Notification, Request, Response};
use crate::pki::NodeBundle;
use crate::tools::{
    RegisteredTool, TOOLS_CALL_METHOD, ToolCallParams, ToolCallResult, ToolDeclaration,
    ToolHandler, ToolsDeclareParams, ToolsDeclareResult, tool_error_codes,
};

// ── Hello protocol ────────────────────────────────────────────────────────────

/// Parameters sent in `orca/hello`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HelloParams {
    pub sdk_version: String,
    pub plugin_id: String,
    /// Plugin's own version, taken from `orca-plugin.toml::plugin.version`.
    /// The host stores this so `orca/plugins.list` can return it to peers
    /// and so `depends_on.min_version` constraints can be evaluated.
    /// Empty string means the plugin didn't announce a version (back-compat
    /// for SDK clients that pre-date this field).
    #[serde(default)]
    pub plugin_version: String,
    pub flavor: crate::Flavor,
    pub core_min_required: String,
    #[serde(default)]
    pub methods_required: Vec<String>,
    #[serde(default)]
    pub methods_optional: Vec<String>,
    /// Required peer plugins, formatted as `"<id>>=<min_version>"` (e.g.
    /// `"graphql>=0.1.0"`). Mirrors `manifest.depends_on` with `optional=false`.
    /// Host rejects/degrades hello when an entry is unsatisfied.
    #[serde(default)]
    pub plugins_required: Vec<String>,
    /// Optional peer plugins. Same format as `plugins_required`. Hello stays
    /// `full` if all are satisfied; missing optional deps shift to `degraded`.
    #[serde(default)]
    pub plugins_optional: Vec<String>,
}

/// Result returned by the server for `orca/hello`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HelloResult {
    pub server_version: String,
    pub ok: bool,
    /// "full" | "degraded" | "rejected"
    pub status: String,
    pub methods: Vec<String>,
    /// Human-readable explanation when status is "rejected" or "degraded".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ── types.declare protocol ────────────────────────────────────────────────────

/// Sensitivity class controlling whether a TypedValue may flow into a `general`
/// context or is restricted to `sensitive`-tier nodes.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    General,
    Sensitive,
}

impl Sensitivity {
    pub fn as_str(self) -> &'static str {
        match self {
            Sensitivity::General => "general",
            Sensitivity::Sensitive => "sensitive",
        }
    }
}

/// One type the plugin is announcing. The fully-qualified id is computed as
/// `<plugin_id>.<type_name>` and must be unique within the plugin.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TypeDeclaration {
    pub type_name: String,
    pub schema_version: String,
    /// JSON Schema document describing the payload shape.
    pub schema: serde_json::Value,
    #[serde(default = "default_sensitivity")]
    pub sensitivity: Sensitivity,
}

fn default_sensitivity() -> Sensitivity {
    Sensitivity::General
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TypesDeclareParams {
    pub types: Vec<TypeDeclaration>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TypesDeclareResult {
    /// Fully-qualified ids the server accepted (`<plugin_id>.<type_name>`).
    pub accepted: Vec<String>,
}

// ── context.* protocol ────────────────────────────────────────────────────────

/// Notification method emitted by the server for context events.
pub const CONTEXT_EVENT_METHOD: &str = "orca/context.event";

/// One TypedValue published into a Context. The plugin host treats `payload`
/// as opaque JSON — type-checking against the registered schema is the
/// publisher's responsibility for now.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TypedValue {
    /// Fully-qualified type id, e.g. `"arr.sonarr.Series"`.
    #[serde(rename = "type")]
    pub type_id: String,
    pub schema_version: String,
    #[serde(default = "default_sensitivity")]
    pub sensitivity: Sensitivity,
    pub payload: Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextPublishParams {
    pub context_id: String,
    pub value: TypedValue,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextSubscribeParams {
    pub context_id: String,
    /// Optional filter — only forward TypedValues whose `type_id` is in this
    /// list. Empty = no filter (all types).
    #[serde(default)]
    pub type_filter: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextSubscribeResult {
    pub subscription_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextUnsubscribeParams {
    pub subscription_id: String,
}

/// Notification payload pushed by the server when a TypedValue is published
/// into a context the plugin is subscribed to. Method: `"orca/context.event"`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextEvent {
    pub subscription_id: String,
    pub context_id: String,
    pub value: TypedValue,
}

// ── Demultiplexer state ───────────────────────────────────────────────────────

/// Capacity of the notifications broadcast channel. Slow subscribers that
/// fall behind by more than this many messages will see `Lagged` errors and
/// can resubscribe.
const NOTIFICATIONS_CAPACITY: usize = 256;

struct Demux {
    pending: StdMutex<HashMap<u64, oneshot::Sender<Response>>>,
    notifications: broadcast::Sender<Notification>,
    /// Tools the plugin has registered for the host to invoke. Keyed by
    /// the bare tool name (no `<plugin_id>.` prefix).
    tools: StdMutex<HashMap<String, RegisteredTool>>,
}

impl Demux {
    fn new() -> Arc<Self> {
        let (notifications, _) = broadcast::channel(NOTIFICATIONS_CAPACITY);
        Arc::new(Self {
            pending: StdMutex::new(HashMap::new()),
            notifications,
            tools: StdMutex::new(HashMap::new()),
        })
    }
}

/// Type alias for the writer half. Kept behind `Arc<Mutex<_>>` so the
/// reader task can write responses to incoming `tools/call` requests
/// concurrently with caller-initiated `call()`s.
type SharedWriter = Arc<Mutex<WriteHalf<TlsStream<TcpStream>>>>;

// ── TcpTransport ──────────────────────────────────────────────────────────────

/// A connected, mTLS-authenticated plugin transport.
pub struct TcpTransport {
    writer: SharedWriter,
    demux: Arc<Demux>,
    next_id: AtomicU64,
    /// Cancellation signal for the background reader task. Notified from
    /// [`TcpTransport::drop`] (and explicit [`shutdown`](Self::shutdown))
    /// so the reader unblocks from `read_frame` even when the peer is idle.
    /// Without this, a dropped transport would leave the TCP connection open
    /// until the peer happens to send something.
    shutdown: Arc<Notify>,
}

impl Drop for TcpTransport {
    fn drop(&mut self) {
        // Reader task picks this up via the select! arm and breaks the loop,
        // dropping the read half and closing the connection. notify_waiters
        // (not notify_one) is used so any future-reader that hasn't yet
        // entered notified() also sees the signal.
        self.shutdown.notify_waiters();
    }
}

impl TcpTransport {
    /// Connect to an orca plugin host at `addr` using the supplied node bundle.
    ///
    /// Verifies the server cert against the CA cert in `bundle.ca_cert_pem`.
    /// Presents `bundle.cert_pem` / `bundle.key_pem` as the client identity.
    /// Spawns a background reader task that lives until the stream closes.
    pub async fn connect(addr: SocketAddr, bundle: &NodeBundle) -> Result<Arc<Self>> {
        let (cert_chain, private_key) =
            crate::pki::parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem)?;
        let root_store = crate::pki::ca_root_store(&bundle.ca_cert_pem)?;

        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_client_auth_cert(cert_chain, private_key)
            .context("build rustls client config")?;

        let connector = TlsConnector::from(Arc::new(client_config));
        let tcp = TcpStream::connect(addr)
            .await
            .with_context(|| format!("TCP connect to {addr}"))?;

        let server_name: ServerName<'static> =
            ServerName::try_from("core.orca.local").expect("valid DNS name");
        let tls = connector
            .connect(server_name, tcp)
            .await
            .context("TLS handshake")?;

        let (mut reader, writer) = tokio::io::split(tls);
        let writer: SharedWriter = Arc::new(Mutex::new(writer));
        let demux = Demux::new();
        let demux_for_task = demux.clone();
        // Weak so the reader task does NOT keep the connection alive after
        // the caller drops the TcpTransport. When the last strong ref drops,
        // upgrade() returns None, the loop breaks, the read half drops,
        // and the OS closes the TCP connection.
        let writer_for_task: Weak<Mutex<WriteHalf<TlsStream<TcpStream>>>> = Arc::downgrade(&writer);
        let shutdown = Arc::new(Notify::new());
        let shutdown_for_task = shutdown.clone();
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    biased;
                    _ = shutdown_for_task.notified() => break,
                    res = read_frame(&mut reader) => match res {
                        Ok(f) => f,
                        Err(_) => break, // EOF or transport error — end the task.
                    },
                };
                let msg: Message = match serde_json::from_slice(&frame) {
                    Ok(m) => m,
                    Err(_) => continue, // malformed frame; skip
                };
                match msg {
                    Message::Response(r) => {
                        if let Some(id) = r.id.as_u64() {
                            let tx = demux_for_task.pending.lock().unwrap().remove(&id);
                            if let Some(tx) = tx {
                                let _ = tx.send(r);
                            }
                        }
                    }
                    Message::Notification(n) => {
                        // ignore the SendError when nobody is subscribed
                        let _ = demux_for_task.notifications.send(n);
                    }
                    Message::Request(req) => {
                        // Spawn a per-request task so a slow handler doesn't
                        // stall the read loop. Upgrade the Weak — if the
                        // caller has dropped the transport, skip writing.
                        let demux_for_req = demux_for_task.clone();
                        let Some(writer_for_req) = writer_for_task.upgrade() else {
                            break;
                        };
                        tokio::spawn(async move {
                            let resp = dispatch_incoming(&demux_for_req, req).await;
                            let bytes = match serde_json::to_vec(&resp) {
                                Ok(b) => b,
                                Err(_) => return,
                            };
                            let _ = write_frame(&mut *writer_for_req.lock().await, &bytes).await;
                        });
                    }
                }
            }
            // Stream closed: drop pending senders so awaiting calls fail
            // with `oneshot::Canceled` rather than hanging forever.
            demux_for_task.pending.lock().unwrap().clear();
        });

        Ok(Arc::new(Self {
            writer,
            demux,
            next_id: AtomicU64::new(1),
            shutdown,
        }))
    }

    /// Explicit shutdown — equivalent to dropping the transport. Lets callers
    /// close the connection deterministically without waiting for the last
    /// `Arc<Self>` strong reference to be released.
    pub fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }

    // ── Low-level primitives ──────────────────────────────────────────────────

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Send a request and wait for the matching response. Multiple calls may
    /// be in flight concurrently — each gets its own id.
    pub async fn call(&self, method: &str, params: Option<Value>) -> Result<Response> {
        let id = self.alloc_id();
        let req = Request::new(id, method, params);
        let json = serde_json::to_vec(&req)?;

        let (tx, rx) = oneshot::channel();
        self.demux.pending.lock().unwrap().insert(id, tx);

        // Send the frame. If write fails we have to clean up the pending entry.
        if let Err(e) = write_frame(&mut *self.writer.lock().await, &json).await {
            self.demux.pending.lock().unwrap().remove(&id);
            return Err(e);
        }

        rx.await.context("transport closed before response arrived")
    }

    /// Send a notification (fire-and-forget, no response expected).
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let notif = Notification::new(method, params);
        let json = serde_json::to_vec(&notif)?;
        write_frame(&mut *self.writer.lock().await, &json).await
    }

    /// Subscribe to *all* server-pushed notifications on this connection.
    /// Higher-level helpers (e.g. [`subscribe_context`](Self::subscribe_context))
    /// route to specific subscriptions; raw subscribers should filter by
    /// `notification.method`.
    pub fn notifications(&self) -> broadcast::Receiver<Notification> {
        self.demux.notifications.subscribe()
    }

    /// Publish a TypedValue into a named context. The server fans the value
    /// out to every plugin subscribed to that context.
    pub async fn publish_context(
        &self,
        context_id: impl Into<String>,
        value: TypedValue,
    ) -> Result<()> {
        let params = ContextPublishParams {
            context_id: context_id.into(),
            value,
        };
        let resp = self
            .call("orca/context.publish", Some(serde_json::to_value(params)?))
            .await?;
        if resp.is_error() {
            let msg = resp
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown error");
            bail!("orca/context.publish rejected: {msg}");
        }
        Ok(())
    }

    /// Subscribe to a context. Returns the server-allocated `subscription_id`
    /// and a channel of `ContextEvent`s addressed to that subscription.
    ///
    /// Drop the receiver and call [`unsubscribe_context`](Self::unsubscribe_context)
    /// to stop receiving events.
    pub async fn subscribe_context(
        self: &Arc<Self>,
        context_id: impl Into<String>,
        type_filter: Vec<String>,
    ) -> Result<(String, tokio::sync::mpsc::Receiver<ContextEvent>)> {
        let params = ContextSubscribeParams {
            context_id: context_id.into(),
            type_filter,
        };
        let resp = self
            .call(
                "orca/context.subscribe",
                Some(serde_json::to_value(params)?),
            )
            .await?;
        if resp.is_error() {
            let msg = resp
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown error");
            bail!("orca/context.subscribe rejected: {msg}");
        }
        let result: ContextSubscribeResult = serde_json::from_value(
            resp.result
                .context("orca/context.subscribe returned null")?,
        )?;
        let sub_id = result.subscription_id.clone();

        // Pump notifications matching this subscription_id into a dedicated
        // channel so callers don't need to filter manually.
        let (tx, rx) = tokio::sync::mpsc::channel::<ContextEvent>(64);
        let mut notif_rx = self.notifications();
        let want_id = sub_id.clone();
        tokio::spawn(async move {
            loop {
                match notif_rx.recv().await {
                    Ok(n) if n.method == CONTEXT_EVENT_METHOD => {
                        let event: ContextEvent =
                            match n.params.and_then(|p| serde_json::from_value(p).ok()) {
                                Some(e) => e,
                                None => continue,
                            };
                        if event.subscription_id != want_id {
                            continue;
                        }
                        if tx.send(event).await.is_err() {
                            break; // receiver dropped
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        Ok((sub_id, rx))
    }

    /// Cancel a previous subscription. After this returns, the server stops
    /// forwarding events for `subscription_id`.
    pub async fn unsubscribe_context(&self, subscription_id: impl Into<String>) -> Result<()> {
        let params = ContextUnsubscribeParams {
            subscription_id: subscription_id.into(),
        };
        let resp = self
            .call(
                "orca/context.unsubscribe",
                Some(serde_json::to_value(params)?),
            )
            .await?;
        if resp.is_error() {
            let msg = resp
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown error");
            bail!("orca/context.unsubscribe rejected: {msg}");
        }
        Ok(())
    }

    // ── Tools surface ─────────────────────────────────────────────────────────

    /// Register a tool the host can invoke via `orca/tools.call`. Bare name
    /// (no `<plugin_id>.` prefix — the host applies the namespace at
    /// registration time). Re-registering the same name replaces the
    /// previous handler.
    ///
    /// Call this for each tool, then call [`declare_tools`](Self::declare_tools)
    /// once to send the batch to the host.
    pub fn register_tool(
        &self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        sensitivity: Sensitivity,
        handler: Arc<dyn ToolHandler>,
    ) {
        let name = name.into();
        let declaration = ToolDeclaration {
            name: name.clone(),
            description: description.into(),
            input_schema,
            sensitivity,
        };
        self.demux.tools.lock().unwrap().insert(
            name,
            RegisteredTool {
                declaration,
                handler,
            },
        );
    }

    /// Send the registered tool set to the host via `orca/tools.declare`.
    /// Returns the namespaced ids the host accepted (`<plugin_id>.<name>`).
    /// Idempotent — calling again replaces the host-side set.
    pub async fn declare_tools(&self) -> Result<ToolsDeclareResult> {
        let tools: Vec<ToolDeclaration> = self
            .demux
            .tools
            .lock()
            .unwrap()
            .values()
            .map(|t| t.declaration.clone())
            .collect();
        let params = ToolsDeclareParams { tools };
        let resp = self
            .call(
                crate::tools::TOOLS_DECLARE_METHOD,
                Some(serde_json::to_value(params)?),
            )
            .await?;
        if resp.is_error() {
            let msg = resp
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown error");
            bail!("orca/tools.declare rejected: {msg}");
        }
        let result: ToolsDeclareResult =
            serde_json::from_value(resp.result.context("orca/tools.declare returned null")?)?;
        Ok(result)
    }

    /// Declare TypedValue types this plugin produces. Sent at startup, after
    /// `orca/hello`. Returns the list of accepted type ids; errors if any
    /// declaration was rejected (e.g. invalid sensitivity).
    pub async fn declare_types(&self, types: Vec<TypeDeclaration>) -> Result<TypesDeclareResult> {
        let params = TypesDeclareParams { types };
        let resp = self
            .call("orca/types.declare", Some(serde_json::to_value(params)?))
            .await?;
        if resp.is_error() {
            let msg = resp
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown error");
            bail!("orca/types.declare rejected: {msg}");
        }
        let result: TypesDeclareResult =
            serde_json::from_value(resp.result.context("orca/types.declare returned null")?)?;
        Ok(result)
    }

    /// Perform the `orca/hello` handshake. Call this immediately after connecting.
    pub async fn hello(
        &self,
        plugin_id: &str,
        flavor: crate::Flavor,
        methods_required: Vec<String>,
        methods_optional: Vec<String>,
    ) -> Result<HelloResult> {
        self.hello_full(
            HelloOptions::new(plugin_id, flavor)
                .with_required_methods(methods_required)
                .with_optional_methods(methods_optional),
        )
        .await
    }

    /// Full hello with peer dependency declarations and own version. Use
    /// this when porting an `orca-plugin.toml` straight through — the
    /// manifest's `version` and `depends_on` map onto the new fields.
    pub async fn hello_full(&self, opts: HelloOptions) -> Result<HelloResult> {
        let params = HelloParams {
            sdk_version: crate::SDK_VERSION.to_string(),
            plugin_id: opts.plugin_id.clone(),
            plugin_version: opts.plugin_version.clone(),
            flavor: opts.flavor,
            core_min_required: opts.core_min_required.clone(),
            methods_required: opts.methods_required.clone(),
            methods_optional: opts.methods_optional.clone(),
            plugins_required: opts.plugins_required.clone(),
            plugins_optional: opts.plugins_optional.clone(),
        };

        let resp = self
            .call("orca/hello", Some(serde_json::to_value(params)?))
            .await?;

        if resp.is_error() {
            let msg = resp
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown error");
            bail!("orca/hello rejected: {msg}");
        }

        let result: HelloResult =
            serde_json::from_value(resp.result.context("orca/hello returned null result")?)?;

        if !result.ok {
            let reason = result.reason.as_deref().unwrap_or("no reason given");
            bail!(
                "orca/hello: server returned ok=false (status: {}; {reason})",
                result.status
            );
        }

        Ok(result)
    }

    /// Forward a tool call to a peer plugin via the host. `name` is a
    /// fully-qualified peer tool, e.g. `"graphql.query"`. The host resolves
    /// the owning plugin, dispatches via its in-process registry, and
    /// returns the peer's opaque result. `timeout` is the per-call deadline
    /// the plugin enforces locally; the host may apply a separate budget.
    ///
    /// Errors:
    ///   - peer not connected → JSON-RPC error from the host
    ///   - peer returned a tool error → propagated as `anyhow!`
    ///   - timeout → bubbles `oneshot` cancellation
    pub async fn invoke_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value> {
        let params = crate::tools::ToolInvokeParams {
            name: name.to_string(),
            arguments,
            timeout_secs: Some(timeout.as_secs().max(1)),
        };
        let call = self.call(
            crate::tools::TOOLS_INVOKE_METHOD,
            Some(serde_json::to_value(&params)?),
        );
        let resp = tokio::time::timeout(timeout, call)
            .await
            .map_err(|_| anyhow::anyhow!("orca/tools.invoke timed out after {timeout:?}"))??;
        if resp.is_error() {
            let msg = resp
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown error");
            bail!("orca/tools.invoke '{name}' failed: {msg}");
        }
        let value: crate::tools::ToolInvokeResult =
            serde_json::from_value(resp.result.context("tools.invoke returned null result")?)?;
        Ok(value.result)
    }

    /// Ask the host which peer plugins are currently connected. Used at
    /// startup to fail fast on missing optional deps, or to discover newly
    /// connected peers without polling.
    pub async fn list_peers(&self) -> Result<Vec<crate::tools::PeerInfo>> {
        let resp = self.call(crate::tools::PLUGINS_LIST_METHOD, None).await?;
        if resp.is_error() {
            let msg = resp
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown error");
            bail!("orca/plugins.list failed: {msg}");
        }
        let value: crate::tools::PluginsListResult =
            serde_json::from_value(resp.result.context("plugins.list returned null result")?)?;
        Ok(value.peers)
    }
}

/// Builder for `orca/hello`. Carries the same fields as [`HelloParams`] but
/// keeps optional fields out of the call signature so adding new manifest
/// hints later is a non-breaking SDK change.
#[derive(Debug, Clone)]
pub struct HelloOptions {
    plugin_id: String,
    plugin_version: String,
    flavor: crate::Flavor,
    core_min_required: String,
    methods_required: Vec<String>,
    methods_optional: Vec<String>,
    plugins_required: Vec<String>,
    plugins_optional: Vec<String>,
}

impl HelloOptions {
    pub fn new(plugin_id: impl Into<String>, flavor: crate::Flavor) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            plugin_version: String::new(),
            flavor,
            core_min_required: "0.0.0".to_string(),
            methods_required: Vec::new(),
            methods_optional: Vec::new(),
            plugins_required: Vec::new(),
            plugins_optional: Vec::new(),
        }
    }
    pub fn with_plugin_version(mut self, v: impl Into<String>) -> Self {
        self.plugin_version = v.into();
        self
    }
    pub fn with_core_min_required(mut self, v: impl Into<String>) -> Self {
        self.core_min_required = v.into();
        self
    }
    pub fn with_required_methods(mut self, m: Vec<String>) -> Self {
        self.methods_required = m;
        self
    }
    pub fn with_optional_methods(mut self, m: Vec<String>) -> Self {
        self.methods_optional = m;
        self
    }
    pub fn with_required_plugins(mut self, p: Vec<String>) -> Self {
        self.plugins_required = p;
        self
    }
    pub fn with_optional_plugins(mut self, p: Vec<String>) -> Self {
        self.plugins_optional = p;
        self
    }
}

// ── Incoming-request dispatcher ──────────────────────────────────────────────

/// Convert a server-to-plugin Request into a Response. Currently the only
/// supported method is `orca/tools.call`; anything else returns
/// `method_not_found`. Errors raised by handlers are translated into
/// JSON-RPC error responses with the codes from
/// [`crate::tools::tool_error_codes`].
async fn dispatch_incoming(demux: &Arc<Demux>, req: Request) -> Response {
    if req.method == TOOLS_CALL_METHOD {
        return dispatch_tools_call(demux, req).await;
    }
    Response::err(req.id, ErrorObject::method_not_found(&req.method))
}

async fn dispatch_tools_call(demux: &Arc<Demux>, req: Request) -> Response {
    let params: ToolCallParams = match req
        .params
        .clone()
        .ok_or_else(|| "missing params".to_string())
        .and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string()))
    {
        Ok(p) => p,
        Err(e) => return Response::err(req.id, ErrorObject::invalid_params(&e)),
    };

    let handler = match demux.tools.lock().unwrap().get(&params.name).cloned() {
        Some(t) => t.handler,
        None => {
            return Response::err(
                req.id,
                ErrorObject {
                    code: tool_error_codes::UNKNOWN_TOOL,
                    message: format!("unknown tool: {}", params.name),
                    data: None,
                },
            );
        }
    };

    match handler.call(params.arguments).await {
        Ok(result) => match serde_json::to_value(ToolCallResult { result }) {
            Ok(v) => Response::ok(req.id, v),
            Err(e) => Response::err(req.id, ErrorObject::internal(&e.to_string())),
        },
        Err(e) => Response::err(
            req.id,
            ErrorObject {
                code: tool_error_codes::HANDLER_ERROR,
                message: e.message,
                data: e.data,
            },
        ),
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::tools::ToolHandlerError;
    use serde_json::json;

    fn make_demux() -> Arc<Demux> {
        Demux::new()
    }

    fn req(id: u64, method: &str, params: serde_json::Value) -> Request {
        Request::new(id, method, Some(params))
    }

    #[tokio::test]
    async fn dispatches_registered_tool() {
        let demux = make_demux();
        let handler: Arc<dyn ToolHandler> =
            Arc::new(|args: Value| async move { Ok(json!({"echo": args})) });
        demux.tools.lock().unwrap().insert(
            "stack.list".into(),
            RegisteredTool {
                declaration: ToolDeclaration {
                    name: "stack.list".into(),
                    description: "test".into(),
                    input_schema: json!({}),
                    sensitivity: Sensitivity::General,
                },
                handler,
            },
        );

        let r = req(
            7,
            TOOLS_CALL_METHOD,
            json!({"name":"stack.list","arguments":{"x":1}}),
        );
        let resp = dispatch_incoming(&demux, r).await;
        assert!(!resp.is_error(), "{:?}", resp.error);
        assert_eq!(resp.result.unwrap()["result"]["echo"]["x"], json!(1));
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_code() {
        let demux = make_demux();
        let r = req(
            8,
            TOOLS_CALL_METHOD,
            json!({"name":"missing","arguments":{}}),
        );
        let resp = dispatch_incoming(&demux, r).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, tool_error_codes::UNKNOWN_TOOL);
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let demux = make_demux();
        let r = req(9, "orca/bogus", json!({}));
        let resp = dispatch_incoming(&demux, r).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn handler_error_propagates_with_handler_code() {
        let demux = make_demux();
        let handler: Arc<dyn ToolHandler> =
            Arc::new(|_args: Value| async move { Err(ToolHandlerError::new("upstream rejected")) });
        demux.tools.lock().unwrap().insert(
            "boom".into(),
            RegisteredTool {
                declaration: ToolDeclaration {
                    name: "boom".into(),
                    description: "fails".into(),
                    input_schema: json!({}),
                    sensitivity: Sensitivity::General,
                },
                handler,
            },
        );
        let r = req(10, TOOLS_CALL_METHOD, json!({"name":"boom","arguments":{}}));
        let resp = dispatch_incoming(&demux, r).await;
        assert!(resp.is_error());
        let err = resp.error.unwrap();
        assert_eq!(err.code, tool_error_codes::HANDLER_ERROR);
        assert_eq!(err.message, "upstream rejected");
    }

    #[tokio::test]
    async fn missing_params_returns_invalid_params() {
        let demux = make_demux();
        let r = Request::new(11u64, TOOLS_CALL_METHOD, None);
        let resp = dispatch_incoming(&demux, r).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32602);
    }
}
