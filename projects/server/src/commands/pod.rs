// JSON-RPC envelopes are inherently opaque at the wire boundary; mirroring
// the allow in projects/sdk/rust/src/jsonrpc.rs.
#![allow(clippy::disallowed_types)]

//! CLI handlers for `orca pod {discover,pending,accept,connect,offer,list,
//! trust,self-secure,leave}`. Init lives in main.rs; ping lives in pod::ping.

use anyhow::{Context, Result, bail};
use orca_sdk::framing::{read_frame, write_frame};
use orca_sdk::jsonrpc::{Message, Request, Response};
use orca_sdk::pki::{self, PeerRole};
use orca_utils::config::APP_PLUGIN_PORT;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::pod::{db as pdb, pki_dir};

// ── pod discover ─────────────────────────────────────────────────────────────

pub fn cmd_pod_discover() -> Result<()> {
    let conn = db::open_default()?;
    let rows = pdb::list_discovery(&conn)?;
    if rows.is_empty() {
        println!("(no peers discovered yet — mDNS browse runs in the daemon; ensure it's up)");
        return Ok(());
    }
    println!(
        "{:<32} {:<16} {:<22} {:<10} {}",
        "pubkey_fp", "hostname", "addr:port", "state", "can_invite"
    );
    for r in rows {
        println!(
            "{:<32} {:<16} {:<22} {:<10} {}",
            r.pubkey_fp,
            r.hostname,
            format!("{}:{}", r.addr, r.port),
            r.state,
            r.can_invite
        );
    }
    Ok(())
}

// ── pod pending ──────────────────────────────────────────────────────────────

pub fn cmd_pod_pending() -> Result<()> {
    let conn = db::open_default()?;
    let rows = pdb::list_pending_offers(&conn, "in")?;
    if rows.is_empty() {
        println!(
            "(no pending offers — secure peers on the network will push offers automatically)"
        );
        return Ok(());
    }
    println!("Pending pod-membership offers:");
    for r in rows {
        let id = r.inviter_peer_id.as_deref().unwrap_or("?");
        let pod = r.pod_id.as_deref().unwrap_or("?");
        println!(
            "  • from {id} (pod {pod}) at {}:{} (fp {})",
            r.peer_addr, r.peer_port, r.peer_pubkey_fp
        );
        println!(
            "    expires in {}s — run `orca pod accept <code>` once you have the 6-char pairing code from the inviter",
            (r.expires_at - now_secs()).max(0)
        );
    }
    Ok(())
}

// ── pod accept ───────────────────────────────────────────────────────────────

