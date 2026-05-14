// JSON-RPC envelopes are inherently opaque at the wire boundary; mirroring
// the allow in projects/sdk/rust/src/jsonrpc.rs.
#![allow(clippy::disallowed_types)]

//! Server-side handler for SNI=pod.orca.local connections.
//!
//! Every method on this surface requires a verified mesh-CA-signed client
//! cert (the plugin host's TLS layer rejects connections without one). The
//! pre-join methods (pod/offer, pod/join-confirm) live on a separate SNI
//! (pod-bootstrap.orca.local) — see super::bootstrap.

use anyhow::{Context, Result};
use orca_sdk::framing::{read_frame, write_frame};
use orca_sdk::jsonrpc::{ErrorObject, Message, Request, Response};
use orca_sdk::pki::{self, PeerRole};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_rustls::server::TlsStream;
use tracing::warn;

use super::{POD_PING_METHOD, PodPingResult, db as pdb, pki_dir};

const POD_NOTIFY_TRUST_METHOD: &str = "pod/notify-trust";
const POD_HAS_CA_KEY_METHOD: &str = "pod/has-ca-key";
const POD_PUSH_CA_KEY_METHOD: &str = "pod/push-ca-key";
const POD_PEER_LEAVING_METHOD: &str = "pod/peer-leaving";
const POD_REFRESH_CERT_METHOD: &str = "pod/refresh-cert";
const POD_PUSH_CA_STATE_METHOD: &str = "pod/push-ca-state";

#[derive(Debug, Deserialize)]
struct NotifyTrustParams {
    trust: bool,
}

#[derive(Debug, Serialize)]
struct HasCaKeyResult {
    has_key: bool,
}

#[derive(Debug, Deserialize)]
struct PushCaKeyParams {
    cert_pem: String,
    key_pem: String,
}

#[derive(Debug, Deserialize)]
struct RefreshCertParams {
    joiner_hostname: String,
    csr_client_pem: String,
    csr_server_pem: String,
}

#[derive(Debug, Serialize)]
struct RefreshCertResult {
    client_cert_pem: String,
    server_cert_pem: String,
    ca_cert_pem: String,
}

#[derive(Debug, Deserialize)]
struct PushCaStateParams {
    current_cert_pem: String,
    current_key_pem: String,
    previous_cert_pem: Option<String>,
    previous_key_pem: Option<String>,
    /// Unix timestamp at which the previous slot should be dropped.
    previous_expires_at: Option<i64>,
}

pub async fn handle_pod_connection(
    mut tls: TlsStream<tokio::net::TcpStream>,
    peer_cn: String,
    peer_addr: std::net::SocketAddr,
) -> Result<()> {
    let frame_bytes = read_frame(&mut tls).await.context("read pod frame")?;
    let msg: Message =
        serde_json::from_slice(&frame_bytes).context("parse pod frame as JSON-RPC")?;
    let request = match msg {
        Message::Request(r) => r,
        Message::Response(_) | Message::Notification(_) => {
            warn!("[pod] {peer_cn} sent non-request frame; closing");
            return Ok(());
        }
    };

    let response = dispatch(request, &peer_cn, peer_addr).await;

    let envelope = serde_json::to_vec(&response).context("serialize pod response")?;
    write_frame(&mut tls, &envelope)
        .await
        .context("write pod response")?;
    Ok(())
}

