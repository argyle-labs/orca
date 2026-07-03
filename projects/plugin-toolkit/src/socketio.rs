//! Generic async **Socket.IO client transport**.
//!
//! The socket analogue of the shared HTTP facade (`api_client` / `http`): a
//! protocol-agnostic client for services that speak Socket.IO / Engine.IO v4
//! over WebSocket and expose no REST API (e.g. dockge). This module knows
//! nothing about any specific service — the plugin owns its event names and
//! payloads; payloads are [`serde_json::Value`] because Socket.IO frames are
//! dynamic by nature.
//!
//! Reuse [`crate::api_client::Credentials`] for username/password; login itself
//! is a per-service event, driven by the plugin via [`SocketSession::emit_ack`]
//! (e.g. dockge's `login` → JWT-in-ack).
//!
//! ```no_run
//! # async fn ex() -> anyhow::Result<()> {
//! use plugin_toolkit::socketio::{SocketConfig, SocketSession};
//! use plugin_toolkit::serde_json::json;
//! use std::time::Duration;
//!
//! let sess = SocketSession::connect(
//!     SocketConfig::new("wss://dockge.lan:5001").insecure(true),
//! ).await?;
//! let ack = sess
//!     .emit_ack("login", json!({ "username": "svc", "password": "…" }), Duration::from_secs(5))
//!     .await?;
//! # let _ = ack; Ok(()) }
//! ```
//!
//! A generic Socket.IO transport necessarily traffics in dynamic JSON — it
//! cannot know each service's typed shapes (the plugin decodes acks/pushes into
//! its own structs). `serde_json::Value` therefore lives ONLY at this transport
//! boundary, which is why `disallowed_types` is allowed for this module alone.
#![allow(clippy::disallowed_types)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::FutureExt;
use rust_socketio::Payload;
use rust_socketio::asynchronous::{Client, ClientBuilder};
use serde_json::Value;

/// A handler for a server-pushed event (e.g. dockge's `stackList`,
/// `terminalWrite`). Registered at connect time — Socket.IO callbacks bind to
/// the builder, not a live client.
pub type PushHandler = Arc<dyn Fn(Value) + Send + Sync + 'static>;

/// Connection parameters for a [`SocketSession`].
#[derive(Debug, Clone)]
pub struct SocketConfig {
    /// Base URL to connect to, e.g. `wss://host:5001` or `http://host:5001`.
    pub url: String,
    /// Accept self-signed / invalid TLS certs — common for homelab `wss`.
    pub accept_invalid_certs: bool,
    /// How long to wait for the initial connect handshake.
    pub connect_timeout: Duration,
}

impl SocketConfig {
    /// Config with sane defaults (verified TLS, 20s connect timeout).
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            accept_invalid_certs: false,
            connect_timeout: Duration::from_secs(20),
        }
    }

    /// Accept invalid/self-signed TLS certificates on this connection.
    pub fn insecure(mut self, on: bool) -> Self {
        self.accept_invalid_certs = on;
        self
    }

    /// Override the connect-handshake timeout.
    pub fn connect_timeout(mut self, dur: Duration) -> Self {
        self.connect_timeout = dur;
        self
    }
}

/// A connected Socket.IO session. Hold one per endpoint for its lifetime —
/// Socket.IO authorization is per-connection.
pub struct SocketSession {
    client: Client,
}

impl SocketSession {
    /// Connect with no server-push handlers (request/ack usage only).
    pub async fn connect(cfg: SocketConfig) -> Result<Self> {
        Self::connect_with(cfg, Vec::new()).await
    }

    /// Connect and register handlers for server-pushed events. Handlers must be
    /// supplied up front because Socket.IO binds callbacks to the builder.
    pub async fn connect_with(
        cfg: SocketConfig,
        handlers: Vec<(String, PushHandler)>,
    ) -> Result<Self> {
        let mut builder = ClientBuilder::new(cfg.url.clone()).reconnect(true);

        if cfg.accept_invalid_certs {
            let connector = native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true)
                .build()
                .context("build insecure TLS connector")?;
            builder = builder.tls_config(connector);
        }

        // `ClientBuilder::connect()` returns once the Engine.IO transport is up,
        // but BEFORE the Socket.IO namespace `connect` (the `40` packet)
        // completes — emitting in that window races ahead of a ready socket and
        // the server never sees it (the first `emit_ack` then times out). Signal
        // readiness from the reserved `Connect` event and wait for it below.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));
        {
            let ready_tx = ready_tx.clone();
            builder = builder.on(
                rust_socketio::Event::Connect,
                move |_payload: Payload, _client: Client| {
                    let ready_tx = ready_tx.clone();
                    async move {
                        if let Ok(mut guard) = ready_tx.lock()
                            && let Some(tx) = guard.take()
                        {
                            tx.send(()).ok();
                        }
                    }
                    .boxed()
                },
            );
        }

        for (event, handler) in handlers {
            builder = builder.on(event, move |payload: Payload, _client: Client| {
                let handler = handler.clone();
                async move {
                    handler(payload_to_value(payload));
                }
                .boxed()
            });
        }

        let client = tokio::time::timeout(cfg.connect_timeout, builder.connect())
            .await
            .map_err(|_| anyhow!("socket.io connect to {} timed out", cfg.url))?
            .with_context(|| format!("socket.io connect to {}", cfg.url))?;