pub async fn cmd_pod_accept(code: &str) -> Result<()> {
    let conn = db::open_default()?;
    let offer = pdb::find_pending_offer_by_code(&conn, code)?
        .context("no pending offer matches that code (mistyped, expired, or already used?)")?;
    drop(conn);

    let pki_d = pki_dir();
    std::fs::create_dir_all(pki::mesh_dir(&pki_d))?;
    let ca_pem = offer
        .mesh_ca_cert_pem
        .as_deref()
        .context("offer has no mesh CA cert")?;
    std::fs::write(pki::mesh_ca_cert_path(&pki_d), ca_pem.as_bytes())?;

    let hostname = local_hostname();
    let (csr_client_pem, client_key_pem) = pki::build_peer_csr(&hostname, PeerRole::Client)?;
    let (csr_server_pem, server_key_pem) = pki::build_peer_csr(&hostname, PeerRole::Server)?;

    // Dial inviter's bootstrap SNI, pinned to the fp stored on the offer row.
    let signing = pki::load_or_init_bootstrap_key(&pki_d)?;
    #[derive(serde::Serialize)]
    struct ConfirmBody<'a> {
        code: &'a str,
        joiner_hostname: &'a str,
        csr_client_pem: &'a str,
        csr_server_pem: &'a str,
    }
    let body = ConfirmBody {
        code,
        joiner_hostname: &hostname,
        csr_client_pem: &csr_client_pem,
        csr_server_pem: &csr_server_pem,
    };
    let env = pki::sign_envelope(&signing, &body)?;

    let resp_value = dial_bootstrap(
        &offer.peer_addr,
        offer.peer_port,
        &offer.peer_pubkey_fp,
        "pod/join-confirm",
        serde_json::to_value(&env)?,
    )
    .await
    .context("pod/join-confirm over bootstrap channel failed")?;

    #[derive(serde::Deserialize)]
    struct Result_ {
        client_cert_pem: String,
        server_cert_pem: String,
        ca_cert_pem: String,
        inviter_peer_id: String,
        pod_id: String,
    }
    let r: Result_ = serde_json::from_value(resp_value)?;

    // Persist signed certs alongside the locally-generated keys.
    let server_dir = pki::mesh_dir(&pki_d).join("server");
    let client_dir = pki::mesh_dir(&pki_d).join("client");
    std::fs::create_dir_all(&server_dir)?;
    std::fs::create_dir_all(&client_dir)?;
    std::fs::write(pki::mesh_server_cert_path(&pki_d), &r.server_cert_pem)?;
    std::fs::write(pki::mesh_server_key_path(&pki_d), &server_key_pem)?;
    std::fs::write(pki::mesh_client_cert_path(&pki_d), &r.client_cert_pem)?;
    std::fs::write(pki::mesh_client_key_path(&pki_d), &client_key_pem)?;

    let conn = db::open_default()?;
    pdb::set_self_secure(&conn, false)?;
    pdb::set_pod_id(&conn, &r.pod_id)?;
    pdb::upsert_peer(
        &conn,
        &r.inviter_peer_id,
        &offer.peer_hostname,
        &offer.peer_addr,
        offer.peer_port,
        Some(&offer.peer_pubkey_fp),
        &r.ca_cert_pem,
    )?;
    pdb::delete_pending_offer(&conn, &offer.offer_id)?;

    println!(
        "✓ joined pod {} via {} ({}:{})",
        r.pod_id, r.inviter_peer_id, offer.peer_addr, offer.peer_port
    );
    println!(
        "  self_secure is OFF — run `orca pod self-secure on` to enable secrets writes on this host."
    );
    Ok(())
}

// ── pod connect (manual fallback when mDNS is blocked) ───────────────────────

pub async fn cmd_pod_connect(addr: &str) -> Result<()> {
    let (host, port) = pki::parse_peer_addr(addr, APP_PLUGIN_PORT)?;
    println!(
        "⚠ `pod connect {host}:{port}` is a manual fallback. Run this on the joiner. \
         For automatic pairing on a shared LAN, just wait for `orca pod pending` to populate."
    );
    // v1 manual flow: surface a hint. Full reverse-discovery handshake (joiner
    // asks "is there an offer for me?") is a follow-up — currently the inviter
    // must push via `orca pod offer <joiner-addr>` from its side.
    println!("Ask a secure peer on {host} to run: `orca pod offer <this-host>`.");
    Ok(())
}

// ── pod offer (manual: push to a specific address) ───────────────────────────

pub async fn cmd_pod_offer(addr: &str) -> Result<()> {
    let (host, port) = pki::parse_peer_addr(addr, APP_PLUGIN_PORT)?;
    println!(
        "Manual `pod offer {host}:{port}` is queued for v1.1 — for now, ensure both hosts can \
         see each other via mDNS and the daemon's auto-offer scheduler will handle it. \
         (Cross-subnet support is on the roadmap.)"
    );
    Ok(())
}

// ── pod list ─────────────────────────────────────────────────────────────────

pub fn cmd_pod_list() -> Result<()> {
    let conn = db::open_default()?;
    let peers = pdb::list_peers(&conn)?;
    if peers.is_empty() {
        println!("(no pod peers — run `orca pod discover` to see what's on the LAN)");
        return Ok(());
    }
    println!(
        "{:<28} {:<16} {:<22} {:<8} {:<8} {}",
        "peer_id", "hostname", "addr:port", "local", "peer", "status"
    );
    for p in peers {
        let status = if p.departed_at.is_some() {
            "DEPARTED"
        } else {
            "active"
        };
        println!(
            "{:<28} {:<16} {:<22} {:<8} {:<8} {}",
            p.peer_id,
            p.peer_hostname,
            format!("{}:{}", p.peer_addr, p.peer_port),
            p.local_secure,
            p.peer_secure,
            status
        );
    }
    Ok(())
}

