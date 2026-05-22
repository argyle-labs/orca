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
const POD_REQUEST_OFFER_METHOD: &str = "pod/request-offer";

/// Joiner → inviter, sent over an unauthenticated bootstrap TLS session (the
/// joiner doesn't know the inviter's fp yet — TOFU). The inviter responds
/// with a `RequestOfferResult` carrying the full signed `pod/offer` payload
/// the joiner would normally have received via the inviter's auto-offer push.
///
/// `joiner_pubkey_fp` lets the inviter pin the joiner's bootstrap pubkey for
/// the matching `pod/join-confirm` step that follows, without having to
/// receive an mDNS broadcast first.
#[derive(Debug, Serialize, Deserialize)]
struct RequestOfferBody {
    joiner_peer_id: String,
    joiner_hostname: String,
    joiner_pubkey_fp: String,
    /// Optional human-readable hostname for the inviter's discovery row.
    #[serde(default)]
    joiner_display_name: Option<String>,
}

/// Response to `pod/request-offer`. Returns the same `code_hint` shape as
/// `pod/offer` plus the raw fields the joiner needs to land an inbound
/// pending-offer row. The pairing code itself is NOT included — it's printed
/// on the inviter's CLI per `project_pod_join_ux.md` so the user types it
/// into `pod accept`.
#[derive(Debug, Serialize, Deserialize)]
struct RequestOfferResult {
    /// Inviter's bootstrap-key fp the joiner just spoke to (TOFU echo so the
    /// joiner can record it).
    inviter_pubkey_fp: String,
    inviter_peer_id: String,
    inviter_hostname: String,
    inviter_addr: String,
    inviter_port: u16,
    mesh_ca_cert_pem: String,
    pod_id: String,
    code_hash: String,
    expires_at: i64,
    #[serde(default)]
    inviter_display_name: Option<String>,
    /// First 2 chars of the pairing code so the joiner can confirm visually
    /// the inviter is the one that printed the matching prefix on its CLI.
    #[serde(default)]
    code_hint: Option<String>,
}

