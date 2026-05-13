// JSON-RPC envelopes are inherently opaque at the wire boundary; mirroring
// the allow in projects/sdk/rust/src/jsonrpc.rs.
#![allow(clippy::disallowed_types)]

//! CLI handlers for `orca pod {invite,join,list,trust,self-secure}`.
//!
//! Init + ping live in main.rs / pod::ping; everything DB-touching or
//! protocol-issuing lives here.

use anyhow::{Context, Result, bail};
use base64::Engine;
use orca_sdk::framing::{read_frame, write_frame};
use orca_sdk::jsonrpc::{Message, Request, Response};
use orca_sdk::pki::{self, PeerRole};
use orca_utils::config::APP_PLUGIN_PORT;
use rand::Rng;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::pod::{db as pdb, pki_dir};

#[derive(Debug, Serialize, Deserialize)]
struct InviteBlob {
    host: String,
    port: u16,
    token: String,
    ca_cert_pem: String,
    issuer_cn: String,
}

// ── pod invite ───────────────────────────────────────────────────────────────

pub fn cmd_pod_invite(ttl_secs: i64) -> Result<()> {
    let pki = pki_dir();
    if !pki::has_mesh_ca_key(&pki) {
        bail!("this host doesn't have the mesh CA key — only secure hosts can issue invites");
    }
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let token = raw.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    });
    let token_hash = pdb::hash_token(&token);

    let host = local_hostname();
    let issuer_cn = format!("peer.{host}");

    let conn = db::open_default()?;
    pdb::insert_invite(&conn, &token_hash, ttl_secs, &issuer_cn)?;

    let ca_cert_pem =
        std::fs::read_to_string(pki::mesh_ca_cert_path(&pki)).context("read mesh CA cert")?;
    let blob = InviteBlob {
        host,
        port: APP_PLUGIN_PORT,
        token,
        ca_cert_pem,
        issuer_cn,
    };
    let json = serde_json::to_vec(&blob)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&json);
    println!("ORCA_POD_INVITE {encoded}");
    println!("(expires in {ttl_secs}s)");
    Ok(())
}

// ── pod join ─────────────────────────────────────────────────────────────────

pub async fn cmd_pod_join(blob: &str) -> Result<()> {
    let blob = blob.strip_prefix("ORCA_POD_INVITE ").unwrap_or(blob).trim();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(blob)
        .context("invite blob is not valid base64")?;
    let invite: InviteBlob = serde_json::from_slice(&raw).context("invite blob JSON malformed")?;

    let pki = pki_dir();
    std::fs::create_dir_all(pki::mesh_dir(&pki))?;
    // Plant the CA cert locally so the connector trusts the founder.
    std::fs::write(pki::mesh_ca_cert_path(&pki), invite.ca_cert_pem.as_bytes())?;

    let host = local_hostname();
    let (csr_client_pem, client_key_pem) = pki::build_peer_csr(&host, PeerRole::Client)?;
    let (csr_server_pem, server_key_pem) = pki::build_peer_csr(&host, PeerRole::Server)?;

    let params = serde_json::json!({
        "token": invite.token,
        "joiner_hostname": host,
        "csr_client_pem": csr_client_pem,
        "csr_server_pem": csr_server_pem,
    });
    let result = call_pod_method_no_cert(
        &invite.host,
        invite.port,
        "pod/join",
        params,
        &invite.ca_cert_pem,
    )
    .await
    .context("pod/join over mTLS-without-client-cert failed")?;

    let client_cert_pem = result
        .get("client_cert_pem")
        .and_then(|v| v.as_str())
        .context("response missing client_cert_pem")?;
    let server_cert_pem = result
        .get("server_cert_pem")
        .and_then(|v| v.as_str())
        .context("response missing server_cert_pem")?;

    // Persist signed certs alongside the locally-generated keys.
    let server_dir = pki::mesh_dir(&pki).join("server");
    let client_dir = pki::mesh_dir(&pki).join("client");
    std::fs::create_dir_all(&server_dir)?;
    std::fs::create_dir_all(&client_dir)?;
    std::fs::write(pki::mesh_server_cert_path(&pki), server_cert_pem)?;
    std::fs::write(pki::mesh_server_key_path(&pki), server_key_pem)?;
    std::fs::write(pki::mesh_client_cert_path(&pki), client_cert_pem)?;
    std::fs::write(pki::mesh_client_key_path(&pki), client_key_pem)?;

    // Joiner is non-secure by default: pod_self row created with self_secure=0
    // so the secrets gate engages until the user runs `pod self-secure on`.
    let conn = db::open_default()?;
    pdb::set_self_secure(&conn, false)?;

    println!(
        "✓ joined pod via {} (issuer: {})",
        invite.host, invite.issuer_cn
    );
    println!(
        "  client cert: {}",
        pki::mesh_client_cert_path(&pki).display()
    );
    println!(
        "  server cert: {}",
        pki::mesh_server_cert_path(&pki).display()
    );
    Ok(())
}

