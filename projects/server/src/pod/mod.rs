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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodPingResult {
    pub peer_id: String,
    pub version: String,
    pub hostname: String,
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

    let req = Request::new(1, POD_PING_METHOD, None);
    let envelope = serde_json::to_vec(&req).context("serialize ping request")?;
    write_frame(&mut tls, &envelope)
        .await
        .context("write ping frame")?;

    let raw = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut tls))
        .await
        .context("ping read timed out")?
        .context("read ping response")?;
    let msg: Message =
        serde_json::from_slice(&raw).context("parse ping response as JSON-RPC Message")?;
    let resp: Response = match msg {
        Message::Response(r) => r,
        Message::Request(_) | Message::Notification(_) => {
            anyhow::bail!("unexpected message type in response to pod/ping")
        }
    };
    if let Some(err) = resp.error {
        anyhow::bail!("peer returned error: {}", err.message);
    }
    let result = resp.result.context("peer response had no result")?;
    let parsed: PodPingResult = serde_json::from_value(result).context("parse pod/ping result")?;
    Ok(parsed)
}
