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
const POD_REFRESH_CERT_BOOTSTRAP_METHOD: &str = "pod/refresh-cert-bootstrap";

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
    /// The inviter's own reachable addresses; the joiner tries each (pinned to
    /// `inviter_pubkey_fp`) for join-confirm. Same fix as the invite path —
    /// robust to the TLS source IP being a tunnel address.
    #[serde(default)]
    inviter_addrs: Vec<String>,
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
    /// The inviter's own reachable addresses. The joiner tries each (pinned to
    /// the bootstrap fp) for join-confirm rather than trusting only the TLS
    /// source IP. `serde(default)` so an offer from a pre-candidate-addr
    /// inviter parses (joiner then falls back to the source IP).
    #[serde(default)]
    inviter_addrs: Vec<String>,
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
                Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
                None,
            ),
        },
        POD_JOIN_CONFIRM_METHOD => match handle_join_confirm(&env) {
            Ok(r) => (value_response(id, &r), None),
            Err(e) => (
                Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
                None,
            ),
        },
        POD_REQUEST_OFFER_METHOD => match handle_request_offer(&env, peer) {
            Ok(r) => (value_response(id, &r), None),
            Err(e) => (
                Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
                None,
            ),
        },
        POD_REFRESH_CERT_BOOTSTRAP_METHOD => match handle_refresh_cert_bootstrap(&env) {
            Ok(r) => (value_response(id, &r), None),
            Err(e) => (
                Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
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
    // Candidate dial-back addresses for join-confirm, most-reliable first: the
    // inviter's self-advertised addresses, then the TLS source IP (always
    // reachable but possibly a tunnel address), then any legacy `inviter_addr`.
    // The joiner tries each pinned to `signer_fp`, so a wrong candidate simply
    // fails the pin and we move on. Deduped, order-preserving.
    let mut candidate_addrs: Vec<String> = Vec::new();
    for a in body
        .inviter_addrs
        .iter()
        .map(String::as_str)
        .chain([inviter_addr, body.inviter_addr.as_str()])
    {
        let a = a.trim();
        if !a.is_empty() && !candidate_addrs.iter().any(|c| c == a) {
            candidate_addrs.push(a.to_string());
        }
    }
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
        &candidate_addrs,
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
        .unwrap_or_else(|| system::host_identity::machine_id().to_string());
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

/// Body of `pod/refresh-cert-bootstrap`: a peer whose mesh **leaf** cert has
/// expired (so it can no longer authenticate an mTLS refresh) asks a CA-key
/// holder to re-sign its CSRs. Identity is bound to the signed bootstrap key
/// rather than a client cert — the bootstrap key is long-lived and unaffected
/// by leaf expiry, which is what breaks the "expired leaf can't renew itself"
/// deadlock.
#[derive(Debug, Deserialize)]
struct RefreshCertBootstrapBody {
    joiner_hostname: String,
    csr_client_pem: String,
    csr_server_pem: String,
}

#[derive(Debug, Serialize)]
struct RefreshCertBootstrapResult {
    client_cert_pem: String,
    server_cert_pem: String,
    ca_cert_pem: String,
}

/// Sign refreshed CSRs for a peer over the bootstrap channel. Authorization
/// mirrors the mTLS `handle_refresh_cert` CN check, but binds identity to the
/// envelope signer's bootstrap fp: the signer must be a known, non-departed
/// pod member whose pinned bootstrap fp matches AND whose peer_id equals the
/// claimed `joiner_hostname` (== machine_id). This ensures an unauthenticated
/// bootstrap caller can only mint leaves for the identity it already owns.
fn handle_refresh_cert_bootstrap(env: &SignedEnvelope) -> Result<RefreshCertBootstrapResult> {
    let pki_d = pki_dir();
    anyhow::ensure!(
        utils::pki::has_mesh_ca_key(&pki_d),
        "this host does not have the mesh CA key — cannot refresh peer certs"
    );
    let (body, signer_vk) = utils::pki::verify_envelope::<RefreshCertBootstrapBody>(env)?;
    let signer_fp = utils::pki::bootstrap_pubkey_fingerprint(&signer_vk);

    let conn = db::open_default()?;
    let peers = pdb::list_peers(&conn)?;
    let peer = peers
        .iter()
        .find(|p| p.departed_at.is_none() && p.pubkey_fp.as_deref() == Some(signer_fp.as_str()))
        .with_context(|| {
            format!("bootstrap refresh refused: signer fp {signer_fp} is not a known active peer")
        })?;
    anyhow::ensure!(
        peer.peer_id == body.joiner_hostname,
        "bootstrap refresh refused: peer_id ({}) does not match joiner_hostname ({})",
        peer.peer_id,
        body.joiner_hostname
    );

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
    Ok(RefreshCertBootstrapResult {
        client_cert_pem,
        server_cert_pem,
        ca_cert_pem,
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
    let inviter_peer_id = system::host_identity::machine_id().to_string();
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
        &[], // outbound offer: the joiner dials us, not the reverse
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
        inviter_addrs: crate::scheduler::self_advertised_addrs(),
    })
}

fn value_response<T: Serialize>(id: Value, v: &T) -> Response {
    match serde_json::to_value(v) {
        Ok(val) => Response::ok(id, val),
        Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
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
            inviter_addrs: vec!["10.0.0.1".into()],
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
            inviter_addrs: vec!["10.0.0.1".into(), "100.64.0.1".into()],
        };
        let v = serde_json::to_value(&r).unwrap();
        let back: RequestOfferResult = serde_json::from_value(v).unwrap();
        assert_eq!(back.code_plain.as_deref(), Some("ABCDEF"));
        assert_eq!(back.inviter_addrs, vec!["10.0.0.1", "100.64.0.1"]);
    }

    // ── serialize-shape guards (assert on serialized strings) ────────────────

    #[test]
    fn offer_ack_serializes_null_code_hint() {
        let ack = OfferAck { code_hint: None };
        let s = serde_json::to_string(&ack).unwrap();
        assert_eq!(s, r#"{"code_hint":null}"#);
    }

    #[test]
    fn offer_ack_serializes_populated_code_hint() {
        let ack = OfferAck {
            code_hint: Some("AB".into()),
        };
        let s = serde_json::to_string(&ack).unwrap();
        assert_eq!(s, r#"{"code_hint":"AB"}"#);
    }

    #[test]
    fn join_confirm_result_serializes_all_fields() {
        let r = JoinConfirmResult {
            client_cert_pem: "CLIENT".into(),
            server_cert_pem: "SERVER".into(),
            ca_cert_pem: "CA".into(),
            inviter_peer_id: "host-g".into(),
            pod_id: "p1".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""client_cert_pem":"CLIENT""#));
        assert!(s.contains(r#""server_cert_pem":"SERVER""#));
        assert!(s.contains(r#""ca_cert_pem":"CA""#));
        assert!(s.contains(r#""inviter_peer_id":"host-g""#));
        assert!(s.contains(r#""pod_id":"p1""#));
    }

    #[test]
    fn refresh_cert_bootstrap_result_serializes_all_fields() {
        let r = RefreshCertBootstrapResult {
            client_cert_pem: "C".into(),
            server_cert_pem: "S".into(),
            ca_cert_pem: "A".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(
            s,
            r#"{"client_cert_pem":"C","server_cert_pem":"S","ca_cert_pem":"A"}"#
        );
    }

    #[test]
    fn refresh_cert_bootstrap_body_deserializes() {
        let json = r#"{"joiner_hostname":"xyz789","csr_client_pem":"CC","csr_server_pem":"SS"}"#;
        let body: RefreshCertBootstrapBody = serde_json::from_str(json).unwrap();
        assert_eq!(body.joiner_hostname, "xyz789");
        assert_eq!(body.csr_client_pem, "CC");
        assert_eq!(body.csr_server_pem, "SS");
    }

    #[test]
    fn offer_body_defaults_inviter_addrs_empty() {
        let json = r#"{
            "inviter_peer_id":"abc","inviter_hostname":"abc123",
            "inviter_addr":"10.0.0.1","inviter_port":12002,
            "mesh_ca_cert_pem":"","pod_id":"p1","code_hash":"h","expires_at":0
        }"#;
        let body: OfferBody = serde_json::from_str(json).unwrap();
        assert!(body.inviter_addrs.is_empty());
    }

    #[test]
    fn offer_body_parses_inviter_addrs_list() {
        let json = r#"{
            "inviter_peer_id":"abc","inviter_hostname":"abc123",
            "inviter_addr":"10.0.0.1","inviter_port":12002,
            "mesh_ca_cert_pem":"","pod_id":"p1","code_hash":"h","expires_at":0,
            "inviter_addrs":["10.0.0.1","100.64.0.1"]
        }"#;
        let body: OfferBody = serde_json::from_str(json).unwrap();
        assert_eq!(body.inviter_addrs, vec!["10.0.0.1", "100.64.0.1"]);
    }

    // ── value_response ───────────────────────────────────────────────────────

    #[test]
    fn value_response_wraps_ok_result() {
        let ack = OfferAck {
            code_hint: Some("AB".into()),
        };
        let resp = value_response(serde_json::Value::from(7u64), &ack);
        assert!(!resp.is_error());
        assert!(resp.error.is_none());
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""result":{"code_hint":"AB"}"#));
        assert!(s.contains(r#""id":7"#));
    }

    // ── envelope sign / verify + fingerprint ─────────────────────────────────

    #[test]
    fn signed_offer_body_roundtrips_and_fp_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let key = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let body = OfferBody {
            inviter_peer_id: "abc".into(),
            inviter_hostname: "abc123".into(),
            inviter_addr: "10.0.0.1".into(),
            inviter_port: 12002,
            mesh_ca_cert_pem: String::new(),
            pod_id: "p1".into(),
            code_hash: "h".into(),
            expires_at: 0,
            inviter_display_name: None,
            code_plain: None,
            inviter_addrs: vec![],
        };
        let env = utils::pki::sign_envelope(&key, &body).unwrap();
        let (back, vk): (OfferBody, _) = utils::pki::verify_envelope(&env).unwrap();
        assert_eq!(back.inviter_peer_id, "abc");
        assert_eq!(back.pod_id, "p1");
        let fp = utils::pki::bootstrap_pubkey_fingerprint(&vk);
        let fp_direct = utils::pki::bootstrap_pubkey_fingerprint(&key.verifying_key());
        assert_eq!(fp, fp_direct);
        assert!(!fp.is_empty());
    }

    #[test]
    fn tampered_envelope_fails_verification() {
        let dir = tempfile::tempdir().unwrap();
        let key = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let body = OfferBody {
            inviter_peer_id: "abc".into(),
            inviter_hostname: "abc123".into(),
            inviter_addr: "10.0.0.1".into(),
            inviter_port: 12002,
            mesh_ca_cert_pem: String::new(),
            pod_id: "p1".into(),
            code_hash: "h".into(),
            expires_at: 0,
            inviter_display_name: None,
            code_plain: None,
            inviter_addrs: vec![],
        };
        let mut env = utils::pki::sign_envelope(&key, &body).unwrap();
        env.payload.push_str("tampered");
        let res: Result<(OfferBody, _)> = utils::pki::verify_envelope(&env);
        assert!(res.is_err());
    }

    // ── dispatch guard branches (no DB / network required) ───────────────────

    fn test_peer() -> std::net::SocketAddr {
        "127.0.0.1:9999".parse().unwrap()
    }

    #[tokio::test]
    async fn dispatch_rejects_missing_params() {
        let req = Request::new(1u64, POD_OFFER_METHOD, None);
        let (resp, auto) = dispatch(req, test_peer()).await;
        assert!(auto.is_none());
        let err = resp.error.expect("error expected");
        assert_eq!(err.code, -32603);
        assert!(err.message.contains("requires signed params"));
    }

    #[tokio::test]
    async fn dispatch_rejects_unparseable_params() {
        // An empty object is not a valid SignedEnvelope (missing fields).
        let params = serde_json::to_value(serde_json::Map::new()).unwrap();
        let req = Request::new(1u64, POD_OFFER_METHOD, Some(params));
        let (resp, auto) = dispatch(req, test_peer()).await;
        assert!(auto.is_none());
        let err = resp.error.expect("error expected");
        assert_eq!(err.code, -32603);
        assert!(err.message.contains("parse signed envelope"));
    }

    #[tokio::test]
    async fn dispatch_unknown_method_returns_method_not_found() {
        // A structurally valid SignedEnvelope parses fine; the method routing
        // then rejects an unsupported method before any verification/DB work.
        let env = SignedEnvelope {
            payload: "{}".into(),
            signer_pubkey_b64: String::new(),
            signature_b64: String::new(),
        };
        let params = serde_json::to_value(&env).unwrap();
        let req = Request::new(1u64, "pod/does-not-exist", Some(params));
        let (resp, auto) = dispatch(req, test_peer()).await;
        assert!(auto.is_none());
        let err = resp.error.expect("error expected");
        assert_eq!(err.code, -32601);
        assert!(err.message.contains("not supported"));
    }

    // ── dispatch: each real method returns an internal error when the
    //    signed envelope fails verification (empty signer). None of these
    //    reach the DB — verification bails first. ──────────────────────────────

    fn unverifiable_envelope_params() -> serde_json::Value {
        // Structurally valid SignedEnvelope but the signer/signature are empty,
        // so verify_envelope fails at base64/key parsing before any DB work.
        let env = SignedEnvelope {
            payload: "{}".into(),
            signer_pubkey_b64: String::new(),
            signature_b64: String::new(),
        };
        serde_json::to_value(&env).unwrap()
    }

    #[tokio::test]
    async fn dispatch_join_confirm_bad_envelope_is_internal_error() {
        let req = Request::new(
            1u64,
            POD_JOIN_CONFIRM_METHOD,
            Some(unverifiable_envelope_params()),
        );
        let (resp, auto) = dispatch(req, test_peer()).await;
        assert!(auto.is_none());
        let err = resp.error.expect("error expected");
        assert_eq!(err.code, -32603);
    }

    #[tokio::test]
    async fn dispatch_request_offer_bad_envelope_is_internal_error() {
        let req = Request::new(
            1u64,
            POD_REQUEST_OFFER_METHOD,
            Some(unverifiable_envelope_params()),
        );
        let (resp, auto) = dispatch(req, test_peer()).await;
        assert!(auto.is_none());
        let err = resp.error.expect("error expected");
        assert_eq!(err.code, -32603);
    }

    #[tokio::test]
    async fn dispatch_refresh_cert_bootstrap_bad_envelope_is_internal_error() {
        // Either this host lacks the mesh CA key (ensure! bails) or it has one
        // and verification of the empty-signer envelope fails — both are
        // internal errors, neither touches the pod DB.
        let req = Request::new(
            1u64,
            POD_REFRESH_CERT_BOOTSTRAP_METHOD,
            Some(unverifiable_envelope_params()),
        );
        let (resp, auto) = dispatch(req, test_peer()).await;
        assert!(auto.is_none());
        let err = resp.error.expect("error expected");
        assert_eq!(err.code, -32603);
    }

    // ── handle_request_offer: fp-mismatch guard fires before any DB access ─────

    #[test]
    fn handle_request_offer_rejects_fp_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let key = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let body = RequestOfferBody {
            joiner_peer_id: "abc".into(),
            joiner_hostname: "abc123".into(),
            // Deliberately does NOT match the real signer fp.
            joiner_pubkey_fp: "fp-does-not-match".into(),
            joiner_display_name: None,
        };
        let env = utils::pki::sign_envelope(&key, &body).unwrap();
        let res = handle_request_offer(&env, test_peer());
        let err = res.expect_err("fp mismatch must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("does not match advertised joiner_pubkey_fp"));
        assert!(msg.contains("fp-does-not-match"));
    }

    // ── signed round-trips for the remaining bodies ───────────────────────────

    #[test]
    fn signed_join_confirm_body_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let key = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let body = JoinConfirmBody {
            code: "ABC123".into(),
            joiner_hostname: "xyz789".into(),
            csr_client_pem: "CC".into(),
            csr_server_pem: "SS".into(),
            joiner_display_name: Some("host-h".into()),
        };
        let env = utils::pki::sign_envelope(&key, &body).unwrap();
        let (back, vk): (JoinConfirmBody, _) = utils::pki::verify_envelope(&env).unwrap();
        assert_eq!(back.code, "ABC123");
        assert_eq!(back.joiner_hostname, "xyz789");
        assert_eq!(back.joiner_display_name.as_deref(), Some("host-h"));
        assert!(!utils::pki::bootstrap_pubkey_fingerprint(&vk).is_empty());
    }

    #[test]
    fn signed_request_offer_body_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let key = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let body = RequestOfferBody {
            joiner_peer_id: "abc".into(),
            joiner_hostname: "abc123".into(),
            joiner_pubkey_fp: "fp".into(),
            joiner_display_name: None,
        };
        let env = utils::pki::sign_envelope(&key, &body).unwrap();
        let (back, _vk): (RequestOfferBody, _) = utils::pki::verify_envelope(&env).unwrap();
        assert_eq!(back.joiner_peer_id, "abc");
        assert!(back.joiner_display_name.is_none());
    }

    #[test]
    fn envelope_signed_by_different_keys_have_distinct_fps() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let key_a = utils::pki::load_or_init_bootstrap_key(dir_a.path()).unwrap();
        let key_b = utils::pki::load_or_init_bootstrap_key(dir_b.path()).unwrap();
        let fp_a = utils::pki::bootstrap_pubkey_fingerprint(&key_a.verifying_key());
        let fp_b = utils::pki::bootstrap_pubkey_fingerprint(&key_b.verifying_key());
        assert_ne!(fp_a, fp_b);
    }

    #[test]
    fn envelope_with_wrong_signature_length_fails_verification() {
        let dir = tempfile::tempdir().unwrap();
        let key = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let body = JoinConfirmBody {
            code: "ABC123".into(),
            joiner_hostname: "xyz789".into(),
            csr_client_pem: String::new(),
            csr_server_pem: String::new(),
            joiner_display_name: None,
        };
        let mut env = utils::pki::sign_envelope(&key, &body).unwrap();
        // Corrupt the signature so base64 decodes but the bytes don't verify.
        env.signature_b64 = utils::pki::sign_envelope(&key, &"other")
            .unwrap()
            .signature_b64;
        let res: Result<(JoinConfirmBody, _)> = utils::pki::verify_envelope(&env);
        assert!(res.is_err());
    }

    // ── serialize-shape guards for request/offer bodies ───────────────────────

    #[test]
    fn request_offer_body_serializes_display_name_null_when_none() {
        let body = RequestOfferBody {
            joiner_peer_id: "abc".into(),
            joiner_hostname: "abc123".into(),
            joiner_pubkey_fp: "fp".into(),
            joiner_display_name: None,
        };
        let s = serde_json::to_string(&body).unwrap();
        assert!(s.contains(r#""joiner_pubkey_fp":"fp""#));
        assert!(s.contains(r#""joiner_display_name":null"#));
    }

    #[test]
    fn offer_body_serializes_optional_fields_null_when_none() {
        let body = OfferBody {
            inviter_peer_id: "abc".into(),
            inviter_hostname: "abc123".into(),
            inviter_addr: "10.0.0.1".into(),
            inviter_port: 12002,
            mesh_ca_cert_pem: String::new(),
            pod_id: "p1".into(),
            code_hash: "h".into(),
            expires_at: 0,
            inviter_display_name: None,
            code_plain: None,
            inviter_addrs: vec![],
        };
        let s = serde_json::to_string(&body).unwrap();
        assert!(s.contains(r#""inviter_display_name":null"#));
        assert!(s.contains(r#""code_plain":null"#));
        assert!(s.contains(r#""inviter_addrs":[]"#));
    }

    // ── deserialize error branches (missing required fields) ───────────────────

    #[test]
    fn offer_body_missing_required_field_errors() {
        // Omits `inviter_port` — required (no serde default).
        let json = r#"{
            "inviter_peer_id":"abc","inviter_hostname":"abc123",
            "inviter_addr":"10.0.0.1",
            "mesh_ca_cert_pem":"","pod_id":"p1","code_hash":"h","expires_at":0
        }"#;
        let res: std::result::Result<OfferBody, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }

    #[test]
    fn join_confirm_body_missing_code_errors() {
        let json = r#"{"joiner_hostname":"xyz","csr_client_pem":"","csr_server_pem":""}"#;
        let res: std::result::Result<JoinConfirmBody, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }

    #[test]
    fn request_offer_body_missing_fp_errors() {
        let json = r#"{"joiner_peer_id":"abc","joiner_hostname":"abc123"}"#;
        let res: std::result::Result<RequestOfferBody, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }

    #[test]
    fn refresh_cert_bootstrap_body_missing_csr_errors() {
        let json = r#"{"joiner_hostname":"xyz789","csr_client_pem":"CC"}"#;
        let res: std::result::Result<RefreshCertBootstrapBody, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }

    #[test]
    fn request_offer_result_defaults_inviter_addrs_empty() {
        let json = r#"{
            "inviter_pubkey_fp":"fp","inviter_peer_id":"x","inviter_hostname":"x",
            "inviter_addr":"","inviter_port":12002,"mesh_ca_cert_pem":"",
            "pod_id":"p","code_hash":"h","expires_at":0
        }"#;
        let r: RequestOfferResult = serde_json::from_str(json).unwrap();
        assert!(r.inviter_addrs.is_empty());
        assert!(r.inviter_display_name.is_none());
    }

    // ── value_response error path (id preserved) ──────────────────────────────

    #[test]
    fn value_response_preserves_string_id_and_result_shape() {
        let r = JoinConfirmResult {
            client_cert_pem: "C".into(),
            server_cert_pem: "S".into(),
            ca_cert_pem: "A".into(),
            inviter_peer_id: "host-g".into(),
            pod_id: "p1".into(),
        };
        let resp = value_response(serde_json::Value::from("req-1"), &r);
        assert!(!resp.is_error());
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""id":"req-1""#));
        assert!(s.contains(r#""inviter_peer_id":"host-g""#));
    }

    // ── handle_offer: signed-envelope happy paths against an ephemeral DB ──────
    //
    // These reach the full offer-ingest body (ttl guard, source-IP fallback,
    // candidate-addr dedup, pending-offer insert, auto-accept selection) without
    // touching real mesh PKI: the signer key lives in a tempdir and the CA is
    // never consulted by `handle_offer` (only `verify_envelope` runs, keyed by
    // the in-test bootstrap key).

    fn mk_offer_body(expires_at: i64) -> OfferBody {
        OfferBody {
            inviter_peer_id: "inv-peer".into(),
            inviter_hostname: "invhost".into(),
            inviter_addr: "10.0.0.1".into(),
            inviter_port: 12002,
            mesh_ca_cert_pem: "CA-PEM".into(),
            pod_id: "pod-x".into(),
            code_hash: "hash-x".into(),
            expires_at,
            inviter_display_name: Some("Inviter Host".into()),
            code_plain: None,
            inviter_addrs: vec![],
        }
    }

    #[tokio::test]
    async fn handle_offer_stores_pending_and_dedups_candidate_addrs() {
        let dir = tempfile::tempdir().unwrap();
        let key = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let expected_fp = utils::pki::bootstrap_pubkey_fingerprint(&key.verifying_key());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let mut body = mk_offer_body(utils::time::now_secs_since_epoch() + 3600);
            // Two duplicate self-advertised addrs plus a distinct inviter_addr:
            // the dedup loop must collapse to two entries, order preserved.
            body.inviter_addrs = vec!["10.0.0.2".into(), "10.0.0.2".into()];
            let env = utils::pki::sign_envelope(&key, &body).unwrap();
            let (ack, auto) = handle_offer(&env, test_peer()).unwrap();
            assert!(ack.code_hint.is_none());
            assert!(auto.is_none(), "no code_plain → no auto-accept");

            let conn = db::open_default().unwrap();
            let offers = pdb::list_pending_offers(&conn, "in").unwrap();
            assert_eq!(offers.len(), 1);
            let o = &offers[0];
            assert_eq!(o.peer_pubkey_fp, expected_fp);
            // Non-empty inviter_addr is stored as the primary peer_addr.
            assert_eq!(o.peer_addr, "10.0.0.1");
            assert_eq!(o.peer_port, 12002);
            assert_eq!(o.pod_id.as_deref(), Some("pod-x"));
            assert_eq!(o.inviter_peer_id.as_deref(), Some("inv-peer"));
            // Candidate order: advertised addrs first (deduped), then inviter_addr.
            assert_eq!(o.candidate_addrs, vec!["10.0.0.2", "10.0.0.1"]);
        })
        .await;
    }

    #[tokio::test]
    async fn handle_offer_returns_auto_accept_when_code_plain_present() {
        let dir = tempfile::tempdir().unwrap();
        let key = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let mut body = mk_offer_body(utils::time::now_secs_since_epoch() + 3600);
            body.code_plain = Some("SECRET-CODE".into());
            let env = utils::pki::sign_envelope(&key, &body).unwrap();
            let (_ack, auto) = handle_offer(&env, test_peer()).unwrap();
            assert_eq!(auto.as_deref(), Some("SECRET-CODE"));
        })
        .await;
    }

    #[tokio::test]
    async fn handle_offer_rejects_already_expired_offer() {
        let dir = tempfile::tempdir().unwrap();
        let key = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            // expires_at strictly in the past → ttl <= 0 → bail.
            let body = mk_offer_body(utils::time::now_secs_since_epoch() - 10);
            let env = utils::pki::sign_envelope(&key, &body).unwrap();
            let err = handle_offer(&env, test_peer()).unwrap_err();
            assert!(
                format!("{err:#}").contains("offer already expired"),
                "got: {err:#}"
            );
            // Nothing persisted on the expired path.
            let conn = db::open_default().unwrap();
            assert!(pdb::list_pending_offers(&conn, "in").unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn handle_offer_falls_back_to_source_ip_when_inviter_addr_empty() {
        let dir = tempfile::tempdir().unwrap();
        let key = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let mut body = mk_offer_body(utils::time::now_secs_since_epoch() + 3600);
            body.inviter_addr = String::new();
            body.inviter_addrs = vec![];
            let env = utils::pki::sign_envelope(&key, &body).unwrap();
            handle_offer(&env, test_peer()).unwrap();
            let conn = db::open_default().unwrap();
            let offers = pdb::list_pending_offers(&conn, "in").unwrap();
            // test_peer() is 127.0.0.1:9999 → the TLS source IP backfills addr.
            assert_eq!(offers[0].peer_addr, "127.0.0.1");
            assert_eq!(offers[0].candidate_addrs, vec!["127.0.0.1"]);
        })
        .await;
    }

    #[tokio::test]
    async fn dispatch_offer_valid_envelope_returns_ack_and_auto_accept() {
        let dir = tempfile::tempdir().unwrap();
        let key = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let mut body = mk_offer_body(utils::time::now_secs_since_epoch() + 3600);
            body.code_plain = Some("AUTO-1".into());
            let env = utils::pki::sign_envelope(&key, &body).unwrap();
            let params = serde_json::to_value(&env).unwrap();
            let req = Request::new(1u64, POD_OFFER_METHOD, Some(params));
            let (resp, auto) = dispatch(req, test_peer()).await;
            assert!(!resp.is_error(), "valid offer must succeed");
            assert_eq!(auto.as_deref(), Some("AUTO-1"));
            let conn = db::open_default().unwrap();
            assert_eq!(pdb::list_pending_offers(&conn, "in").unwrap().len(), 1);
        })
        .await;
    }
}
