//! Pod / mesh networking — peer-to-peer mTLS over SNI=pod.orca.local.
//!
//! Phase 1 surface:
//!   - Founder bootstrap (`orca pod init` → cmd::cmd_pod_init).
//!   - Server-side connection handler invoked by plugin_host when SNI=pod.
//!   - Client-side `pod ping` dialer.
//!
//! Wire format: JSON-RPC 2.0 over the same length-prefixed framing as the
//! plugin host. Reusing that framing means no axum/hyper on this path.
//!
//! v1 methods:
//!   - `pod/ping`  → returns `{peer_id, version, hostname}` so two hosts can
//!     confirm the SNI multiplex + mTLS chain end-to-end.
//!
//! v2 will add `pod/join`, `pod/list`, and the trust-promotion methods.

mod bootstrap;
pub mod cert_rotation;
pub mod db;
mod listener;
pub mod mdns;
pub mod scheduler;

pub use bootstrap::handle_pod_bootstrap_connection;
pub use listener::handle_pod_connection;

use anyhow::{Context, Result};
use orca_sdk::framing::{read_frame, write_frame};
use orca_sdk::jsonrpc::{Message, Request, Response};
use orca_sdk::pki;
use orca_utils::config::{APP_PKI_DIR, APP_PLUGIN_PORT, APP_STATE_DIR};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

pub const POD_PING_METHOD: &str = "pod/ping";
pub const POD_DEV_SYNC_METHOD: &str = "pod/dev-sync";
pub const POD_DEV_ENABLE_METHOD: &str = "pod/dev-enable";
pub const POD_DEV_DISABLE_METHOD: &str = "pod/dev-disable";
pub const POD_EXEC_METHOD: &str = "pod/exec";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodPingResult {
    pub peer_id: String,
    pub version: String,
    pub hostname: String,
}

/// Result of `pod/dev-sync`. `status` is one of:
/// - `"synced"` — `git pull` completed; cargo-watch will rebuild.
/// - `"skipped"` — peer is not in dev mode (intentional no-op).
/// - `"error"`  — pull failed; `detail` carries the message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodDevSyncResult {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits_pulled: Option<u32>,
}

/// Resolve the PKI dir for this host using the same logic as the rest of
/// the daemon (HOME + APP_STATE_DIR + APP_PKI_DIR).
pub fn pki_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(APP_STATE_DIR).join(APP_PKI_DIR)
}

/// Dial `host` over mTLS with SNI=pod.orca.local, send a `pod/ping`, and
/// return the peer's report. `host` is a bare hostname or IP; the connector
/// always uses the canonical SNI so the server's resolver returns the
/// mesh-CA-signed cert.
pub async fn ping(host: &str) -> Result<PodPingResult> {
    call_typed(host, POD_PING_METHOD, None::<()>, Duration::from_secs(5)).await
}

/// Result of `pod/dev-enable`. `status` is `"enabled"` on success, `"error"`
/// on failure (`detail` carries the message).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodDevEnableResult {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_parked: Option<bool>,
}

/// Result of `pod/dev-disable`. `status` is `"disabled"` on success,
/// `"error"` on failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodDevDisableResult {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_process_stopped: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_reclaimed: Option<bool>,
}

/// Dial `host` over the existing pod mTLS channel and ask it to git-pull its
/// dev checkout. `host` is a bare hostname or IP; SNI is fixed to
/// `pod.orca.local`. Identity is proven by the mesh-CA-signed client cert —
/// no bearer tokens involved, so this is the canonical peer↔peer auth path.
pub async fn dev_sync(host: &str) -> Result<PodDevSyncResult> {
    // git pull + cargo-watch detect can run long on a slow LAN; allow more
    // headroom than `pod/ping`.
    call_typed(
        host,
        POD_DEV_SYNC_METHOD,
        None::<()>,
        Duration::from_secs(45),
    )
    .await
}

