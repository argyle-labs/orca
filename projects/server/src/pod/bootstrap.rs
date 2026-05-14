// JSON-RPC envelopes are inherently opaque at the wire boundary; mirroring
// the allow in projects/sdk/rust/src/jsonrpc.rs.
#![allow(clippy::disallowed_types)]

//! Server-side handler for SNI=pod-bootstrap.orca.local.
//!
//! Two methods live here, both unauthenticated at the TLS layer and gated by
//! signed-envelope verification at the application layer:
//!
//!   pod/offer        — inviter → joiner. Inviter pushes an offer (mesh CA
//!                      cert, pod id, hashed pairing code, TTL). Joiner stores
//!                      a pending_offer row and surfaces via `orca pod pending`.
//!
//!   pod/join-confirm — joiner → inviter. After the user types `pod accept
//!                      <code>` on the joiner, the joiner dials back here with
//!                      the raw code + CSRs. Inviter looks up the pending
//!                      outbound offer (peer_pubkey_fp from envelope, code_hash
//!                      derived from raw code), verifies, signs CSRs, returns
//!                      the certs.

use anyhow::{Context, Result};
use orca_sdk::framing::{read_frame, write_frame};
use orca_sdk::jsonrpc::{ErrorObject, Message, Request, Response};
use orca_sdk::pki::SignedEnvelope;
use orca_sdk::pki::{self, PeerRole};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_rustls::server::TlsStream;
use tracing::{info, warn};
use uuid::Uuid;

use super::{db as pdb, pki_dir};

const POD_OFFER_METHOD: &str = "pod/offer";
const POD_JOIN_CONFIRM_METHOD: &str = "pod/join-confirm";

/// Signed payload pushed by the inviter. The signing key's fp identifies the
/// inviter; the joiner cross-checks it against the mDNS-advertised fp before
/// surfacing the offer.
#[derive(Debug, Serialize, Deserialize)]
struct OfferBody {
    inviter_peer_id: String,
    inviter_hostname: String,
    inviter_addr: String,
    inviter_port: u16,
    mesh_ca_cert_pem: String,
    pod_id: String,
    code_hash: String,
    expires_at: i64,
}

