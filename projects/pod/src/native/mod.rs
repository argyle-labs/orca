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
pub mod dialer;
pub mod dispatcher;
pub mod host_status_replica;
mod listener;
pub mod mdns;
pub mod roster_sync;
pub mod runtime_cache;
pub mod scheduler;
pub mod subscribe;
pub mod subscribe_client;
pub mod subscribe_demand;
pub mod subscribe_wire;

pub use bootstrap::handle_pod_bootstrap_connection;
pub use listener::handle_pod_connection;

use ::db::ports::mesh_port;
use anyhow::{Context, Result};
use orca_sdk::framing::{read_frame, write_frame};
use orca_sdk::jsonrpc::{Message, Request, Response};
use orca_sdk::pki;
use orca_utils::config::{APP_PKI_DIR, APP_STATE_DIR};
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
    /// Addressing snapshot of the responding peer (rc.25+). Optional +
    /// `#[serde(default)]` so rc.≤24 daemons that omit the field still
    /// deserialize cleanly. Callers use this to refresh
    /// `pod_peer_addresses` without requiring a re-pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addressing: Option<HostAddressingSnapshot>,
}

/// Peer-to-peer addressing snapshot carried on `pod/ping`. `display_name` is
/// the human label; `channels` is the per-channel address list (`lan_v4`,
/// `lan_v6`, `tailscale_v4`, `tailscale_v6`, `fqdn`). Source + detected_at
/// stay local to the responding peer and are not propagated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostAddressingSnapshot {
    pub display_name: String,
    pub channels: Vec<AddressChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressChannel {
    pub kind: String,
    pub value: String,
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

/// Detect a mesh client cert whose CN doesn't match the current naming
/// convention (`peer.<machine_id_short>`). Stale certs come from hosts
/// joined under the old `peer.<hostname>` convention; mixing the two
/// produces duplicate `pod_peers` rows because the TLS-extracted CN keys
/// `ensure_peer_stub` differ from the CNs minted by `pod/join-confirm`.
///
/// When a stale CN is detected we delete the mesh client/server cert+key
/// pairs and wipe `pod_peers/pod_trust/pod_pending_offers/pod_discovery`
/// so the daemon comes up unpaired and the operator can re-pair into a
/// clean mesh. The mesh CA + bootstrap key are preserved (host identity
/// + founder ability survive).
///
/// Returns `Ok(true)` if a reset happened. Best-effort: any error is
/// logged at warn and returns `Ok(false)` so daemon startup proceeds.
pub fn reset_if_stale_mesh_identity(pki_dir: &std::path::Path) -> Result<bool> {
    let cert_path = pki::mesh_client_cert_path(pki_dir);
    let expected = format!("peer.{}", fleet::host_identity::machine_id_short());

    // Classify current state into one of:
    //   "ok"     – cert present, CN matches expected. No-op.
    //   "stale"  – cert present, CN drifted. Wipe + (founder) reissue.
    //   "missing"– cert absent, founder must reissue from its CA. Wipe
    //              pod tables in case a prior partial reset left them.
    //   "none"   – cert absent, no CA. Pre-pod. No-op.
    let state = if cert_path.exists() {
        match std::fs::read_to_string(&cert_path)
            .ok()
            .and_then(|pem| {
                rustls_pemfile::certs(&mut pem.as_bytes())
                    .next()
                    .and_then(Result::ok)
            })
            .and_then(|der| pki::peer_common_name(&der).ok())
        {
            Some(cn) if cn == expected => "ok",
            Some(cn) => {
                tracing::warn!(
                    "[pod] mesh client cert CN {cn:?} does not match expected {expected:?} — \
                     resetting pod identity (cert was issued under an older naming convention)."
                );
                "stale"
            }
            None => {
                tracing::warn!(
                    "[pod] mesh client cert at {} is unreadable — treating as stale",
                    cert_path.display()
                );
                "stale"
            }
        }
    } else if pki::has_mesh_ca_key(pki_dir) {
        tracing::warn!(
            "[pod] mesh client cert is missing but this host holds the CA key — \
             founder will self-reissue client+server certs."
        );
        "missing"
    } else {
        return Ok(false);
    };

    if state == "ok" {
        return Ok(false);
    }

    let mesh = pki::mesh_dir(pki_dir);
    for sub in ["client", "server"] {
        let d = mesh.join(sub);
        if d.exists() {
            _ = std::fs::remove_dir_all(&d);
        }
    }
    let conn = ::db::open_default()?;
    self::db::wipe_pod_membership(&conn)?;
    drop(conn);

    // If this host holds the mesh CA key (founder), self-issue fresh
    // client/server certs under the new CN immediately so the daemon can
    // keep operating without an external re-pair. Joiner-only hosts have
    // to wait for an inviter; log the path so the operator knows.
    if pki::has_mesh_ca_key(pki_dir) {
        let host = fleet::host_identity::machine_id_short().to_string();
        pki::reissue_mesh_server_cert(pki_dir).context("self-reissue mesh server cert")?;
        pki::reissue_mesh_client_cert(pki_dir, &host).context("self-reissue mesh client cert")?;
        tracing::warn!(
            "[pod] founder reissued mesh client+server certs under CN peer.{host}; \
             pod-membership wiped — re-pair joiners as needed"
        );
        let conn = ::db::open_default()?;
        self::db::set_self_secure(&conn, true)?;
    } else {
        tracing::warn!(
            "[pod] mesh cert+pod-membership state wiped; daemon will come up unpaired — \
             re-pair this host with `orca pod join <inviter>` or wait for an mDNS auto-offer"
        );
    }
    Ok(true)
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

// `pod/exec` is the wire-level JSON-RPC dispatch for cross-peer OrcaTool
// invocation. The Value fields here are strictly the JSON-RPC wire payload —
// the caller (`orca_dispatch::cli::exec_remote`) serializes the tool's
// typed Args before this point and deserializes the typed Output immediately
// after, so opaque JSON never reaches any user-facing type.
mod exec_wire {
    #![allow(clippy::disallowed_types)]
    use serde::{Deserialize, Serialize};

    /// Parameters for `pod/exec`. `tool` is a fully-qualified
    /// `<domain>.<verb>` name; `args` is the on-wire JSON args payload.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PodExecParams {
        pub tool: String,
        #[serde(default)]
        pub args: serde_json::Value,
    }

    /// Wire result of `pod/exec` — `result` is the tool's serialized output.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PodExecResult {
        pub tool: String,
        pub result: serde_json::Value,
    }
}

