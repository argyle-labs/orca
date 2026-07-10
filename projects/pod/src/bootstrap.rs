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
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_rustls::server::TlsStream;
use tracing::{info, warn};
use utils::framing::{read_frame, write_frame};
use utils::jsonrpc::{ErrorObject, Message, Request, Response};
use utils::pki::PeerRole;
use utils::pki::SignedEnvelope;

use super::pki_dir;
use db::pod as pdb;

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
    #[serde(default)]
    code_hint: Option<String>,
    /// Plaintext pairing code — included when both sides are mDNS-verified LAN
    /// peers so the joiner can auto-accept without out-of-band code entry.
    #[serde(default)]
    code_plain: Option<String>,
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
    /// Plaintext pairing code — included by rc.12+ inviters on mDNS-verified
    /// LAN peers so the joiner can auto-accept without out-of-band code entry.
    #[serde(default)]
    code_plain: Option<String>,
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

    let (response, auto_accept_code) = dispatch(request, peer).await;
    let envelope = serde_json::to_vec(&response).context("serialize bootstrap response")?;
    write_frame(&mut tls, &envelope)
        .await
        .context("write bootstrap response")?;

    // Ack is on the wire — now safe to dial back for auto-accept.
    if let Some(code) = auto_accept_code {
        tokio::spawn(async move {
            if let Err(e) = crate::cli::cmd_pod_accept(&code).await {
                warn!("[pod-bootstrap] auto-accept failed: {e:#}");
            } else {
                info!("[pod-bootstrap] auto-accept succeeded");
            }
        });
    }

    Ok(())
}

async fn dispatch(request: Request, peer: std::net::SocketAddr) -> (Response, Option<String>) {
    let id = request.id.clone();
    let method = request.method.as_str();

    let env: SignedEnvelope = match request.params {
        Some(v) => match serde_json::from_value(v) {
            Ok(e) => e,
            Err(e) => {
                return (
                    Response::err(
                        id,
                        ErrorObject::internal(&format!("parse signed envelope: {e}")),
                    ),
                    None,
                );
            }
        },
        None => {
            return (
                Response::err(
                    id,
                    ErrorObject::internal("bootstrap requires signed params"),
                ),
                None,
            );
        }
    };

    match method {
        POD_OFFER_METHOD => match handle_offer(&env, peer) {
            Ok((ack, auto_accept_code)) => (value_response(id, &ack), auto_accept_code),
            Err(e) => (
                Response::err(id, ErrorObject::internal(&e.to_string())),
                None,
            ),
        },
        POD_JOIN_CONFIRM_METHOD => match handle_join_confirm(&env) {
            Ok(r) => (value_response(id, &r), None),
            Err(e) => (
                Response::err(id, ErrorObject::internal(&e.to_string())),
                None,
            ),
        },
        POD_REQUEST_OFFER_METHOD => match handle_request_offer(&env, peer) {
            Ok(r) => (value_response(id, &r), None),
            Err(e) => (
                Response::err(id, ErrorObject::internal(&e.to_string())),
                None,
            ),
        },
        other => (
            Response::err(
                id,
                ErrorObject::method_not_found(&format!("bootstrap method '{other}' not supported")),
            ),
            None,
        ),
    }
}

fn handle_offer(
    env: &SignedEnvelope,
    peer: std::net::SocketAddr,
) -> Result<(OfferAck, Option<String>)> {
    let (body, signer_vk) = utils::pki::verify_envelope::<OfferBody>(env)?;
    let signer_fp = utils::pki::bootstrap_pubkey_fingerprint(&signer_vk);

    let conn = db::open_default()?;
    let offer_id = utils::id::new();
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
        body.code_plain.as_deref(),
    )?;
    let auto_accept_code = body.code_plain.clone();
    if auto_accept_code.is_some() {
        info!(
            "[pod-bootstrap] received auto-pair offer from {} ({}@{}:{}) — accepting",
            body.inviter_hostname, body.inviter_peer_id, inviter_addr, body.inviter_port
        );
    } else {
        info!(
            "[pod-bootstrap] received offer from {} ({}, {}@{}:{}); run `orca pod pending` to view",
            body.inviter_hostname, body.inviter_peer_id, signer_fp, inviter_addr, body.inviter_port
        );
    }
    Ok((OfferAck { code_hint: None }, auto_accept_code))
}