// ── pod list ─────────────────────────────────────────────────────────────────

pub fn cmd_pod_list() -> Result<()> {
    let conn = db::open_default()?;
    let peers = pdb::list_peers(&conn)?;
    if peers.is_empty() {
        println!("(no pod peers — invite some hosts or `orca pod join` an invite)");
        return Ok(());
    }
    println!(
        "{:<28} {:<16} {:<8} {:<8} last-seen",
        "peer_id", "hostname", "local", "peer"
    );
    for p in peers {
        println!(
            "{:<28} {:<16} {:<8} {:<8} {}",
            p.peer_id, p.peer_hostname, p.local_secure, p.peer_secure, p.last_seen_at
        );
    }
    Ok(())
}

// ── pod trust ────────────────────────────────────────────────────────────────

pub async fn cmd_pod_trust(peer_id: &str, on: bool) -> Result<()> {
    let conn = db::open_default()?;
    let new = pdb::set_trust(&conn, peer_id, Some(on), None)?;
    println!(
        "✓ local trust for {peer_id} → {} (peer side: {})",
        on, new.peer_secure
    );

    // Notify the peer so they can flip their `peer_secure` for us.
    let peer_host = peer_id.strip_prefix("peer.").unwrap_or(peer_id);
    match call_pod_method(
        peer_host,
        "pod/notify-trust",
        serde_json::json!({ "trust": on }),
    )
    .await
    {
        Ok(_) => println!("✓ notified {peer_host}"),
        Err(e) => println!("  warning: notify-trust dial failed ({e}); peer will pick it up later"),
    }

    if pdb::is_mutual_secure(new) {
        println!("→ mutual secure; replicating CA key if needed…");
        if let Err(e) = replicate_ca_key_if_needed(peer_host).await {
            println!("  warning: CA-key replication: {e}");
        }
    }
    Ok(())
}

async fn replicate_ca_key_if_needed(peer_host: &str) -> Result<()> {
    let pki = pki_dir();
    let i_have_key = pki::has_mesh_ca_key(&pki);
    let resp = call_pod_method(peer_host, "pod/has-ca-key", serde_json::json!({})).await?;
    let peer_has_key = resp
        .get("has_key")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if i_have_key && !peer_has_key {
        let (cert_pem, key_pem) = pki::export_mesh_ca_keypair(&pki)?;
        call_pod_method(
            peer_host,
            "pod/push-ca-key",
            serde_json::json!({ "cert_pem": cert_pem, "key_pem": key_pem }),
        )
        .await?;
        println!("✓ pushed CA key to {peer_host}");
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

/// Dial a peer with our mesh client cert and invoke `method`. Used by every
/// post-join pod call (notify-trust, has-ca-key, push-ca-key).
async fn call_pod_method(
    host: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let pki = pki_dir();
    let bundle = pki::load_mesh_client(&pki)
        .context("load mesh client bundle (run `orca pod init` or `pod join`)")?;
    let (chain, key) = pki::parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem)?;
    let roots = Arc::new(pki::ca_root_store(&bundle.ca_cert_pem)?);
    let client_config = ClientConfig::builder()
        .with_root_certificates((*roots).clone())
        .with_client_auth_cert(chain, key)?;
    dial_and_invoke(host, APP_PLUGIN_PORT, client_config, method, params).await
}

/// Same as `call_pod_method` but without a client cert — used only by the
/// joiner during pod/join bootstrap. Trust is anchored on the caller-supplied
/// CA cert PEM (from the invite blob).
async fn call_pod_method_no_cert(
    host: &str,
    port: u16,
    method: &str,
    params: serde_json::Value,
    ca_cert_pem: &str,
) -> Result<serde_json::Value> {
    let roots = pki::ca_root_store(ca_cert_pem)?;
    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    dial_and_invoke(host, port, client_config, method, params).await
}

async fn dial_and_invoke(
    host: &str,
    port: u16,
    client_config: ClientConfig,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let connector = TlsConnector::from(Arc::new(client_config));
    let addr = format!("{host}:{port}");
    let tcp = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let sni = ServerName::try_from(pki::POD_SERVER_SAN)?.to_owned();
    let mut tls = connector.connect(sni, tcp).await.context("TLS handshake")?;

    let req = Request::new(1, method, Some(params));
    let envelope = serde_json::to_vec(&req)?;
    write_frame(&mut tls, &envelope).await?;
    let raw = tokio::time::timeout(Duration::from_secs(15), read_frame(&mut tls))
        .await
        .context("response timed out")?
        .context("read response")?;
    let msg: Message = serde_json::from_slice(&raw)?;
    let resp: Response = match msg {
        Message::Response(r) => r,
        _ => bail!("unexpected message type"),
    };
    if let Some(err) = resp.error {
        bail!("peer returned error: {}", err.message);
    }
    resp.result.context("response had no result")
}