/// Signed payload pushed by the inviter. The signing key's fp identifies the
/// inviter; the joiner cross-checks it against the mDNS-advertised fp before
/// surfacing the offer.
#[derive(Debug, Serialize, Deserialize)]
struct OfferBody {
    inviter_peer_id: String,
    /// On the wire this is the inviter's stable identity label (today =
    /// `machine_id_short`). Kept named `inviter_hostname` for wire compat
    /// with rc.≤24 daemons; new field `inviter_display_name` carries the
    /// human-readable hostname.
    inviter_hostname: String,
    inviter_addr: String,
    inviter_port: u16,
    mesh_ca_cert_pem: String,
    pod_id: String,
    code_hash: String,
    expires_at: i64,
    /// Human-readable hostname (slice 7). Optional + serde(default) so an
    /// rc.25 daemon can parse an rc.24 OfferBody that omits the field.
    #[serde(default)]
    inviter_display_name: Option<String>,
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
    /// Cert CN material — stable `machine_id_short` of the joiner. The
    /// name `joiner_hostname` is kept for wire compat with rc.≤24
    /// daemons (which conflated CN with hostname); new field
    /// `joiner_display_name` carries the human label.
    joiner_hostname: String,
    csr_client_pem: String,
    csr_server_pem: String,
    /// Human-readable hostname (slice 7). Optional + serde(default) for
    /// rc.24 wire compat.
    #[serde(default)]
    joiner_display_name: Option<String>,
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
        POD_REQUEST_OFFER_METHOD => match handle_request_offer(&env, peer) {
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
    let offer_id = Uuid::now_v7().to_string();
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
    let inviter_label =
        select_peer_label(&body.inviter_hostname, body.inviter_display_name.as_deref());
    pdb::insert_pending_offer(
        &conn,
        &offer_id,
        "in",
        &signer_fp,
        inviter_label,
        inviter_addr,
        body.inviter_port,
        &body.code_hash,
        Some(&body.mesh_ca_cert_pem),
        Some(&body.inviter_peer_id),
        Some(&body.pod_id),
        ttl,
    )?;
    info!(
        "[pod-bootstrap] received offer from {} ({}, {}@{}:{}); run `orca pod pending` to view",
        body.inviter_hostname, body.inviter_peer_id, signer_fp, inviter_addr, body.inviter_port
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
    let peer_label = select_peer_label(&body.joiner_hostname, body.joiner_display_name.as_deref());
    pdb::upsert_peer(
        &conn,
        &joiner_peer_id,
        peer_label,
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

/// Joiner-initiated handshake (Slice JU-3). Joiner calls this over TOFU TLS
/// asking "please offer me membership". We treat the request like an mDNS
/// discovery hit: record the joiner in `pod_discovery`, mint a pairing code,
/// insert an outbound pending offer keyed by `joiner_pubkey_fp`, and return
/// the offer details so the joiner can land an inbound pending row in the
/// same round-trip.
fn handle_request_offer(
    env: &SignedEnvelope,
    peer: std::net::SocketAddr,
) -> Result<RequestOfferResult> {
    let (body, signer_vk) = pki::verify_envelope::<RequestOfferBody>(env)?;
    let signer_fp = pki::bootstrap_pubkey_fingerprint(&signer_vk);
    // Envelope-signer must match the fp the joiner advertises. Otherwise any
    // signer could request offers for an arbitrary fp.
    if signer_fp != body.joiner_pubkey_fp {
        anyhow::bail!(
            "envelope signer fp {} does not match advertised joiner_pubkey_fp {}",
            signer_fp,
            body.joiner_pubkey_fp
        );
    }

    let conn = db::open_default()?;
    // Inviter must already be a pod member (have a mesh CA) to invite peers.
    let pki_d = pki_dir();
    let mesh_ca_cert_pem = std::fs::read_to_string(pki::mesh_ca_cert_path(&pki_d))
        .context("this host has no mesh CA; run `orca pod init` first")?;
    let pod_id = pdb::get_pod_id(&conn)?.unwrap_or_else(|| "default".to_string());

    // Record the joiner in discovery (idempotent — same fp = same row).
    let joiner_label =
        select_peer_label(&body.joiner_hostname, body.joiner_display_name.as_deref());
    pdb::upsert_discovery(
        &conn,
        &body.joiner_pubkey_fp,
        Some(&body.joiner_peer_id),
        joiner_label,
        &peer.ip().to_string(),
        peer.port(),
        "unclaimed",
        true,
    )?;

    if pdb::has_open_outbound_offer(&conn, &body.joiner_pubkey_fp)? {
        anyhow::bail!(
            "an outbound offer to {} is already pending — try `pod accept` with the existing code",
            joiner_label
        );
    }

    let code = crate::pod::scheduler::mint_pairing_code();
    let code_hash = pdb::hash_code(&code);
    let offer_id = Uuid::now_v7().to_string();
    let expires_at = now_secs() + crate::pod::scheduler::OFFER_TTL_SECS;
    pdb::insert_pending_offer(
        &conn,
        &offer_id,
        "out",
        &body.joiner_pubkey_fp,
        joiner_label,
        &peer.ip().to_string(),
        peer.port(),
        &code_hash,
        None,
        None,
        None,
        crate::pod::scheduler::OFFER_TTL_SECS,
    )?;

    // Print code on the inviter side so a watching operator can read it.
    // Matches the auto-offer scheduler's behavior.
    info!(
        "[pod-bootstrap] joiner-initiated request from {} ({}, fp {}) — pairing code: {code}",
        joiner_label, body.joiner_peer_id, body.joiner_pubkey_fp
    );

    let signing = pki::load_or_init_bootstrap_key(&pki_d)?;
    let inviter_fp = pki::bootstrap_pubkey_fingerprint(&signing.verifying_key());
    let inviter_hostname = crate::host_identity::hostname().to_string();
    let inviter_display_name = crate::host_identity::display_hostname().to_string();
    let inviter_peer_id = format!("peer.{}", crate::host_identity::machine_id_short());

    Ok(RequestOfferResult {
        inviter_pubkey_fp: inviter_fp,
        inviter_peer_id,
        inviter_hostname,
        inviter_addr: String::new(), // joiner already knows our addr — it dialed us
        inviter_port: orca_utils::config::APP_PLUGIN_PORT,
        mesh_ca_cert_pem,
        pod_id,
        code_hash,
        expires_at,
        inviter_display_name: Some(inviter_display_name),
        code_hint: Some(code.chars().take(2).collect()),
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

/// Pick the human-readable label to store in `pod_peers.peer_hostname` (or
/// `pending_offers.peer_hostname`) for a peer that's announcing itself.
///
/// rc.25+ peers send both an identity CN (`*_hostname` = `machine_id_short`)
/// and an optional `*_display_name`. We prefer the display name when present
/// and non-blank; otherwise fall back to the CN so rc.≤24 peers don't go
/// nameless mid-rollout.
fn select_peer_label<'a>(cn_hostname: &'a str, display_name: Option<&'a str>) -> &'a str {
    match display_name {
        Some(s) if !s.trim().is_empty() => s,
        _ => cn_hostname,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_peer_label_prefers_display_name() {
        assert_eq!(select_peer_label("abc123", Some("thor")), "thor");
    }

    #[test]
    fn select_peer_label_falls_back_when_none() {
        assert_eq!(select_peer_label("abc123", None), "abc123");
    }

    #[test]
    fn select_peer_label_falls_back_when_blank() {
        assert_eq!(select_peer_label("abc123", Some("")), "abc123");
        assert_eq!(select_peer_label("abc123", Some("   ")), "abc123");
    }

    #[test]
    fn offer_body_deserializes_rc24_without_display_name() {
        let json = serde_json::json!({
            "inviter_peer_id": "peer.abc",
            "inviter_hostname": "abc123",
            "inviter_addr": "10.0.0.1",
            "inviter_port": 12002,
            "mesh_ca_cert_pem": "",
            "pod_id": "p1",
            "code_hash": "h",
            "expires_at": 0,
        });
        let body: OfferBody = serde_json::from_value(json).unwrap();
        assert_eq!(body.inviter_hostname, "abc123");
        assert!(body.inviter_display_name.is_none());
    }

    #[test]
    fn offer_body_roundtrip_rc25_with_display_name() {
        let json = serde_json::json!({
            "inviter_peer_id": "peer.abc",
            "inviter_hostname": "abc123",
            "inviter_addr": "10.0.0.1",
            "inviter_port": 12002,
            "mesh_ca_cert_pem": "",
            "pod_id": "p1",
            "code_hash": "h",
            "expires_at": 0,
            "inviter_display_name": "thor",
        });
        let body: OfferBody = serde_json::from_value(json).unwrap();
        assert_eq!(body.inviter_display_name.as_deref(), Some("thor"));
    }

    #[test]
    fn join_confirm_body_deserializes_rc24_without_display_name() {
        let json = serde_json::json!({
            "code": "ABC123",
            "joiner_hostname": "xyz789",
            "csr_client_pem": "",
            "csr_server_pem": "",
        });
        let body: JoinConfirmBody = serde_json::from_value(json).unwrap();
        assert_eq!(body.joiner_hostname, "xyz789");
        assert!(body.joiner_display_name.is_none());
    }

    #[test]
    fn request_offer_body_roundtrip() {
        let json = serde_json::json!({
            "joiner_peer_id": "peer.abc",
            "joiner_hostname": "abc123",
            "joiner_pubkey_fp": "fp-deadbeef",
            "joiner_display_name": "loki",
        });
        let body: RequestOfferBody = serde_json::from_value(json).unwrap();
        assert_eq!(body.joiner_pubkey_fp, "fp-deadbeef");
        assert_eq!(body.joiner_display_name.as_deref(), Some("loki"));
    }

    #[test]
    fn request_offer_body_optional_display_name() {
        let json = serde_json::json!({
            "joiner_peer_id": "peer.abc",
            "joiner_hostname": "abc123",
            "joiner_pubkey_fp": "fp-deadbeef",
        });
        let body: RequestOfferBody = serde_json::from_value(json).unwrap();
        assert!(body.joiner_display_name.is_none());
    }

    #[test]
    fn request_offer_result_roundtrip() {
        let r = RequestOfferResult {
            inviter_pubkey_fp: "fp-inviter".into(),
            inviter_peer_id: "peer.thor".into(),
            inviter_hostname: "thor".into(),
            inviter_addr: String::new(),
            inviter_port: 12002,
            mesh_ca_cert_pem: "ca".into(),
            pod_id: "p1".into(),
            code_hash: "h".into(),
            expires_at: 1234,
            inviter_display_name: Some("thor.local".into()),
            code_hint: Some("AB".into()),
        };
        let v = serde_json::to_value(&r).unwrap();
        let back: RequestOfferResult = serde_json::from_value(v).unwrap();
        assert_eq!(back.inviter_pubkey_fp, "fp-inviter");
        assert_eq!(back.code_hint.as_deref(), Some("AB"));
        assert_eq!(back.expires_at, 1234);
    }

    #[test]
    fn join_confirm_body_roundtrip_rc25_with_display_name() {
        let json = serde_json::json!({
            "code": "ABC123",
            "joiner_hostname": "xyz789",
            "csr_client_pem": "",
            "csr_server_pem": "",
            "joiner_display_name": "loki",
        });
        let body: JoinConfirmBody = serde_json::from_value(json).unwrap();
        assert_eq!(body.joiner_display_name.as_deref(), Some("loki"));
    }
}