async fn dispatch(request: Request, peer_cn: &str, peer_addr: std::net::SocketAddr) -> Response {
    let method = request.method.clone();
    let id = request.id.clone();

    // Departed peers are rejected at the gate — they need to re-pair before
    // we'll talk to them again. pod/peer-leaving is the one exception: a
    // peer that's already departed can re-send leaving without harm.
    if method != POD_PEER_LEAVING_METHOD {
        match db::open_default() {
            Ok(conn) => {
                if let Ok(true) = pdb::is_peer_departed(&conn, peer_cn) {
                    return Response::err(
                        id,
                        ErrorObject::method_not_found(&format!(
                            "peer {peer_cn} has departed this pod; re-pair to re-establish trust"
                        )),
                    );
                }
            }
            Err(_) => { /* DB unavailable — fall through to method handlers, which will fail with a clearer error */
            }
        }
    }

    match method.as_str() {
        POD_PING_METHOD => {
            let result = PodPingResult {
                peer_id: peer_cn.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                hostname: crate::host_identity::hostname().to_string(),
            };
            value_response(id, &result)
        }
        POD_NOTIFY_TRUST_METHOD => match handle_notify_trust(peer_cn, peer_addr, request) {
            Ok(()) => Response::ok(id, Value::Null),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
        POD_HAS_CA_KEY_METHOD => {
            let has = pki::has_mesh_ca_key(&pki_dir());
            value_response(id, &HasCaKeyResult { has_key: has })
        }
        POD_PUSH_CA_KEY_METHOD => match handle_push_ca_key(peer_cn, request) {
            Ok(()) => Response::ok(id, Value::Null),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
        POD_PEER_LEAVING_METHOD => match handle_peer_leaving(peer_cn) {
            Ok(()) => Response::ok(id, Value::Null),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
        POD_REFRESH_CERT_METHOD => match handle_refresh_cert(peer_cn, request) {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
        POD_PUSH_CA_STATE_METHOD => match handle_push_ca_state(peer_cn, request) {
            Ok(()) => Response::ok(id, Value::Null),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
        other => Response::err(
            id,
            ErrorObject::method_not_found(&format!("pod method '{other}' not supported")),
        ),
    }
}

fn handle_notify_trust(
    peer_cn: &str,
    peer_addr: std::net::SocketAddr,
    request: Request,
) -> Result<()> {
    let params: NotifyTrustParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/notify-trust params")?,
        None => anyhow::bail!("pod/notify-trust requires params"),
    };
    let conn = db::open_default()?;
    // Self-heal: the mTLS layer validated this CN against the mesh CA, so we
    // can trust it. If no pod_peers row exists yet (legacy rc.≤24 joiner that
    // landed as peer_id="unknown", or CN/peer_id drift), materialize a stub
    // keyed by the CN so the FK on pod_trust.peer_id is satisfied.
    pdb::ensure_peer_stub(
        &conn,
        peer_cn,
        &peer_addr.ip().to_string(),
        peer_addr.port(),
    )?;
    pdb::set_trust(&conn, peer_cn, None, Some(params.trust))?;
    Ok(())
}

fn handle_push_ca_key(peer_cn: &str, request: Request) -> Result<()> {
    let params: PushCaKeyParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/push-ca-key params")?,
        None => anyhow::bail!("pod/push-ca-key requires params"),
    };
    let conn = db::open_default()?;
    let t = pdb::get_trust(&conn, peer_cn)?;
    if !pdb::is_mutual_secure(t) {
        anyhow::bail!(
            "pod/push-ca-key refused: peer {peer_cn} is not mutually secure with this host"
        );
    }
    pki::import_mesh_ca_keypair(&pki_dir(), &params.cert_pem, &params.key_pem)?;
    Ok(())
}

fn handle_peer_leaving(peer_cn: &str) -> Result<()> {
    let conn = db::open_default()?;
    pdb::mark_peer_departed(&conn, peer_cn)?;
    Ok(())
}

fn handle_push_ca_state(peer_cn: &str, request: Request) -> Result<()> {
    let params: PushCaStateParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/push-ca-state params")?,
        None => anyhow::bail!("pod/push-ca-state requires params"),
    };
    let conn = db::open_default()?;
    let t = pdb::get_trust(&conn, peer_cn)?;
    if !pdb::is_mutual_secure(t) {
        anyhow::bail!(
            "pod/push-ca-state refused: peer {peer_cn} is not mutually secure with this host"
        );
    }
    pki::import_mesh_ca_state(
        &pki_dir(),
        &params.current_cert_pem,
        &params.current_key_pem,
        params.previous_cert_pem.as_deref(),
        params.previous_key_pem.as_deref(),
    )?;
    if let Some(exp) = params.previous_expires_at {
        pdb::set_ca_previous_expires_at(&conn, Some(exp))?;
    }
    Ok(())
}

/// Sign refreshed CSRs for a peer that doesn't hold the mesh CA key itself
/// (non-secure joiner that needs rotation before its 30-day cert expires).
/// Requires the requesting peer to be a known, non-departed pod member —
/// the mTLS handshake already authenticated the CN, and the departed-peer
/// gate above blocks departed CNs from reaching this method.
fn handle_refresh_cert(peer_cn: &str, request: Request) -> Result<RefreshCertResult> {
    anyhow::ensure!(
        pki::has_mesh_ca_key(&pki_dir()),
        "this host does not have the mesh CA key — cannot refresh peer certs"
    );
    let params: RefreshCertParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/refresh-cert params")?,
        None => anyhow::bail!("pod/refresh-cert requires params"),
    };

    // Enforce that the joiner identifier matches the authenticated CN. CN is
    // `peer.<machine_id_short>`; the param is named `joiner_hostname` for wire
    // compat but now carries the stable machine_id, not the OS hostname.
    let expected_cn = format!("peer.{}", params.joiner_hostname);
    anyhow::ensure!(
        peer_cn == expected_cn,
        "refresh refused: cert CN ({peer_cn}) does not match joiner_hostname ({expected_cn})"
    );

    let pki_d = pki_dir();
    let (client_cert_pem, ca_cert_pem) = pki::sign_peer_csr(
        &pki_d,
        &params.csr_client_pem,
        &params.joiner_hostname,
        PeerRole::Client,
    )?;
    let (server_cert_pem, _) = pki::sign_peer_csr(
        &pki_d,
        &params.csr_server_pem,
        &params.joiner_hostname,
        PeerRole::Server,
    )?;
    Ok(RefreshCertResult {
        client_cert_pem,
        server_cert_pem,
        ca_cert_pem,
    })
}

fn value_response<T: Serialize>(id: Value, v: &T) -> Response {
    match serde_json::to_value(v) {
        Ok(val) => Response::ok(id, val),
        Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
    }
}
