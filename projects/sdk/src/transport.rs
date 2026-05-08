//! TCP + mTLS transport: plugin (client) side.
//!
//! A `TcpTransport` wraps a mutually-authenticated TLS stream over TCP.
//! Calls are serialized — only one in-flight request at a time. This is the
//! correct model for Phase A (single-plugin, single-thread use). For concurrent
//! multi-request use, a request demultiplexer with per-request channels is
//! needed (future work).

use anyhow::{Context, Result, bail};
use rustls::pki_types::ServerName;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::{TlsConnector, client::TlsStream};

use crate::framing::{read_frame, write_frame};
use crate::jsonrpc::{Message, Notification, Request, Response};
use crate::pki::NodeBundle;

// ── Hello protocol ────────────────────────────────────────────────────────────

/// Parameters sent in `orca/hello`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HelloParams {
    pub sdk_version: String,
    pub plugin_id: String,
    pub flavor: crate::Flavor,
    pub core_min_required: String,
    #[serde(default)]
    pub methods_required: Vec<String>,
    #[serde(default)]
    pub methods_optional: Vec<String>,
}

/// Result returned by the server for `orca/hello`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HelloResult {
    pub server_version: String,
    pub ok: bool,
    /// "full" | "degraded" | "rejected"
    pub status: String,
    pub methods: Vec<String>,
}

// ── Inner state ───────────────────────────────────────────────────────────────

struct Inner {
    reader: ReadHalf<TlsStream<TcpStream>>,
    writer: WriteHalf<TlsStream<TcpStream>>,
}

impl Inner {
    async fn send_raw(&mut self, json: &[u8]) -> Result<()> {
        write_frame(&mut self.writer, json).await
    }

    async fn recv_raw(&mut self) -> Result<Vec<u8>> {
        read_frame(&mut self.reader).await
    }

    async fn recv_message(&mut self) -> Result<Message> {
        let frame = self.recv_raw().await?;
        serde_json::from_slice(&frame).context("deserialize message")
    }
}

// ── TcpTransport ──────────────────────────────────────────────────────────────

/// A connected, mTLS-authenticated plugin transport.
pub struct TcpTransport {
    inner: Mutex<Inner>,
    next_id: AtomicU64,
}

impl TcpTransport {
    /// Connect to an orca plugin host at `addr` using the supplied node bundle.
    ///
    /// Verifies the server cert against the CA cert in `bundle.ca_cert_pem`.
    /// Presents `bundle.cert_pem` / `bundle.key_pem` as the client identity.
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

        let (r, w) = tokio::io::split(tls);
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner {
                reader: r,
                writer: w,
            }),
            next_id: AtomicU64::new(1),
        }))
    }

    // ── Low-level primitives ──────────────────────────────────────────────────

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Send a request and wait for the matching response.
    ///
    /// Calls are serialized: the mutex is held for the full round-trip so only
    /// one in-flight request exists at a time.
    pub async fn call(&self, method: &str, params: Option<Value>) -> Result<Response> {
        let id = self.alloc_id();
        let req = Request::new(id, method, params);
        let json = serde_json::to_vec(&req)?;

        let mut guard = self.inner.lock().await;
        guard.send_raw(&json).await?;

        loop {
            match guard.recv_message().await? {
                Message::Response(r) if r.id.as_u64() == Some(id) => return Ok(r),
                Message::Response(_) => continue, // stale / out-of-order
                Message::Notification(_) | Message::Request(_) => continue,
            }
        }
    }

    /// Send a notification (fire-and-forget, no response expected).
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let notif = Notification::new(method, params);
        let json = serde_json::to_vec(&notif)?;
        self.inner.lock().await.send_raw(&json).await
    }

    /// Perform the `orca/hello` handshake. Call this immediately after connecting.
    pub async fn hello(
        &self,
        plugin_id: &str,
        flavor: crate::Flavor,
        methods_required: Vec<String>,
        methods_optional: Vec<String>,
    ) -> Result<HelloResult> {
        let params = HelloParams {
            sdk_version: crate::SDK_VERSION.to_string(),
            plugin_id: plugin_id.to_string(),
            flavor,
            core_min_required: "0.1.0".to_string(),
            methods_required,
            methods_optional,
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
            bail!(
                "orca/hello: server returned ok=false (status: {})",
                result.status
            );
        }

        Ok(result)
    }
}
