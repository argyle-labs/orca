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
use orca_utils::state::DaemonMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use system::dev::{cmd_dev_disable, cmd_dev_enable, cmd_dev_sync};
use tokio_rustls::server::TlsStream;
use tracing::warn;

use super::{
    AddressChannel, HostAddressingSnapshot, POD_DEV_DISABLE_METHOD, POD_DEV_ENABLE_METHOD,
    POD_DEV_SYNC_METHOD, POD_EXEC_METHOD, POD_PING_METHOD, POD_REPLICATE_EXPORT_METHOD,
    PodDevDisableResult, PodDevEnableResult, PodDevSyncResult, PodExecParams, PodExecResult,
    PodPingResult, ReplicateBundle, db as pdb, pki_dir,
};

const POD_NOTIFY_TRUST_METHOD: &str = "pod/notify-trust";
const POD_HAS_CA_KEY_METHOD: &str = "pod/has-ca-key";
const POD_PUSH_CA_KEY_METHOD: &str = "pod/push-ca-key";
const POD_PEER_LEAVING_METHOD: &str = "pod/peer-leaving";
const POD_PEER_REMOVED_METHOD: &str = "pod/peer-removed";
const POD_PEER_FORGET_METHOD: &str = "pod/peer-forget";
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

    // pod/subscribe takes over the stream for the rest of the connection:
    // one request → ack → streamed events until close. The normal one-shot
    // request/response path below is bypassed.
    if request.method == crate::native::subscribe_wire::METHOD {
        let own_peer_id = format!("peer.{}", system::host_identity::machine_id_short());
        return crate::native::subscribe_wire::serve_session_with_request(
            tls,
            request,
            &own_peer_id,
        )
        .await;
    }

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
                hostname: system::host_identity::hostname().to_string(),
                addressing: build_addressing_snapshot(),
            };
            value_response(id, &result)
        }
        POD_DEV_SYNC_METHOD => match handle_dev_sync().await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
        POD_DEV_ENABLE_METHOD => match handle_dev_enable().await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
        POD_DEV_DISABLE_METHOD => match handle_dev_disable().await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
        POD_EXEC_METHOD => match handle_exec(request).await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
        POD_REPLICATE_EXPORT_METHOD => match handle_replicate_export() {
            Ok(env) => value_response(id, &env),
            Err(e) => Response::err(id, ErrorObject::internal(&e.to_string())),
        },
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
        POD_PEER_REMOVED_METHOD => {
            // Caller (peer_cn) is telling us they've kicked us from their pod.
            // Log it; do NOT mark the caller as departed — that's
            // `pod/peer-leaving`'s job. Reusing this method for kick was the
            // 2026-05-28 bug that departed mint on willow/maple.
            tracing::info!("[pod] peer {peer_cn} removed us from their pod");
            Response::ok(id, Value::Null)
        }
        POD_PEER_FORGET_METHOD => match handle_peer_forget(peer_cn, request) {
            Ok(removed) => value_response(id, &serde_json::json!({ "rows_removed": removed })),
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
        db::ports::mesh_port(),
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

/// Handle `pod/peer-forget`: a pod member (validated by the mTLS CN against the
/// mesh CA) is telling us to purge a stale/orphan peer_id from our local
/// roster. Hard-delete every trace of it so the eviction propagates mesh-wide.
fn handle_peer_forget(peer_cn: &str, request: Request) -> Result<u32> {
    #[derive(serde::Deserialize)]
    struct ForgetParams {
        peer_id: String,
    }
    let params: ForgetParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/peer-forget params")?,
        None => anyhow::bail!("pod/peer-forget requires params"),
    };
    let conn = db::open_default()?;
    let removed = pdb::forget_peer(&conn, &params.peer_id)?;
    tracing::info!(
        "[pod] peer {peer_cn} asked us to forget {} ({removed} rows removed)",
        params.peer_id
    );
    Ok(removed)
}

/// Handle `pod/dev-sync`: if this host is in dev mode, run `cmd_dev_sync`
/// (git pull of the dev checkout). Cargo-watch picks up the new commits and
/// rebuilds. Skipped silently when the host isn't running a dev binary —
/// dev_sync is a no-op on production-only peers, not an error.
async fn handle_dev_sync() -> Result<PodDevSyncResult> {
    let in_dev_mode = orca_utils::state::read()
        .ok()
        .flatten()
        .map(|s| matches!(s.mode, DaemonMode::Dev | DaemonMode::Parked))
        .unwrap_or(false);

    if !in_dev_mode {
        return Ok(PodDevSyncResult {
            status: "skipped".into(),
            detail: Some("peer not in dev mode".into()),
            commits_pulled: None,
        });
    }

    match tokio::task::spawn_blocking(cmd_dev_sync).await {
        Ok(Ok(r)) => Ok(PodDevSyncResult {
            status: "synced".into(),
            detail: Some(r.detail),
            commits_pulled: Some(r.commits_pulled),
        }),
        Ok(Err(e)) => Ok(PodDevSyncResult {
            status: "error".into(),
            detail: Some(e.to_string()),
            commits_pulled: None,
        }),
        Err(e) => Ok(PodDevSyncResult {
            status: "error".into(),
            detail: Some(format!("join error: {e}")),
            commits_pulled: None,
        }),
    }
}

