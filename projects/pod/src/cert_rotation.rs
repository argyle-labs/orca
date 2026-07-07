// Wire envelopes are opaque JSON; mirrors the allow in jsonrpc.rs.
#![allow(clippy::disallowed_types)]

//! Daily cert rotation task.
//!
//! Two paths, picked per host:
//!
//!   * **Secure path** (`has_mesh_ca_key`): self-sign new server+client certs
//!     locally and atomic-rename them over the old ones. Zero network.
//!
//!   * **Non-secure path** (no CA key): pick any active mutual-secure peer
//!     that does have the CA key, dial it on the mTLS pod surface, call
//!     `pod/refresh-cert` with fresh CSRs, install the returned certs.
//!     If every candidate peer is unreachable, log and retry next tick.
//!
//! The TLS resolver in plugin_host reads from disk on every handshake, so
//! `utils::pki::atomic_write_pem` is what makes rotation seamless — no resolver
//! swap, no in-process cache.

use anyhow::{Context, Result};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::{info, warn};
use utils::framing::{read_frame, write_frame};
use utils::jsonrpc::{Message, Request, Response};

use super::pki_dir;
use db::pod as pdb;
use system::periodic;

/// Once per day. Cheap (one cert parse + a comparison), and a stale cert
/// check on this cadence covers a 7-day refresh threshold comfortably.
const TICK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

pub fn spawn() -> tokio::task::JoinHandle<()> {
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "pod.cert_rotation.run",
            // Small initial delay so we don't slam the daemon on every restart.
            initial_delay: Duration::from_secs(60),
            interval: TICK_INTERVAL,
        },
        periodic::boxed(tick),
    )
}

async fn tick() -> Result<()> {
    let pki_d = pki_dir();

    // Drop the previous CA slot once its overlap window has elapsed. Done
    // unconditionally (independent of whether leaf rotation is needed) so a
    // host that's been online through a rotation eventually shrinks back
    // to a single trust anchor without a daemon restart.
    if utils::pki::has_mesh_ca_previous(&pki_d)
        && let Ok(conn) = db::open_default()
        && let Ok(Some(expires_at)) = pdb::get_ca_previous_expires_at(&conn)
        && now_secs() > expires_at
    {
        if let Err(e) = utils::pki::drop_mesh_ca_previous(&pki_d) {
            warn!("[cert-rotation] could not drop previous CA: {e:#}");
        } else {
            _ = pdb::set_ca_previous_expires_at(&conn, None);
            info!("[cert-rotation] dropped previous CA (overlap expired)");
        }
    }

    if !utils::pki::mesh_server_cert_path(&pki_d).exists() {
        return Ok(()); // not a pod member yet
    }

    let server_pem = std::fs::read_to_string(utils::pki::mesh_server_cert_path(&pki_d))?;
    let client_pem = std::fs::read_to_string(utils::pki::mesh_client_cert_path(&pki_d))?;
    let threshold = utils::pki::PEER_REFRESH_THRESHOLD_DAYS;
    let need_server = utils::pki::should_rotate(&server_pem, threshold).unwrap_or(true);
    let need_client = utils::pki::should_rotate(&client_pem, threshold).unwrap_or(true);
    if !need_server && !need_client {
        return Ok(());
    }

    if utils::pki::has_mesh_ca_key(&pki_d) {
        // Cert CN must be stable across hostname flaps — use machine_id.
        let host = system::host_identity::machine_id_short().to_string();
        if need_server {
            utils::pki::reissue_mesh_server_cert(&pki_d).context("self-sign mesh server cert")?;
            info!("[cert-rotation] self-reissued mesh server cert");
        }
        if need_client {
            utils::pki::reissue_mesh_client_cert(&pki_d, &host)
                .context("self-sign mesh client cert")?;
            info!("[cert-rotation] self-reissued mesh client cert");
        }
    } else {
        refresh_via_peer().await?;
    }
    Ok(())
}

async fn refresh_via_peer() -> Result<()> {
    let conn = db::open_default()?;
    let peers = pdb::list_peers(&conn)?;
    drop(conn);
    // Prefer mutually-secure peers (those have the CA key). Skip departed.
    let mut candidates: Vec<_> = peers
        .into_iter()
        .filter(|p| p.departed_at.is_none() && p.local_secure && p.peer_secure)
        .collect();
    if candidates.is_empty() {
        anyhow::bail!("no mutual-secure peers available to sign a refresh");
    }
    // Most-recently-seen first to maximize success likelihood.
    candidates.sort_by_key(|p| std::cmp::Reverse(p.last_seen_at));

    let host = system::host_identity::machine_id_short().to_string();
    let (csr_client, key_client, csr_server, key_server) = utils::pki::build_refresh_csrs(&host)?;

    for p in candidates {
        match call_refresh(&p.peer_addr, p.peer_port, &host, &csr_client, &csr_server).await {
            Ok((client_cert, server_cert)) => {
                let pki_d = pki_dir();
                utils::pki::install_refreshed_peer_certs(
                    &pki_d,
                    &client_cert,
                    &key_client,
                    &server_cert,
                    &key_server,
                )?;
                info!(
                    "[cert-rotation] refreshed peer certs via {} ({}:{})",
                    p.peer_id, p.peer_addr, p.peer_port
                );
                return Ok(());
            }
            Err(e) => warn!("[cert-rotation] {} refused refresh: {e:#}", p.peer_id),
        }
    }
    anyhow::bail!("all candidate peers refused refresh");
}

async fn call_refresh(
    host: &str,
    port: u16,
    joiner_hostname: &str,
    csr_client_pem: &str,
    csr_server_pem: &str,
) -> Result<(String, String)> {
    let pki_d = pki_dir();
    let bundle = utils::pki::load_mesh_client(&pki_d)?;
    let (chain, key) = utils::pki::parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem)?;
    let roots = utils::pki::ca_root_store(&bundle.ca_cert_pem)?;
    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)?;

    let connector = TlsConnector::from(Arc::new(client_config));
    let target = format!("{host}:{port}");
    let tcp = TcpStream::connect(&target)
        .await
        .with_context(|| format!("connect {target}"))?;
    let sni = ServerName::try_from(utils::pki::POD_SERVER_SAN)?.to_owned();
    let mut tls = connector.connect(sni, tcp).await?;

    let params = serde_json::json!({
        "joiner_hostname": joiner_hostname,
        "csr_client_pem": csr_client_pem,
        "csr_server_pem": csr_server_pem,
    });
    write_frame(
        &mut tls,
        &serde_json::to_vec(&Request::new(1, "pod/refresh-cert", Some(params)))?,
    )
    .await?;
    let raw = tokio::time::timeout(Duration::from_secs(15), read_frame(&mut tls))
        .await
        .context("pod/refresh-cert timed out")??;
    let msg: Message = serde_json::from_slice(&raw)?;
    let resp: Response = match msg {
        Message::Response(r) => r,
        _ => anyhow::bail!("non-response frame"),
    };
    if let Some(err) = resp.error {
        anyhow::bail!("peer rejected refresh: {}", err.message);
    }
    let r = resp.result.context("empty refresh result")?;
    let client_cert = r
        .get("client_cert_pem")
        .and_then(|v| v.as_str())
        .context("response missing client_cert_pem")?
        .to_string();
    let server_cert = r
        .get("server_cert_pem")
        .and_then(|v| v.as_str())
        .context("response missing server_cert_pem")?
        .to_string();
    Ok((client_cert, server_cert))
}

use utils::time::now_secs_since_epoch as now_secs;
