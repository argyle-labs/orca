// Wire envelopes are opaque JSON; mirrors the allow in jsonrpc.rs.
#![allow(clippy::disallowed_types)]

//! Auto-offer scheduler.
//!
//! Periodically (every 15s by default) scans `pod_discovery` for peers in
//! state=unclaimed and pushes a pod/offer to each — provided:
//!
//!   * This host has the mesh CA private key (`can_invite`).
//!   * This host's `self_secure` is true.
//!   * There isn't already an open outbound offer to that peer fp.
//!
//! The dial is over SNI=pod-bootstrap.orca.local with the joiner's
//! mDNS-advertised pubkey pinned. If the dial fails (peer offline, blocked,
//! restarted with a new key), the row stays unclaimed and we retry next tick.

use anyhow::{Context, Result};
use orca_sdk::framing::{read_frame, write_frame};
use orca_sdk::jsonrpc::{Message, Request, Response};
use orca_sdk::pki;
use orca_utils::config::APP_PLUGIN_PORT;
use rand::Rng;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::{info, warn};

use super::{db as pdb, pki_dir};
use crate::periodic;

const TICK_INTERVAL: Duration = Duration::from_secs(15);
pub const OFFER_TTL_SECS: i64 = 600;
const PAIRING_CODE_LEN: usize = 6;

/// Spawn the scheduler. Returns immediately; the task runs until the process
/// exits.
pub fn spawn() -> tokio::task::JoinHandle<()> {
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "pod.scheduler.run",
            initial_delay: Duration::ZERO,
            interval: TICK_INTERVAL,
        },
        periodic::boxed(tick),
    )
}

async fn tick() -> Result<()> {
    // Gate: only secure hosts with the CA key extend offers.
    let pki_d = pki_dir();
    if !pki::has_mesh_ca_key(&pki_d) {
        return Ok(());
    }
    let conn = db::open_default()?;
    if !pdb::get_self_secure(&conn)? {
        return Ok(());
    }
    let pod_id = pdb::get_pod_id(&conn)?.unwrap_or_else(|| "default".to_string());

    let unclaimed = pdb::list_unclaimed_discovery(&conn)?;
    drop(conn);

    for d in unclaimed {
        let conn = db::open_default()?;
        if pdb::has_open_outbound_offer(&conn, &d.pubkey_fp)? {
            continue;
        }
        let code = mint_pairing_code();
        let code_hash = pdb::hash_code(&code);
        let offer_id = uuid::Uuid::new_v4().to_string();
        pdb::insert_pending_offer(
            &conn,
            &offer_id,
            "out",
            &d.pubkey_fp,
            &d.hostname,
            &d.addr,
            d.port,
            &code_hash,
            None,
            None,
            None,
            OFFER_TTL_SECS,
        )?;
        drop(conn);

        let pod_id = pod_id.clone();
        let code_for_log = code.clone();
        let hostname = d.hostname.clone();
        let addr = d.addr.clone();
        let port = d.port;
        let fp = d.pubkey_fp.clone();
        tokio::spawn(async move {
            if let Err(e) = push_offer(&hostname, &addr, port, &fp, &code, &pod_id).await {
                warn!("[pod-scheduler] push offer to {hostname} failed: {e:#}");
            } else {
                info!(
                    "[pod-scheduler] offered pod-membership to {hostname} ({addr}:{port}) — pairing code: {code_for_log}"
                );
            }
        });
    }
    Ok(())
}

pub fn mint_pairing_code() -> String {
    // Crockford base32 alphabet minus I/L/O/U to avoid visual confusion.
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
    let mut s = String::with_capacity(PAIRING_CODE_LEN);
    let mut buf = [0u8; PAIRING_CODE_LEN];
    rand::rng().fill_bytes(&mut buf);
    for b in buf {
        let i = (b as usize) % ALPHABET.len();
        s.push(ALPHABET[i] as char);
    }
    s
}

/// Push a pod-membership offer to a joiner over its bootstrap surface. Mints
/// no DB state — callers (the scheduler and the `pod.offer` tool) must have
/// already inserted the outbound `pod_pending_offers` row keyed by
/// `joiner_pubkey_fp` so the joiner's confirm dial can be reconciled.
///
/// `code` is the raw pairing code shown on both sides. The joiner sees it on
/// the inviter's daemon log + on its own `pod pending` row.
pub async fn push_offer(
    _joiner_hostname: &str,
    joiner_addr: &str,
    joiner_port: u16,
    joiner_pubkey_fp: &str,
    code: &str,
    pod_id: &str,
) -> Result<()> {
    let pki_d = pki_dir();
    let signing = pki::load_or_init_bootstrap_key(&pki_d)?;
    let mesh_ca_cert_pem =
        std::fs::read_to_string(pki::mesh_ca_cert_path(&pki_d)).context("read mesh CA cert")?;

    let inviter_hostname = crate::host_identity::hostname().to_string();
    let inviter_peer_id = format!("peer.{}", crate::host_identity::machine_id_short());

    #[derive(serde::Serialize)]
    struct OfferBody<'a> {
        inviter_peer_id: &'a str,
        inviter_hostname: &'a str,
        inviter_addr: &'a str,
        inviter_port: u16,
        mesh_ca_cert_pem: &'a str,
        pod_id: &'a str,
        code_hash: String,
        expires_at: i64,
        inviter_display_name: &'a str,
    }
    let body = OfferBody {
        inviter_peer_id: &inviter_peer_id,
        inviter_hostname: &inviter_hostname,
        inviter_addr: "", // joiner uses the TLS source addr; we don't reveal ours here
        inviter_port: APP_PLUGIN_PORT,
        mesh_ca_cert_pem: &mesh_ca_cert_pem,
        pod_id,
        code_hash: pdb::hash_code(code),
        expires_at: now_secs() + OFFER_TTL_SECS,
        inviter_display_name: &inviter_hostname,
    };
    let env = pki::sign_envelope(&signing, &body)?;

    // Pinned dial to the joiner's bootstrap surface.
    let verifier = pki::pinned_bootstrap_verifier(joiner_pubkey_fp.to_string());
    let client_config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    let target = format!("{joiner_addr}:{joiner_port}");
    let tcp = TcpStream::connect(&target)
        .await
        .with_context(|| format!("dial {target}"))?;
    let sni = ServerName::try_from(pki::POD_BOOTSTRAP_SAN)
        .context("bootstrap SNI")?
        .to_owned();
    let mut tls = connector
        .connect(sni, tcp)
        .await
        .context("bootstrap TLS handshake")?;

    let req = Request::new(1, "pod/offer", Some(serde_json::to_value(&env)?));
    write_frame(&mut tls, &serde_json::to_vec(&req)?).await?;

    let raw = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut tls))
        .await
        .context("pod/offer response timed out")??;
    let msg: Message = serde_json::from_slice(&raw)?;
    let resp: Response = match msg {
        Message::Response(r) => r,
        _ => anyhow::bail!("non-response frame"),
    };
    if let Some(err) = resp.error {
        anyhow::bail!("joiner rejected offer: {}", err.message);
    }
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