/// Ask `host` to flip into dev mode. cmd_dev_enable may clone the repo on
/// first run, so allow generous timeout.
pub async fn dev_enable(host: &str) -> Result<PodDevEnableResult> {
    call_typed(
        host,
        POD_DEV_ENABLE_METHOD,
        None::<()>,
        Duration::from_secs(120),
    )
    .await
}

/// Parameters for `pod/exec`. `tool` is a fully-qualified `<domain>.<verb>`
/// name; `args` is the raw JSON args payload. The peer side checks the
/// allowlist (`OrcaToolDef::REMOTE_OK`) before dispatching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodExecParams {
    pub tool: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// Result of `pod/exec` — `result` is the tool's typed output as JSON.
/// `tool` echoes the request for trace clarity. Errors surface as JSON-RPC
/// errors at the wire level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodExecResult {
    pub tool: String,
    pub result: serde_json::Value,
}

/// Dial `host` and dispatch an allowlisted OrcaTool on the peer over mTLS.
/// Identity is the mesh client cert; the peer additionally checks the tool's
/// `REMOTE_OK` flag and 401s anything not in its allowlist.
pub async fn exec(host: &str, tool: &str, args: serde_json::Value) -> Result<PodExecResult> {
    call_typed(
        host,
        POD_EXEC_METHOD,
        Some(PodExecParams {
            tool: tool.to_string(),
            args,
        }),
        Duration::from_secs(120),
    )
    .await
}

/// Ask `host` to drop dev mode and let the production daemon reclaim.
pub async fn dev_disable(host: &str) -> Result<PodDevDisableResult> {
    call_typed(
        host,
        POD_DEV_DISABLE_METHOD,
        None::<()>,
        Duration::from_secs(30),
    )
    .await
}

/// Generic mTLS JSON-RPC roundtrip to a peer over the pod channel. One-shot:
/// connect → write one request → read one response → return. No pooling yet;
/// adopters call this directly per peer. Keeping the connection short-lived
/// matches how `pod/ping` worked previously and avoids leaking sockets.
async fn call_typed<P, R>(
    host: &str,
    method: &str,
    params: Option<P>,
    timeout: Duration,
) -> Result<R>
where
    P: Serialize,
    R: for<'de> Deserialize<'de>,
{
    let pki = pki_dir();
    let bundle =
        pki::load_mesh_client(&pki).context("load mesh client bundle (run `orca pod init`)")?;
    let (chain, key) = pki::parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem)?;
    let roots = Arc::new(pki::ca_root_store(&bundle.ca_cert_pem)?);

    let client_config = ClientConfig::builder()
        .with_root_certificates((*roots).clone())
        .with_client_auth_cert(chain, key)
        .context("build client TLS config")?;

    let connector = TlsConnector::from(Arc::new(client_config));
    let addr = format!("{host}:{}", APP_PLUGIN_PORT);
    let tcp = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let sni = ServerName::try_from(pki::POD_SERVER_SAN)
        .context("build SNI ServerName")?
        .to_owned();
    let mut tls = connector
        .connect(sni, tcp)
        .await
        .context("TLS handshake (is the peer's mesh CA the same as ours?)")?;

    let params_value = match params {
        Some(p) => Some(serde_json::to_value(p).context("serialize request params")?),
        None => None,
    };
    let req = Request::new(1, method, params_value);
    let envelope = serde_json::to_vec(&req).context("serialize request")?;
    write_frame(&mut tls, &envelope)
        .await
        .context("write request frame")?;

    let raw = tokio::time::timeout(timeout, read_frame(&mut tls))
        .await
        .with_context(|| format!("{method} read timed out"))?
        .context("read response")?;
    let msg: Message =
        serde_json::from_slice(&raw).context("parse response as JSON-RPC Message")?;
    let resp: Response = match msg {
        Message::Response(r) => r,
        Message::Request(_) | Message::Notification(_) => {
            anyhow::bail!("unexpected message type in response to {method}")
        }
    };
    if let Some(err) = resp.error {
        anyhow::bail!("peer returned error: {}", err.message);
    }
    let result = resp.result.context("peer response had no result")?;
    serde_json::from_value(result).with_context(|| format!("parse {method} result"))
}