fn handle_join_confirm(env: &SignedEnvelope) -> Result<JoinConfirmResult> {
    let (body, signer_vk) = utils::pki::verify_envelope::<JoinConfirmBody>(env)?;
    let signer_fp = utils::pki::bootstrap_pubkey_fingerprint(&signer_vk);

    let conn = db::open_default()?;
    let offer = pdb::find_outbound_offer_by_code_and_fp(&conn, &body.code, &signer_fp)?
        .context("no matching pending outbound offer (wrong code, wrong peer, or expired)")?;

    let pki_d = pki_dir();
    let (client_cert_pem, ca_cert_pem) = utils::pki::sign_peer_csr(
        &pki_d,
        &body.csr_client_pem,
        &body.joiner_hostname,
        PeerRole::Client,
    )?;
    let (server_cert_pem, _) = utils::pki::sign_peer_csr(
        &pki_d,
        &body.csr_server_pem,
        &body.joiner_hostname,
        PeerRole::Server,
    )?;

    // `joiner_hostname` IS the joiner's machine_id_short — the field name
    // is misleading wire-compat (see struct docstring + feedback_no_id_prefixes).
    let joiner_peer_id = body.joiner_hostname.clone();
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
    // The inviter just chose to sign this joiner's CSR — that IS the local
    // trust signal. Without this the trust flag stays false even after a
    // successful pairing, which blocks every downstream mutual-trust gate
    // (CA-key replication, secrets sync).
    pdb::set_trust(&conn, &joiner_peer_id, Some(true), None)?;
    // Drop any legacy `"unknown"` stub that points at the same joiner. These
    // were materialized by `ensure_peer_stub` for pre-rc.25 mTLS clients
    // whose CN was literally the string `"unknown"`; they're dead weight
    // once the real peer_id row exists at the same address.
    pdb::cleanup_unknown_stub_at(&conn, &offer.peer_addr)?;
    pdb::delete_pending_offer(&conn, &offer.offer_id)?;

    // Defensive: any pending offer that survived migration without an
    // inviter_peer_id field should still resolve to *this host's* identity,
    // not the string "unknown". The offer is on OUR side; we know who we are.
    let inviter_peer_id = offer
        .inviter_peer_id
        .clone()
        .unwrap_or_else(|| system::host_identity::machine_id_short().to_string());
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
    let (body, signer_vk) = utils::pki::verify_envelope::<RequestOfferBody>(env)?;
    let signer_fp = utils::pki::bootstrap_pubkey_fingerprint(&signer_vk);
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
    let mesh_ca_cert_pem = std::fs::read_to_string(utils::pki::mesh_ca_cert_path(&pki_d))
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

    let code = crate::scheduler::mint_pairing_code();
    let code_hash = pdb::hash_code(&code);
    let offer_id = utils::id::new();
    let expires_at = now_secs() + crate::scheduler::OFFER_TTL_SECS;
    // Persist the inviter's own peer_id on the pending offer so the matching
    // `pod/join-confirm` step can echo it back to the joiner. Without this
    // the joiner records the inviter as `"unknown"` and roster-sync skips
    // every row that references it.
    let inviter_peer_id = system::host_identity::machine_id_short().to_string();
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
        Some(&inviter_peer_id),
        None,
        crate::scheduler::OFFER_TTL_SECS,
        None,
    )?;

    // Print code on the inviter side so a watching operator can read it.
    // Matches the auto-offer scheduler's behavior.
    info!(
        "[pod-bootstrap] joiner-initiated request from {} ({}, fp {}) — pairing code: {code}",
        joiner_label, body.joiner_peer_id, body.joiner_pubkey_fp
    );

    let signing = utils::pki::load_or_init_bootstrap_key(&pki_d)?;
    let inviter_fp = utils::pki::bootstrap_pubkey_fingerprint(&signing.verifying_key());
    let inviter_hostname = system::host_identity::hostname().to_string();
    let inviter_display_name = system::host_identity::display_hostname().to_string();

    Ok(RequestOfferResult {
        inviter_pubkey_fp: inviter_fp,
        inviter_peer_id,
        inviter_hostname,
        inviter_addr: String::new(), // joiner already knows our addr — it dialed us
        inviter_port: db::ports::mesh_port(),
        mesh_ca_cert_pem,
        pod_id,
        code_hash,
        expires_at,
        inviter_display_name: Some(inviter_display_name),
        code_hint: Some(code.chars().take(2).collect()),
        // S1: ship the plaintext code alongside the offer so `pod join`
        // can finish in one command. Authenticity is already covered by
        // the TOFU pubkey pin + signed-envelope echo the joiner verifies;
        // the code's prior role was only operator transcription. Keeping
        // `code_hint` populated so manual `pod accept` still works for
        // out-of-band flows.
        code_plain: Some(code.clone()),
    })
}

fn value_response<T: Serialize>(id: Value, v: &T) -> Response {
    match serde_json::to_value(v) {
        Ok(val) => Response::ok(id, val),
        Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
    }
}