        // Wait for the namespace `connect` before handing the session back, so
        // the caller's first emit lands on a ready socket. Bounded by the same
        // connect timeout; if the event never arrives we proceed rather than
        // hang (emit_ack has its own timeout as a backstop).
        let _readiness = tokio::time::timeout(cfg.connect_timeout, ready_rx).await;

        Ok(Self { client })
    }

    /// Emit an event with a single argument and await the server's ack. See
    /// [`Self::emit_ack_args`] for events that take multiple positional args
    /// (e.g. dockge's agent wrapper: `("agent", endpoint, event, …)`).
    pub async fn emit_ack(&self, event: &str, args: Value, timeout: Duration) -> Result<Value> {
        self.emit_ack_args(event, vec![args], timeout).await
    }

    /// Emit an event with **multiple positional arguments** and await the ack.
    /// Socket.IO events are positional (`emit(event, a, b, c, cb)`); this sends
    /// `args` as those positions. Single-arg acks are unwrapped; multi-arg acks
    /// come back as a JSON array.
    pub async fn emit_ack_args(
        &self,
        event: &str,
        args: Vec<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Value>();
        let tx = Arc::new(Mutex::new(Some(tx)));

        self.client
            .emit_with_ack(
                event.to_string(),
                Payload::Text(args),
                timeout,
                move |payload: Payload, _client: Client| {
                    let tx = tx.clone();
                    async move {
                        let Ok(mut guard) = tx.lock() else { return };
                        if let Some(tx) = guard.take() {
                            // receiver may be gone if the caller already timed out
                            tx.send(payload_to_value(payload)).ok();
                        }
                    }
                    .boxed()
                },
            )
            .await
            .map_err(|e| anyhow!("emit_with_ack '{event}': {e}"))?;

        // rust_socketio invokes the ack callback only on receipt; guard the wait
        // with our own timeout (a hair beyond the ack timeout) so a silent server
        // can't hang the caller.
        match tokio::time::timeout(timeout + Duration::from_secs(1), rx).await {
            Ok(Ok(v)) => Ok(unwrap_ack(v)),
            Ok(Err(_)) => bail!("ack channel closed for '{event}'"),
            Err(_) => bail!("ack for '{event}' timed out"),
        }
    }

    /// Fire an event with a single argument, without awaiting an ack.
    pub async fn emit(&self, event: &str, args: Value) -> Result<()> {
        self.emit_args(event, vec![args]).await
    }

    /// Fire an event with **multiple positional arguments**, without an ack.
    pub async fn emit_args(&self, event: &str, args: Vec<Value>) -> Result<()> {
        self.client
            .emit(event.to_string(), Payload::Text(args))
            .await
            .map_err(|e| anyhow!("emit '{event}': {e}"))
    }

    /// Disconnect the session.
    pub async fn disconnect(self) -> Result<()> {
        self.client
            .disconnect()
            .await
            .map_err(|e| anyhow!("disconnect: {e}"))
    }
}

/// A Socket.IO ack delivers the callback's *args array* as the payload, so a
/// single-arg ack (the common case — `callback({ ok, … })`) arrives wrapped one
/// level deep: `[{ ok, … }]`. Strip that wrapper so callers get the value they
/// acked with; multi-arg acks (`[a, b]`) are left as an array.
fn unwrap_ack(v: Value) -> Value {
    match v {
        Value::Array(mut items) if items.len() == 1 => items.remove(0),
        other => other,
    }
}

/// Flatten a Socket.IO [`Payload`] into a single JSON value. A one-element text
/// payload (the common ack shape) is unwrapped; multiple args become an array.
fn payload_to_value(payload: Payload) -> Value {
    match payload {
        Payload::Text(mut items) => {
            if items.len() == 1 {
                items.remove(0)
            } else {
                Value::Array(items)
            }
        }
        Payload::Binary(bytes) => Value::String(format!("<binary {} bytes>", bytes.len())),
        #[allow(deprecated)]
        Payload::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_defaults_are_secure() {
        let c = SocketConfig::new("wss://dockge.lan:5001");
        assert_eq!(c.url, "wss://dockge.lan:5001");
        assert!(!c.accept_invalid_certs);
        assert_eq!(c.connect_timeout, Duration::from_secs(20));
        assert!(c.insecure(true).accept_invalid_certs);
    }

    #[test]
    fn payload_single_arg_is_unwrapped() {
        let p = Payload::Text(vec![json!({ "ok": true })]);
        assert_eq!(payload_to_value(p), json!({ "ok": true }));
    }

    #[test]
    fn payload_multi_arg_is_array() {
        let p = Payload::Text(vec![json!("a"), json!("b")]);
        assert_eq!(payload_to_value(p), json!(["a", "b"]));
    }

    #[test]
    fn unwrap_ack_strips_single_arg_array() {
        // dockge login ack shape: `[{ ok, msg }]` → the object.
        assert_eq!(
            unwrap_ack(json!([{ "ok": true, "token": "jwt" }])),
            json!({ "ok": true, "token": "jwt" })
        );
    }

    #[test]
    fn unwrap_ack_leaves_multi_arg_and_scalars() {
        assert_eq!(unwrap_ack(json!(["a", "b"])), json!(["a", "b"]));
        assert_eq!(unwrap_ack(json!({ "ok": true })), json!({ "ok": true }));
        assert_eq!(unwrap_ack(json!(null)), json!(null));
    }
}