// ── pod trust ────────────────────────────────────────────────────────────────

pub async fn cmd_pod_trust(peer_id: &str, on: bool) -> Result<()> {
    let conn = db::open_default()?;
    let peer = pdb::list_peers(&conn)?
        .into_iter()
        .find(|p| p.peer_id == peer_id)
        .with_context(|| format!("no such peer: {peer_id}"))?;
    let new = pdb::set_trust(&conn, peer_id, Some(on), None)?;
    println!(
        "✓ local trust for {peer_id} → {on} (peer side: {})",
        new.peer_secure
    );

    match call_pod_method(
        &peer.peer_addr,
        peer.peer_port,
        "pod/notify-trust",
        serde_json::json!({ "trust": on }),
    )
    .await
    {
        Ok(_) => println!("✓ notified {peer_id}"),
        Err(e) => println!("  warning: notify-trust dial failed ({e}); peer will pick it up later"),
    }

    if pdb::is_mutual_secure(new) {
        println!("→ mutual secure; replicating CA key if needed…");
        if let Err(e) = replicate_ca_key_if_needed(&peer).await {
            println!("  warning: CA-key replication: {e}");
        }
    }
    Ok(())
}

async fn replicate_ca_key_if_needed(peer: &pdb::PeerRow) -> Result<()> {
    let pki_d = pki_dir();
    let i_have_key = pki::has_mesh_ca_key(&pki_d);
    let resp = call_pod_method(
        &peer.peer_addr,
        peer.peer_port,
        "pod/has-ca-key",
        serde_json::json!({}),
    )
    .await?;
    let peer_has_key = resp
        .get("has_key")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if i_have_key && !peer_has_key {
        let (cert_pem, key_pem) = pki::export_mesh_ca_keypair(&pki_d)?;
        call_pod_method(
            &peer.peer_addr,
            peer.peer_port,
            "pod/push-ca-key",
            serde_json::json!({ "cert_pem": cert_pem, "key_pem": key_pem }),
        )
        .await?;
        println!("✓ pushed CA key to {}", peer.peer_id);
    } else if !i_have_key && peer_has_key {
        println!("  peer has CA key; we don't — they should push to us on their side");
    }
    Ok(())
}

// ── pod self-secure ──────────────────────────────────────────────────────────

pub fn cmd_pod_self_secure(action: SelfSecureAction) -> Result<()> {
    let conn = db::open_default()?;
    match action {
        SelfSecureAction::Show => {
            let v = pdb::get_self_secure(&conn)?;
            println!("self_secure: {v}");
        }
        SelfSecureAction::On => {
            pdb::set_self_secure(&conn, true)?;
            println!("✓ self_secure: true (secrets writes enabled)");
        }
        SelfSecureAction::Off => {
            pdb::set_self_secure(&conn, false)?;
            println!("✓ self_secure: false (secrets writes will be refused)");
        }
    }
    Ok(())
}

pub enum SelfSecureAction {
    On,
    Off,
    Show,
}

// ── pod leave ────────────────────────────────────────────────────────────────