use utils::time::now_secs_since_epoch as now_secs;

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
        assert_eq!(select_peer_label("abc123", Some("host-g")), "host-g");
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
            "inviter_peer_id": "abc",
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
            "inviter_peer_id": "abc",
            "inviter_hostname": "abc123",
            "inviter_addr": "10.0.0.1",
            "inviter_port": 12002,
            "mesh_ca_cert_pem": "",
            "pod_id": "p1",
            "code_hash": "h",
            "expires_at": 0,
            "inviter_display_name": "host-g",
        });
        let body: OfferBody = serde_json::from_value(json).unwrap();
        assert_eq!(body.inviter_display_name.as_deref(), Some("host-g"));
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
            "joiner_peer_id": "abc",
            "joiner_hostname": "abc123",
            "joiner_pubkey_fp": "fp-deadbeef",
            "joiner_display_name": "host-h",
        });
        let body: RequestOfferBody = serde_json::from_value(json).unwrap();
        assert_eq!(body.joiner_pubkey_fp, "fp-deadbeef");
        assert_eq!(body.joiner_display_name.as_deref(), Some("host-h"));
    }

    #[test]
    fn request_offer_body_optional_display_name() {
        let json = serde_json::json!({
            "joiner_peer_id": "abc",
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
            inviter_peer_id: "host-g".into(),
            inviter_hostname: "host-g".into(),
            inviter_addr: String::new(),
            inviter_port: 12002,
            mesh_ca_cert_pem: "ca".into(),
            pod_id: "p1".into(),
            code_hash: "h".into(),
            expires_at: 1234,
            inviter_display_name: Some("host-g.local".into()),
            code_hint: Some("AB".into()),
            code_plain: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        let back: RequestOfferResult = serde_json::from_value(v).unwrap();
        assert_eq!(back.inviter_pubkey_fp, "fp-inviter");
        assert_eq!(back.code_hint.as_deref(), Some("AB"));
        assert_eq!(back.expires_at, 1234);
        assert!(back.code_plain.is_none());
    }

    #[test]
    fn join_confirm_body_roundtrip_rc25_with_display_name() {
        let json = serde_json::json!({
            "code": "ABC123",
            "joiner_hostname": "xyz789",
            "csr_client_pem": "",
            "csr_server_pem": "",
            "joiner_display_name": "host-h",
        });
        let body: JoinConfirmBody = serde_json::from_value(json).unwrap();
        assert_eq!(body.joiner_display_name.as_deref(), Some("host-h"));
    }

    #[test]
    fn offer_body_deserializes_rc11_without_code_plain() {
        // rc.≤11 inviters don't send code_plain; must default to None.
        let json = serde_json::json!({
            "inviter_peer_id": "abc",
            "inviter_hostname": "abc123",
            "inviter_addr": "10.0.0.1",
            "inviter_port": 12002,
            "mesh_ca_cert_pem": "",
            "pod_id": "p1",
            "code_hash": "h",
            "expires_at": 0,
        });
        let body: OfferBody = serde_json::from_value(json).unwrap();
        assert!(body.code_plain.is_none());
    }

    #[test]
    fn offer_body_deserializes_rc12_with_code_plain() {
        let json = serde_json::json!({
            "inviter_peer_id": "abc",
            "inviter_hostname": "abc123",
            "inviter_addr": "10.0.0.1",
            "inviter_port": 12002,
            "mesh_ca_cert_pem": "",
            "pod_id": "p1",
            "code_hash": "h",
            "expires_at": 0,
            "code_plain": "ABCDEF",
        });
        let body: OfferBody = serde_json::from_value(json).unwrap();
        assert_eq!(body.code_plain.as_deref(), Some("ABCDEF"));
    }

    #[test]
    fn request_offer_result_code_plain_defaults_none() {
        // Older inviters omit code_plain; must not break deserialization.
        let json = serde_json::json!({
            "inviter_pubkey_fp": "fp",
            "inviter_peer_id": "x",
            "inviter_hostname": "x",
            "inviter_addr": "",
            "inviter_port": 12002,
            "mesh_ca_cert_pem": "",
            "pod_id": "p",
            "code_hash": "h",
            "expires_at": 0,
        });
        let r: RequestOfferResult = serde_json::from_value(json).unwrap();
        assert!(r.code_plain.is_none());
        assert!(r.code_hint.is_none());
    }

    #[test]
    fn request_offer_result_roundtrip_with_code_plain() {
        let r = RequestOfferResult {
            inviter_pubkey_fp: "fp".into(),
            inviter_peer_id: "x".into(),
            inviter_hostname: "x".into(),
            inviter_addr: String::new(),
            inviter_port: 12002,
            mesh_ca_cert_pem: String::new(),
            pod_id: "p".into(),
            code_hash: "h".into(),
            expires_at: 0,
            inviter_display_name: None,
            code_hint: Some("AB".into()),
            code_plain: Some("ABCDEF".into()),
        };
        let v = serde_json::to_value(&r).unwrap();
        let back: RequestOfferResult = serde_json::from_value(v).unwrap();
        assert_eq!(back.code_plain.as_deref(), Some("ABCDEF"));
    }
}