pub use exec_wire::{PodExecParams, PodExecResult};

/// Dial `host` and dispatch an allowlisted OrcaTool on the peer over mTLS.
/// Identity is the mesh client cert; the peer additionally checks the tool's
/// `REMOTE_OK` flag and 401s anything not in its allowlist.
#[allow(clippy::disallowed_types)]
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

/// Open a fresh mTLS client connection to a peer's pod channel. Used by
/// both one-shot `call_typed` and long-lived streaming dials
/// (`subscribe_client`). The caller owns the returned stream.
pub(crate) async fn connect_pod_tls(
    host: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
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
    let addr = format!("{host}:{}", mesh_port());
    let tcp = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let sni = ServerName::try_from(pki::POD_SERVER_SAN)
        .context("build SNI ServerName")?
        .to_owned();
    connector
        .connect(sni, tcp)
        .await
        .context("TLS handshake (is the peer's mesh CA the same as ours?)")
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
    let mut tls = connect_pod_tls(host).await?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_result_deserializes_rc24_without_addressing() {
        let json = serde_json::json!({
            "peer_id": "abc",
            "version": "0.0.3",
            "hostname": "abc123",
        });
        let r: PodPingResult = serde_json::from_value(json).unwrap();
        assert_eq!(r.peer_id, "abc");
        assert!(r.addressing.is_none());
    }

    #[test]
    fn ping_result_roundtrip_rc25_with_addressing() {
        let json = serde_json::json!({
            "peer_id": "abc",
            "version": "0.0.4",
            "hostname": "abc123",
            "addressing": {
                "display_name": "host-g",
                "channels": [
                    { "kind": "lan_v4", "value": "10.0.0.8" },
                    { "kind": "tailscale_v4", "value": "100.96.1.2" },
                ],
            },
        });
        let r: PodPingResult = serde_json::from_value(json).unwrap();
        let a = r.addressing.expect("addressing populated");
        assert_eq!(a.display_name, "host-g");
        assert_eq!(a.channels.len(), 2);
        assert_eq!(a.channels[0].kind, "lan_v4");
        assert_eq!(a.channels[0].value, "10.0.0.8");
        assert_eq!(a.channels[1].kind, "tailscale_v4");
    }

    #[test]
    fn ping_result_serialize_omits_none_addressing() {
        let r = PodPingResult {
            peer_id: "abc".into(),
            version: "0.0.4".into(),
            hostname: "abc123".into(),
            addressing: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert!(
            v.get("addressing").is_none(),
            "None must be skipped on wire"
        );
    }
}