pub async fn cmd_pod_leave(wipe_secrets: bool, wipe_all: bool) -> Result<()> {
    let conn = db::open_default()?;
    let peers = pdb::list_peers(&conn)?;

    // Best-effort: notify peers we're leaving. Fire-and-forget per peer.
    for p in &peers {
        if p.departed_at.is_some() {
            continue;
        }
        if let Err(e) = call_pod_method(
            &p.peer_addr,
            p.peer_port,
            "pod/peer-leaving",
            serde_json::json!({}),
        )
        .await
        {
            println!("  warning: could not notify {} ({e})", p.peer_id);
        }
    }

    // Local wipe of pod membership state.
    pdb::wipe_pod_membership(&conn)?;

    // Optional secret/data wipes.
    if wipe_secrets || wipe_all {
        conn.execute("DELETE FROM secrets", [])?;
        println!("✓ wiped secrets table");
    }
    if wipe_all {
        for tbl in [
            "plugin_data",
            "plugin_credentials",
            "oauth_tokens",
            "profile_credentials",
        ] {
            let _ = conn.execute(&format!("DELETE FROM {tbl}"), []);
        }
        println!("✓ wiped plugin_data, plugin_credentials, oauth_tokens, profile_credentials");
    }

    // Remove mesh PKI material. Bootstrap key stays (host identity persists
    // across pod re-joins).
    let pki_d = pki_dir();
    let mesh = pki::mesh_dir(&pki_d);
    if mesh.exists() {
        let _ = std::fs::remove_dir_all(&mesh);
        println!("✓ removed mesh PKI material at {}", mesh.display());
    }

    println!("✓ left the pod. This host can re-join via auto-discovery.");
    Ok(())
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn local_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Dial a paired peer with our mesh client cert. Used by post-join methods
/// (notify-trust, has-ca-key, push-ca-key, peer-leaving).
async fn call_pod_method(
    host: &str,
    port: u16,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let pki_d = pki_dir();
    let bundle = pki::load_mesh_client(&pki_d)
        .context("load mesh client bundle (this host is not a pod member)")?;
    let (chain, key) = pki::parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem)?;
    let roots = pki::ca_root_store(&bundle.ca_cert_pem)?;
    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)?;
    dial_pod_mtls(host, port, client_config, method, params).await
}

async fn dial_pod_mtls(
    host: &str,
    port: u16,
    client_config: ClientConfig,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let connector = TlsConnector::from(Arc::new(client_config));
    let target = format!("{host}:{port}");
    let tcp = TcpStream::connect(&target)
        .await
        .with_context(|| format!("connect {target}"))?;
    let sni = ServerName::try_from(pki::POD_SERVER_SAN)?.to_owned();
    let mut tls = connector.connect(sni, tcp).await.context("TLS handshake")?;
    write_frame(
        &mut tls,
        &serde_json::to_vec(&Request::new(1, method, Some(params)))?,
    )
    .await?;
    let raw = tokio::time::timeout(Duration::from_secs(15), read_frame(&mut tls))
        .await
        .context("response timed out")??;
    parse_resp(&raw)
}

/// Dial a peer over the bootstrap SNI with a pinned pubkey. Used by `pod
/// accept` (join-confirm) and by the auto-offer scheduler (offer push).
async fn dial_bootstrap(
    host: &str,
    port: u16,
    pinned_fp: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let verifier = pki::pinned_bootstrap_verifier(pinned_fp.to_string());
    let client_config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));
    let target = format!("{host}:{port}");
    let tcp = TcpStream::connect(&target)
        .await
        .with_context(|| format!("connect {target}"))?;
    let sni = ServerName::try_from(pki::POD_BOOTSTRAP_SAN)?.to_owned();
    let mut tls = connector
        .connect(sni, tcp)
        .await
        .context("bootstrap TLS handshake (pubkey pin mismatch?)")?;
    write_frame(
        &mut tls,
        &serde_json::to_vec(&Request::new(1, method, Some(params)))?,
    )
    .await?;
    let raw = tokio::time::timeout(Duration::from_secs(15), read_frame(&mut tls))
        .await
        .context("response timed out")??;
    parse_resp(&raw)
}

fn parse_resp(raw: &[u8]) -> Result<serde_json::Value> {
    let msg: Message = serde_json::from_slice(raw)?;
    let resp: Response = match msg {
        Message::Response(r) => r,
        _ => bail!("unexpected message type"),
    };
    if let Some(err) = resp.error {
        bail!("peer returned error: {}", err.message);
    }
    resp.result.context("response had no result")
}
