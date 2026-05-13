//! Server-side handler for SNI=pod.orca.local connections.
//!
//! Per-method auth gate: pod/join is the only method allowed without a peer
//! cert (it's the bootstrap path that issues the cert). Every other method
//! requires the connection to have presented a valid mesh-CA-signed client
//! cert at the TLS handshake.

use anyhow::{Context, Result};
use orca_sdk::framing::{read_frame, write_frame};
use orca_sdk::jsonrpc::{ErrorObject, Message, Request, Response};
use orca_sdk::pki::{self, PeerRole};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_rustls::server::TlsStream;
use tracing::warn;

use super::{POD_PING_METHOD, PodPingResult, db as pdb, pki_dir};

const POD_JOIN_METHOD: &str = "pod/join";
const POD_NOTIFY_TRUST_METHOD: &str = "pod/notify-trust";
const POD_HAS_CA_KEY_METHOD: &str = "pod/has-ca-key";
const POD_PUSH_CA_KEY_METHOD: &str = "pod/push-ca-key";

/// Pod-join request body. `joiner_hostname` becomes the CN of both certs;
/// the founder enforces naming regardless of what the CSRs claim.
#[derive(Debug, Deserialize)]
struct PodJoinParams {
    token: String,
    joiner_hostname: String,
    csr_client_pem: String,
    csr_server_pem: String,
}

#[derive(Debug, Serialize)]
struct PodJoinResult {
    client_cert_pem: String,
    server_cert_pem: String,
    ca_cert_pem: String,
}

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

pub async fn handle_pod_connection(
    mut tls: TlsStream<tokio::net::TcpStream>,
    peer_cn: Option<String>,
) -> Result<()> {
    let frame_bytes = read_frame(&mut tls).await.context("read pod frame")?;
    let msg: Message =
        serde_json::from_slice(&frame_bytes).context("parse pod frame as JSON-RPC")?;
    let request = match msg {
        Message::Request(r) => r,
        Message::Response(_) | Message::Notification(_) => {
            warn!("[pod] {peer_cn:?} sent non-request frame; closing");
            return Ok(());
        }
    };

    let response = dispatch(request, &peer_cn).await;

    let envelope = serde_json::to_vec(&response).context("serialize pod response")?;
    write_frame(&mut tls, &envelope)
        .await
        .context("write pod response")?;
    Ok(())
}

async fn dispatch(request: Request, peer_cn: &Option<String>) -> Response {
    let method = request.method.clone();
    let id = request.id.clone();

    // Auth gate: pod/join is the only method that accepts a no-cert
    // connection. Every other method requires a verified peer cert.
    if method != POD_JOIN_METHOD && peer_cn.is_none() {
        return Response::err(
            id,
            ErrorObject::method_not_found(&format!(
                "pod method '{method}' requires a mesh client certificate"
            )),
        );
    }

    match method.as_str() {
        POD_PING_METHOD => {
            let result = PodPingResult {
                peer_id: peer_cn.clone().unwrap_or_else(|| "anonymous".into()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                hostname: gethostname_string(),
            };
            value_response(id, &result)
        }
        POD_JOIN_METHOD => match handle_join(request).await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
        POD_NOTIFY_TRUST_METHOD => {
            let cn = peer_cn.clone().unwrap_or_default();
            match handle_notify_trust(&cn, request) {
                Ok(()) => Response::ok(id, Value::Null),
                Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
            }
        }
        POD_HAS_CA_KEY_METHOD => {
            let has = pki::has_mesh_ca_key(&pki_dir());
            value_response(id, &HasCaKeyResult { has_key: has })
        }
        POD_PUSH_CA_KEY_METHOD => {
            let cn = peer_cn.clone().unwrap_or_default();
            match handle_push_ca_key(&cn, request) {
                Ok(()) => Response::ok(id, Value::Null),
                Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
            }
        }
        other => Response::err(
            id,
            ErrorObject::method_not_found(&format!("pod method '{other}' not supported")),
        ),
    }
}

async fn handle_join(request: Request) -> Result<PodJoinResult> {
    let params: PodJoinParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/join params")?,
        None => anyhow::bail!("pod/join requires params"),
    };

    let conn = db::open_default().context("open orca.db")?;
    let token_hash = pdb::hash_token(&params.token);
    if !pdb::redeem_invite(&conn, &token_hash)? {
        anyhow::bail!("invite token invalid, expired, or already used");
    }

    let pki = pki_dir();
    let ca_cert_pem =
        std::fs::read_to_string(pki::mesh_ca_cert_path(&pki)).context("read mesh CA cert")?;
    let (client_cert_pem, _) = pki::sign_peer_csr(
        &pki,
        &params.csr_client_pem,
        &params.joiner_hostname,
        PeerRole::Client,
    )?;
    let (server_cert_pem, _) = pki::sign_peer_csr(
        &pki,
        &params.csr_server_pem,
        &params.joiner_hostname,
        PeerRole::Server,
    )?;

    pdb::upsert_peer(
        &conn,
        &format!("peer.{}", params.joiner_hostname),
        &params.joiner_hostname,
        &ca_cert_pem,
    )?;

    Ok(PodJoinResult {
        client_cert_pem,
        server_cert_pem,
        ca_cert_pem,
    })
}

fn handle_notify_trust(peer_cn: &str, request: Request) -> Result<()> {
    let params: NotifyTrustParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/notify-trust params")?,
        None => anyhow::bail!("pod/notify-trust requires params"),
    };
    let conn = db::open_default()?;
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

fn value_response<T: Serialize>(id: Value, v: &T) -> Response {
    match serde_json::to_value(v) {
        Ok(val) => Response::ok(id, val),
        Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
    }
}

fn gethostname_string() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