/// Handle `pod/dev-enable`: flip the peer into dev mode (clone repo if
/// missing, park production daemon, spawn cargo-watch).
async fn handle_dev_enable() -> Result<PodDevEnableResult> {
    match tokio::task::spawn_blocking(cmd_dev_enable).await {
        Ok(Ok(r)) => Ok(PodDevEnableResult {
            status: "enabled".into(),
            detail: None,
            repo_path: Some(r.repo_path),
            cloned: Some(r.cloned),
            daemon_parked: Some(r.daemon_parked),
        }),
        Ok(Err(e)) => Ok(PodDevEnableResult {
            status: "error".into(),
            detail: Some(e.to_string()),
            repo_path: None,
            cloned: None,
            daemon_parked: None,
        }),
        Err(e) => Ok(PodDevEnableResult {
            status: "error".into(),
            detail: Some(format!("join error: {e}")),
            repo_path: None,
            cloned: None,
            daemon_parked: None,
        }),
    }
}

/// Handle `pod/dev-disable`: stop cargo-watch and let the production daemon
/// reclaim the port.
async fn handle_dev_disable() -> Result<PodDevDisableResult> {
    match tokio::task::spawn_blocking(cmd_dev_disable).await {
        Ok(Ok(r)) => Ok(PodDevDisableResult {
            status: "disabled".into(),
            detail: None,
            dev_process_stopped: Some(r.dev_process_stopped),
            daemon_reclaimed: Some(r.daemon_reclaimed),
        }),
        Ok(Err(e)) => Ok(PodDevDisableResult {
            status: "error".into(),
            detail: Some(e.to_string()),
            dev_process_stopped: None,
            daemon_reclaimed: None,
        }),
        Err(e) => Ok(PodDevDisableResult {
            status: "error".into(),
            detail: Some(format!("join error: {e}")),
            dev_process_stopped: None,
            daemon_reclaimed: None,
        }),
    }
}

/// Authorization gate for `pod/exec`.
///
/// Target model: every host knows every user (pod-replicated identity
/// registry). Each `pod/exec` request carries the invoking *user's* identity;
/// the executing peer checks that user's role against the tool's required
/// role at request time. The mTLS chain proves the *peer* on the wire is a
/// paired pod member, but that is not, by itself, authorization — admin
/// delegation is per-user, not per-peer.
///
/// Interim (v0): the wire frame carries the caller's *asserted* role
/// (`caller_role`) but no signed proof of it, so we trust the assertion and
/// check it against the tool's required role. This is functional but not yet
/// secure — a modified peer could assert `admin`. S1 of
/// [[project-remote-exec-full-fix]] replaces the bare assertion with an
/// HMAC-signed `caller_token` (caller_user_id, tool, args-hash, expires_at,
/// nonce) verified against the pod-replicated users table (S2).
fn authorize_exec(
    tool: &str,
    remote_ok: bool,
    required_role: &str,
    caller_role: Option<&str>,
) -> Result<()> {
    if !remote_ok {
        anyhow::bail!(
            "pod/exec refused: tool '{tool}' is not in the REMOTE_OK allowlist on this peer"
        );
    }
    if required_role == "any" {
        return Ok(());
    }
    let claim = caller_role.unwrap_or("any");
    if !role_satisfies(claim, required_role) {
        anyhow::bail!(
            "pod/exec refused: tool '{tool}' requires role '{required_role}' but caller asserted \
             '{claim}'"
        );
    }
    Ok(())
}

/// Returns true when the caller's asserted role meets the required role. Order:
/// `any` < `user` < `admin`. Unknown claims compare as `any`.
fn role_rank(role: &str) -> u8 {
    match role {
        "admin" => 2,
        "user" => 1,
        _ => 0,
    }
}

fn role_satisfies(claim: &str, required: &str) -> bool {
    role_rank(claim) >= role_rank(required)
}