#[derive(Debug, Serialize)]
struct OfferAck {
    /// First few chars of the pairing code that the joiner should display so
    /// the user can confirm visually. (Joiner doesn't know the raw code; it
    /// only has the hash.) Sent as null in v1 — the inviter displays the
    /// code in its own CLI/log output and the user reads from there.
    code_hint: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JoinConfirmBody {
    code: String,
    joiner_hostname: String,
    csr_client_pem: String,
    csr_server_pem: String,
}

#[derive(Debug, Serialize)]
struct JoinConfirmResult {
    client_cert_pem: String,
    server_cert_pem: String,
    ca_cert_pem: String,
    inviter_peer_id: String,
    pod_id: String,
}

pub async fn handle_pod_bootstrap_connection(
    mut tls: TlsStream<tokio::net::TcpStream>,
    peer: std::net::SocketAddr,
) -> Result<()> {
    let frame_bytes = read_frame(&mut tls).await.context("read bootstrap frame")?;
    let msg: Message =
        serde_json::from_slice(&frame_bytes).context("parse bootstrap frame as JSON-RPC")?;
    let request = match msg {
        Message::Request(r) => r,
        Message::Response(_) | Message::Notification(_) => {
            warn!("[pod-bootstrap] non-request frame; closing");
            return Ok(());
        }
    };

    let response = dispatch(request, peer).await;
    let envelope = serde_json::to_vec(&response).context("serialize bootstrap response")?;
    write_frame(&mut tls, &envelope)
        .await
        .context("write bootstrap response")?;
    Ok(())
}

async fn dispatch(request: Request, peer: std::net::SocketAddr) -> Response {
    let id = request.id.clone();
    let method = request.method.as_str();

    let env: SignedEnvelope = match request.params {
        Some(v) => match serde_json::from_value(v) {
            Ok(e) => e,
            Err(e) => {
                return Response::err(
                    id,
                    ErrorObject::internal(&format!("parse signed envelope: {e}")),
                );
            }
        },
        None => {
            return Response::err(
                id,
                ErrorObject::internal("bootstrap requires signed params"),
            );
        }
    };

    match method {
        POD_OFFER_METHOD => match handle_offer(&env, peer) {
            Ok(ack) => value_response(id, &ack),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
        POD_JOIN_CONFIRM_METHOD => match handle_join_confirm(&env) {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
        other => Response::err(
            id,
            ErrorObject::method_not_found(&format!("bootstrap method '{other}' not supported")),
        ),
    }
}

fn handle_offer(env: &SignedEnvelope, peer: std::net::SocketAddr) -> Result<OfferAck> {
    let (body, signer_vk) = pki::verify_envelope::<OfferBody>(env)?;
    let signer_fp = pki::bootstrap_pubkey_fingerprint(&signer_vk);

    let conn = db::open_default()?;
    let offer_id = Uuid::new_v4().to_string();
    let ttl = body.expires_at - now_secs();
    if ttl <= 0 {
        anyhow::bail!("offer already expired");
    }
    // The inviter intentionally does not embed its own routable address in the
    // signed body (it may not know which of its interfaces is reachable from
    // here). Fall back to the TLS source IP, which is by definition reachable.
    let inviter_addr_owned: String;
    let inviter_addr: &str = if body.inviter_addr.is_empty() {
        inviter_addr_owned = peer.ip().to_string();
        &inviter_addr_owned
    } else {
        &body.inviter_addr
    };
    pdb::insert_pending_offer(
        &conn,
        &offer_id,
        "in",
        &signer_fp,
        &body.inviter_hostname,
        inviter_addr,
        body.inviter_port,
        &body.code_hash,
        Some(&body.mesh_ca_cert_pem),
        Some(&body.inviter_peer_id),
        Some(&body.pod_id),
        ttl,
    )?;
    info!(
        "[pod-bootstrap] received offer from {} ({}@{}:{}); run `orca pod pending` to view",
        body.inviter_peer_id, signer_fp, inviter_addr, body.inviter_port
    );
    Ok(OfferAck { code_hint: None })
}

fn handle_join_confirm(env: &SignedEnvelope) -> Result<JoinConfirmResult> {
    let (body, signer_vk) = pki::verify_envelope::<JoinConfirmBody>(env)?;
    let signer_fp = pki::bootstrap_pubkey_fingerprint(&signer_vk);

    let conn = db::open_default()?;
    let offer = pdb::find_outbound_offer_by_code_and_fp(&conn, &body.code, &signer_fp)?
        .context("no matching pending outbound offer (wrong code, wrong peer, or expired)")?;

    let pki_d = pki_dir();
    let (client_cert_pem, ca_cert_pem) = pki::sign_peer_csr(
        &pki_d,
        &body.csr_client_pem,
        &body.joiner_hostname,
        PeerRole::Client,
    )?;
    let (server_cert_pem, _) = pki::sign_peer_csr(
        &pki_d,
        &body.csr_server_pem,
        &body.joiner_hostname,
        PeerRole::Server,
    )?;

    let joiner_peer_id = format!("peer.{}", body.joiner_hostname);
    pdb::upsert_peer(
        &conn,
        &joiner_peer_id,
        &body.joiner_hostname,
        &offer.peer_addr,
        offer.peer_port,
        Some(&signer_fp),
        &ca_cert_pem,
    )?;
    pdb::delete_pending_offer(&conn, &offer.offer_id)?;

    let inviter_peer_id = offer
        .inviter_peer_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let pod_id = offer
        .pod_id
        .clone()
        .unwrap_or_else(|| "default".to_string());

    Ok(JoinConfirmResult {
        client_cert_pem,
        server_cert_pem,
        ca_cert_pem,
        inviter_peer_id,
        pod_id,
    })
}

fn value_response<T: Serialize>(id: Value, v: &T) -> Response {
    match serde_json::to_value(v) {
        Ok(val) => Response::ok(id, val),
        Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