/// Handle `pod/exec`: dispatch an allowlisted local tool on this peer's
/// behalf. The mesh mTLS chain already proves the caller is a paired peer;
/// the additional `REMOTE_OK` allowlist check guards which tools that
/// identity may invoke. We relay to our own loopback `/api/tools/<name>`
/// rather than touching the registry directly so the relay benefits from
/// the same auth/log/middleware stack as any other API call.
async fn handle_exec(request: Request) -> Result<PodExecResult> {
    let params: PodExecParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/exec params")?,
        None => anyhow::bail!("pod/exec requires params"),
    };

    authorize_exec(
        &params.tool,
        orca_dispatch::remote_ok::is_allowed(&params.tool),
        orca_dispatch::tool_roles::required_role(&params.tool),
        params.caller_role.as_deref(),
    )?;

    // Direct in-process dispatch through the shared registry — no HTTPS
    // loopback. Authorization is enforced by `authorize_exec` above (REMOTE_OK
    // allowlist + mTLS peer certificate). Admin-role tools tagged remote_ok are
    // reachable from trusted peers; the pod join handshake is the admin gate.
    let result = crate::native::dispatcher::dispatch(&params.tool, params.args.clone())
        .await
        .with_context(|| format!("dispatch pod-relayed tool '{}'", params.tool))?;

    Ok(PodExecResult {
        tool: params.tool,
        result,
    })
}

/// Handle `pod/replicate-export`: return this host's full view of every shared
/// entity registered via `#[derive(Replicated)]`, signed with the host
/// bootstrap key. The mTLS chain already authenticated the requesting peer; the
/// signature lets the puller bind the payload to this host's pinned bootstrap
/// fp before merging.
fn handle_replicate_export() -> Result<pki::SignedEnvelope> {
    let conn = db::open_default()?;
    let entities = replicate::export_all(&conn)?;
    let body = ReplicateBundle {
        peer_id: format!("peer.{}", system::host_identity::machine_id_short()),
        issued_at: chrono::Utc::now().timestamp(),
        entities,
    };
    let signing = pki::load_or_init_bootstrap_key(&pki_dir())?;
    pki::sign_envelope(&signing, &body).context("sign replicate bundle")
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

/// Read the local host's addressing rows and shape them for the
/// `pod/ping` wire. Returns `None` if the DB is unreachable or empty —
/// callers fall back to the legacy single-address path on the receiver
/// side (Slice 4b will start consuming this snapshot).
fn build_addressing_snapshot() -> Option<HostAddressingSnapshot> {
    let conn = db::open_default().ok()?;
    let rows = db::host_addressing::list_host_addressing(&conn).ok()?;
    if rows.is_empty() {
        return None;
    }
    let mut display_name = String::new();
    let mut channels = Vec::with_capacity(rows.len());
    for r in rows {
        if r.key == "display_name" {
            display_name = r.value;
        } else {
            channels.push(AddressChannel {
                kind: r.key,
                value: r.value,
            });
        }
    }
    if display_name.is_empty() {
        display_name = system::host_identity::display_hostname().to_string();
    }
    Some(HostAddressingSnapshot {
        display_name,
        channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_exec_refuses_when_not_remote_ok() {
        let err = authorize_exec("system.dev_enable", false, "any", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("REMOTE_OK allowlist"), "got: {msg}");
        assert!(msg.contains("system.dev_enable"), "got: {msg}");
    }

    #[test]
    fn authorize_exec_refuses_admin_required_without_claim() {
        let err = authorize_exec("system.update.create", true, "admin", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("requires role 'admin'"), "got: {msg}");
        assert!(msg.contains("caller asserted 'any'"), "got: {msg}");
    }

    #[test]
    fn authorize_exec_refuses_admin_required_with_user_claim() {
        let err = authorize_exec("system.update.create", true, "admin", Some("user")).unwrap_err();
        assert!(err.to_string().contains("caller asserted 'user'"));
    }

    #[test]
    fn authorize_exec_passes_admin_required_with_admin_claim() {
        authorize_exec("system.update.create", true, "admin", Some("admin"))
            .expect("admin claim should satisfy admin requirement");
    }

    #[test]
    fn authorize_exec_passes_remote_ok_and_any_role() {
        authorize_exec("fs.search", true, "any", None).expect("should pass");
    }

    #[test]
    fn value_response_ok_serializes_value() {
        #[derive(Serialize)]
        struct Simple {
            x: u32,
        }
        let resp = value_response(Value::Number(1.into()), &Simple { x: 42 });
        // The response must contain the field we serialized
        let text = serde_json::to_string(&resp).unwrap();
        assert!(text.contains("42"), "serialized: {text}");
    }
}
