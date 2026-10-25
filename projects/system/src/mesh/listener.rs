// JSON-RPC envelopes are inherently opaque at the wire boundary; mirroring
// the allow in projects/sdk/rust/src/jsonrpc.rs.
#![allow(clippy::disallowed_types)]

//! Server-side handler for SNI=mesh.orca.local connections.
//!
//! Every method on this surface requires a verified mesh-CA-signed client
//! cert (the plugin host's TLS layer rejects connections without one). The
//! pre-join methods (mesh/offer, mesh/join-confirm) live on a separate SNI
//! (mesh-bootstrap.orca.local) — see super::bootstrap.

use super::refusal::{PeerRefusal, peer_facing_error, refuse, refuse_unless};
use anyhow::{Context, Result};
use dev::mode::{cmd_dev_disable, cmd_dev_enable, cmd_dev_sync};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_rustls::server::TlsStream;
use tracing::warn;
use utils::framing::{read_frame, write_frame};
use utils::jsonrpc::{ErrorObject, Message, Request, Response};
use utils::pki::PeerRole;
use utils::state::DaemonMode;

use super::{
    AddressChannel, HostAddressingSnapshot, MESH_DEV_DISABLE_METHOD, MESH_DEV_ENABLE_METHOD,
    MESH_DEV_SYNC_METHOD, MESH_EXEC_METHOD, MESH_PING_METHOD, MESH_REPLICATE_EXPORT_METHOD,
    MESH_REPLICATE_PUSH_METHOD, MESH_REPLICATE_ROOTS_METHOD, MeshDevDisableResult,
    MeshDevEnableResult, MeshDevSyncResult, MeshExecParams, MeshExecResult, MeshPingResult,
    ReplicatePushResult, ReplicateRootsResult, pki_dir,
};
use db::mesh as pdb;

const MESH_HAS_CA_KEY_METHOD: &str = "mesh/has-ca-key";
const MESH_PUSH_CA_KEY_METHOD: &str = "mesh/push-ca-key";
const MESH_PEER_LEAVING_METHOD: &str = "mesh/peer-leaving";
const MESH_PEER_REMOVED_METHOD: &str = "mesh/peer-removed";
const MESH_PEER_FORGET_METHOD: &str = "mesh/peer-forget";
const MESH_REFRESH_CERT_METHOD: &str = "mesh/refresh-cert";
const MESH_PUSH_CA_STATE_METHOD: &str = "mesh/push-ca-state";

#[derive(Debug, Serialize)]
struct HasCaKeyResult {
    has_key: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    proof: Option<String>,
}

fn valid_ca_holder_nonce(nonce: &str) -> bool {
    nonce.len() <= 64 && uuid::Uuid::try_parse(nonce).is_ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CaKeyPayload {
    pub(crate) cert_pem: String,
    pub(crate) key_pem: String,
    #[serde(default)]
    pub(crate) grant_nonce: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PushCaKeyParams {
    #[serde(flatten)]
    pub(crate) payload: CaKeyPayload,
    #[serde(default)]
    pub(crate) auth: Option<crate::mesh::exec::CaPushAuth>,
}

#[derive(Debug, Deserialize)]
struct RefreshCertParams {
    joiner_hostname: String,
    #[serde(default)]
    envelope: Option<utils::pki::SignedEnvelope>,
    #[serde(default)]
    csr_client_pem: Option<String>,
    #[serde(default)]
    csr_server_pem: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RefreshCsrs {
    pub(crate) joiner_hostname: String,
    pub(crate) csr_client_pem: String,
    pub(crate) csr_server_pem: String,
}

#[derive(Debug, Serialize)]
struct RefreshCertResult {
    client_cert_pem: String,
    server_cert_pem: String,
    ca_cert_pem: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CaStatePayload {
    pub(crate) current_cert_pem: String,
    pub(crate) current_key_pem: String,
    pub(crate) previous_cert_pem: Option<String>,
    pub(crate) previous_key_pem: Option<String>,
    /// Unix timestamp at which the previous slot should be dropped.
    pub(crate) previous_expires_at: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PushCaStateParams {
    #[serde(flatten)]
    pub(crate) payload: CaStatePayload,
    #[serde(default)]
    pub(crate) auth: Option<crate::mesh::exec::CaPushAuth>,
}

pub async fn handle_mesh_connection(
    mut tls: TlsStream<tokio::net::TcpStream>,
    peer_cn: String,
) -> Result<()> {
    let frame_bytes = read_frame(&mut tls).await.context("read mesh frame")?;
    let msg: Message =
        serde_json::from_slice(&frame_bytes).context("parse mesh frame as JSON-RPC")?;
    let request = match msg {
        Message::Request(r) => r,
        Message::Response(_) | Message::Notification(_) => {
            warn!("[mesh] {peer_cn} sent non-request frame; closing");
            return Ok(());
        }
    };

    // mesh/subscribe takes over the stream for the rest of the connection:
    // one request → ack → streamed events until close. The normal one-shot
    // request/response path below is bypassed.
    if request.method == crate::mesh::subscribe_wire::METHOD {
        let own_peer_id = crate::host_identity::machine_id().to_string();
        return crate::mesh::subscribe_wire::serve_session_with_request(tls, request, &own_peer_id)
            .await;
    }

    let response = dispatch(request, &peer_cn).await;

    let envelope = serde_json::to_vec(&response).context("serialize mesh response")?;
    write_frame(&mut tls, &envelope)
        .await
        .context("write mesh response")?;
    Ok(())
}

async fn dispatch(request: Request, peer_cn: &str) -> Response {
    let method = request.method.clone();
    let id = request.id.clone();

    // Departed peers are rejected at the gate — they need to re-pair before
    // we'll talk to them again. mesh/peer-leaving is the one exception: a
    // peer that's already departed can re-send leaving without harm.
    if method != MESH_PEER_LEAVING_METHOD {
        // DB unavailable (fallback open error) or a query error falls through to
        // the method handlers, which will fail with a clearer error.
        let (departed, forgotten) = db::pool::with_pooled_or_open(|conn| {
            Ok((
                pdb::is_peer_departed(conn, peer_cn).unwrap_or(false),
                pdb::is_peer_forgotten(conn, peer_cn).unwrap_or(false),
            ))
        })
        .unwrap_or((false, false));
        if forgotten {
            return Response::err(
                id,
                ErrorObject::method_not_found(&format!(
                    "peer {peer_cn} is not a member of this mesh; re-pair to re-establish it"
                )),
            );
        }
        if departed {
            return Response::err(
                id,
                ErrorObject::method_not_found(&format!(
                    "peer {peer_cn} has departed this mesh; re-pair to re-establish trust"
                )),
            );
        }
    }

    match method.as_str() {
        MESH_PING_METHOD => {
            let result = MeshPingResult {
                peer_id: peer_cn.to_string(),
                version: utils::build_info::version().to_string(),
                hostname: crate::host_identity::hostname().to_string(),
                camel_wire: true,
                bound_envelopes: true,
                addressing: build_addressing_snapshot(),
            };
            value_response(id, &result)
        }
        MESH_DEV_SYNC_METHOD => match handle_dev_sync().await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, peer_facing_error(&method, &e)),
        },
        MESH_DEV_ENABLE_METHOD => match handle_dev_enable().await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, peer_facing_error(&method, &e)),
        },
        MESH_DEV_DISABLE_METHOD => match handle_dev_disable().await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, peer_facing_error(&method, &e)),
        },
        MESH_EXEC_METHOD => match handle_exec(request, peer_cn).await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, exec_error_object(&e)),
        },
        MESH_REPLICATE_EXPORT_METHOD => match handle_replicate_export(peer_cn, request) {
            Ok(env) => value_response(id, &env),
            Err(e) => Response::err(id, peer_facing_error(&method, &e)),
        },
        MESH_REPLICATE_PUSH_METHOD => match handle_replicate_push(peer_cn, request) {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, peer_facing_error(&method, &e)),
        },
        MESH_REPLICATE_ROOTS_METHOD => match handle_replicate_roots() {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, peer_facing_error(&method, &e)),
        },
        MESH_HAS_CA_KEY_METHOD => {
            let pki = pki_dir();
            let nonce = request
                .params
                .as_ref()
                .and_then(|p| p.get("nonce"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if !nonce.is_empty() && !valid_ca_holder_nonce(nonce) {
                return Response::err(
                    id,
                    peer_facing_error(&method, &anyhow::anyhow!("invalid nonce")),
                );
            }
            let has_key = utils::pki::has_mesh_ca_key(&pki);
            let proof = (has_key && !nonce.is_empty())
                .then(|| {
                    crate::mesh::exec::ca_holder_proof(
                        &pki,
                        crate::host_identity::machine_id(),
                        nonce,
                    )
                })
                .flatten();
            value_response(id, &HasCaKeyResult { has_key, proof })
        }
        MESH_PUSH_CA_KEY_METHOD => match handle_push_ca_key(peer_cn, request) {
            Ok(()) => Response::ok(id, Value::Null),
            Err(e) => Response::err(id, peer_facing_error(&method, &e)),
        },
        MESH_PEER_LEAVING_METHOD => match handle_peer_leaving(peer_cn) {
            Ok(()) => Response::ok(id, Value::Null),
            Err(e) => Response::err(id, peer_facing_error(&method, &e)),
        },
        MESH_PEER_REMOVED_METHOD => {
            // Caller (peer_cn) is telling us they've kicked us from their mesh.
            // Log it; do NOT mark the caller as departed — that's
            // `mesh/peer-leaving`'s job. Reusing this method for kick was the
            // 2026-05-28 bug that departed mint on alpha/echo.
            tracing::info!("[mesh] peer {peer_cn} removed us from their mesh");
            Response::ok(id, Value::Null)
        }
        MESH_PEER_FORGET_METHOD => match handle_peer_forget(peer_cn, request) {
            Ok(removed) => value_response(id, &serde_json::json!({ "rows_removed": removed })),
            Err(e) => Response::err(id, peer_facing_error(&method, &e)),
        },
        MESH_REFRESH_CERT_METHOD => match handle_refresh_cert(peer_cn, request) {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, peer_facing_error(&method, &e)),
        },
        MESH_PUSH_CA_STATE_METHOD => match handle_push_ca_state(peer_cn, request) {
            Ok(()) => Response::ok(id, Value::Null),
            Err(e) => Response::err(id, peer_facing_error(&method, &e)),
        },
        other => Response::err(
            id,
            ErrorObject::method_not_found(&format!("mesh method '{other}' not supported")),
        ),
    }
}

fn handle_push_ca_key(peer_cn: &str, request: Request) -> Result<()> {
    let params: PushCaKeyParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse mesh/push-ca-key params")?,
        None => refuse!("mesh/push-ca-key requires params"),
    };
    let pki = pki_dir();
    db::pool::with_pooled_or_open(|conn| {
        admit_ca_push(
            conn,
            &pki,
            MESH_PUSH_CA_KEY_METHOD,
            peer_cn,
            &params.payload,
            params.auth.as_ref(),
        )?;
        refuse_unless!(
            pdb::consumed_promotion_from(conn, peer_cn, &params.payload.grant_nonce)?,
            "mesh/push-ca-key refused: the key push does not answer a promotion grant {peer_cn} \
             issued to this member"
        );
        Ok(())
    })?;
    utils::pki::import_mesh_ca_keypair(&pki, &params.payload.cert_pem, &params.payload.key_pem)
        .context(PeerRefusal("mesh/push-ca-key refused".into()))?;
    drop_ca_keys_if_demoted(
        &pki,
        db::pool::with_pooled_or_open(db::mesh::get_secure)?,
        MESH_PUSH_CA_KEY_METHOD,
    )
}

fn admit_ca_push<T: Serialize>(
    conn: &rusqlite::Connection,
    pki: &std::path::Path,
    method: &str,
    peer_cn: &str,
    payload: &T,
    auth: Option<&crate::mesh::exec::CaPushAuth>,
) -> Result<()> {
    refuse_unless!(
        pdb::is_active_member(conn, peer_cn)?,
        "{method} refused: peer {peer_cn} is not a verified member of this mesh"
    );
    refuse_unless!(
        db::mesh::get_secure(conn)?,
        "{method} refused: this member is insecure and never holds the mesh CA key"
    );
    let now = utils::time::now().unix_seconds();
    let auth = crate::mesh::exec::verify_ca_push(
        pki,
        method,
        crate::host_identity::machine_id(),
        peer_cn,
        payload,
        auth,
        now,
    )
    .map_err(|e| PeerRefusal(utils::pki::with_receiver_time(&format!("{e:#}"), now)))?;
    refuse_unless!(
        pdb::record_mesh_nonce(conn, method, peer_cn, &auth.nonce, auth.expires_at, now)?,
        "{method} refused: the CA-possession proof was already used (replay)"
    );
    Ok(())
}

fn drop_ca_keys_if_demoted(pki: &std::path::Path, secure: bool, method: &str) -> Result<()> {
    if !secure {
        utils::pki::remove_mesh_ca_keys(pki)?;
        refuse!("{method} refused: this member was demoted while the CA key arrived");
    }
    Ok(())
}

fn handle_peer_leaving(peer_cn: &str) -> Result<()> {
    db::pool::with_pooled_or_open(|conn| pdb::mark_peer_departed(conn, peer_cn))?;
    Ok(())
}

/// Handle `mesh/peer-forget`: a peer retiring its own identity from our roster.
/// Forgetting any other peer is a local admin action; a remote peer may only
/// name itself, and never this host.
fn handle_peer_forget(peer_cn: &str, request: Request) -> Result<u32> {
    #[derive(serde::Deserialize)]
    struct ForgetParams {
        peer_id: String,
    }
    let params: ForgetParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse mesh/peer-forget params")?,
        None => refuse!("mesh/peer-forget requires params"),
    };
    check_peer_forget(peer_cn, &params.peer_id, crate::host_identity::machine_id())?;
    let removed = db::pool::with_pooled_or_open(|conn| pdb::forget_peer(conn, &params.peer_id))?;
    crate::mesh::peer_info::remove(&params.peer_id);
    tracing::info!(
        "[mesh] peer {peer_cn} asked us to forget {} ({removed} rows removed)",
        params.peer_id
    );
    Ok(removed)
}

fn check_peer_forget(peer_cn: &str, target: &str, own_id: &str) -> Result<()> {
    refuse_unless!(
        target != own_id,
        "mesh/peer-forget refused: a peer cannot make this host forget itself"
    );
    refuse_unless!(
        target == peer_cn,
        "mesh/peer-forget refused: peer {peer_cn} may only retire itself, not {target}"
    );
    Ok(())
}

/// Handle `mesh/dev-sync`: if this host is in dev mode, run `cmd_dev_sync`
/// (git pull of the dev checkout). Cargo-watch picks up the new commits and
/// rebuilds. Skipped silently when the host isn't running a dev binary —
/// dev_sync is a no-op on production-only peers, not an error.
async fn handle_dev_sync() -> Result<MeshDevSyncResult> {
    let in_dev_mode = utils::state::read()
        .ok()
        .flatten()
        .map(|s| matches!(s.mode, DaemonMode::Dev | DaemonMode::Parked))
        .unwrap_or(false);

    handle_dev_sync_with(in_dev_mode).await
}

/// Decision half of `mesh/dev-sync`, with the ambient daemon-mode read lifted
/// out. Keeps the skip path testable on a host that is itself in dev mode.
async fn handle_dev_sync_with(in_dev_mode: bool) -> Result<MeshDevSyncResult> {
    if !in_dev_mode {
        return Ok(MeshDevSyncResult {
            status: "skipped".into(),
            detail: Some("peer not in dev mode".into()),
            commits_pulled: None,
        });
    }

    match tokio::task::spawn_blocking(cmd_dev_sync).await {
        Ok(Ok(r)) => Ok(MeshDevSyncResult {
            status: "synced".into(),
            detail: Some(r.detail),
            commits_pulled: Some(r.commits_pulled),
        }),
        Ok(Err(e)) => Ok(MeshDevSyncResult {
            status: "error".into(),
            detail: Some(e.to_string()),
            commits_pulled: None,
        }),
        Err(e) => Ok(MeshDevSyncResult {
            status: "error".into(),
            detail: Some(format!("join error: {e}")),
            commits_pulled: None,
        }),
    }
}

/// Handle `mesh/dev-enable`: flip the peer into dev mode (clone repo if
/// missing, park production daemon, spawn cargo-watch).
async fn handle_dev_enable() -> Result<MeshDevEnableResult> {
    let token = crate::update::resolve_github_token();
    match tokio::task::spawn_blocking(move || cmd_dev_enable(&token)).await {
        Ok(Ok(r)) => Ok(MeshDevEnableResult {
            status: "enabled".into(),
            detail: None,
            repo_path: Some(r.repo_path),
            cloned: Some(r.cloned),
            daemon_parked: Some(r.daemon_parked),
        }),
        Ok(Err(e)) => Ok(MeshDevEnableResult {
            status: "error".into(),
            detail: Some(e.to_string()),
            repo_path: None,
            cloned: None,
            daemon_parked: None,
        }),
        Err(e) => Ok(MeshDevEnableResult {
            status: "error".into(),
            detail: Some(format!("join error: {e}")),
            repo_path: None,
            cloned: None,
            daemon_parked: None,
        }),
    }
}

/// Handle `mesh/dev-disable`: stop cargo-watch and let the production daemon
/// reclaim the port.
async fn handle_dev_disable() -> Result<MeshDevDisableResult> {
    match tokio::task::spawn_blocking(cmd_dev_disable).await {
        Ok(Ok(r)) => Ok(MeshDevDisableResult {
            status: "disabled".into(),
            detail: None,
            dev_process_stopped: Some(r.dev_process_stopped),
            daemon_reclaimed: Some(r.daemon_reclaimed),
        }),
        Ok(Err(e)) => Ok(MeshDevDisableResult {
            status: "error".into(),
            detail: Some(e.to_string()),
            dev_process_stopped: None,
            daemon_reclaimed: None,
        }),
        Err(e) => Ok(MeshDevDisableResult {
            status: "error".into(),
            detail: Some(format!("join error: {e}")),
            dev_process_stopped: None,
            daemon_reclaimed: None,
        }),
    }
}

/// Whether `tool` is callable at all over `mesh/exec` (i.e. not `local_only`)
/// and whether it needs an authenticated caller (role-gated). Everything is
/// REMOTE_OK by default; only `local_only` opt-outs are refused. Pure so the
/// gate logic is unit-testable without a DB or PKI.
fn remote_ok_gate(tool: &str, remote_ok: bool, required_role: &str) -> Result<bool> {
    if !remote_ok {
        refuse!("mesh/exec refused: tool '{tool}' is local_only on this peer");
    }
    // "any" tools are authorized by mesh membership alone (the mTLS chain already
    // proved a paired peer). Role-gated tools additionally require a verified
    // caller token bound to a user whose replicated role satisfies the gate.
    Ok(required_role != "any")
}

/// `role = "any"` tools that still change state, so no unauthenticated peer
/// may drive them.
const MESH_ANY_ROLE_DENY: &[&str] = &["auth.login"];

fn is_mutating_tool(tool: &str) -> bool {
    dispatch::tool_roles::is_data_mutation(tool)
        || dispatch::tool_roles::plugin_execute_gated(tool)
        || dispatch::tool_roles::plugin_dry_run_aware(tool)
        || dispatch::tool_roles::plugin_read_unconfirmed(tool)
        || dispatch::execute_gated_names().contains(&tool)
}

/// Deny-by-default role resolution for `mesh/exec`: an unroutable tool is
/// refused, and an `any`-role tool runs without a caller only when it is a
/// read.
fn remote_role_gate(
    tool: &str,
    known_role: Option<&'static str>,
    mutating: bool,
) -> Result<&'static str> {
    let role = known_role.ok_or_else(|| {
        PeerRefusal(format!(
            "mesh/exec refused: tool '{tool}' is not routable on this peer"
        ))
    })?;
    refuse_unless!(
        role != "any" || !(mutating || MESH_ANY_ROLE_DENY.contains(&tool)),
        "mesh/exec refused: tool '{tool}' changes state and has no role gate; it cannot run \
         for a remote peer without a signed caller"
    );
    Ok(role)
}

/// Per-action local-only guard for `mesh/exec`. `system.mesh.update` and `system.mesh.delete`
/// are remote-dispatchable as a whole, but two of their actions repair or exit
/// THIS host's own membership (formerly the `local_only` `mesh.recover` /
/// `mesh.leave` verbs) and must never be driven by a remote peer. The mTLS chain
/// proving a paired peer is not enough — these act on local identity, so they
/// stay local-origin only. Pure over `(tool, args)` so it's unit-testable.
fn reject_remote_local_only_action(tool: &str, args: &Value) -> Result<()> {
    let action = args.get("action").and_then(|v| v.as_str());
    let demote = args.get("secure") == Some(&Value::Bool(false));
    let forbidden = matches!(
        (tool, action),
        ("system.mesh.update", Some("recover" | "rejoin")) | ("system.mesh.delete", Some("leave"))
    ) || (tool == "system.mesh.update"
        && matches!(action, None | Some("settings"))
        && demote);
    refuse_unless!(
        !forbidden,
        "mesh/exec refused: '{tool}' action '{}' is local-only and cannot be invoked by a remote peer",
        action.unwrap_or_default()
    );
    // `plugin.create action=invoke` names a plugin verb in its args; without
    // this a local-only plugin verb is reachable through the remote-OK seam.
    if tool == "plugin.create" && action == Some("invoke") {
        let inner = args
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        refuse_unless!(
            dispatch::remote_ok::is_allowed(inner),
            "mesh/exec refused: plugin verb '{inner}' is local_only on this peer"
        );
    }
    Ok(())
}

fn reject_insecure_secret_source(tool: &str, secure: impl FnOnce() -> Result<bool>) -> Result<()> {
    if tool != "secrets.detail" {
        return Ok(());
    }
    refuse_unless!(
        secure()?,
        "mesh/exec refused: '{tool}' — an insecure member never serves secret values to peers"
    );
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct Presented<'a> {
    caller_token: Option<&'a utils::pki::SignedEnvelope>,
    ca_proof: Option<&'a str>,
}

/// Zero-trust authorization gate for a role-gated `mesh/exec` call.
///
/// The mTLS chain proves which *peer* is on the wire; this proves which *user*
/// the call acts for and that the executing host independently agrees they may.
/// Nothing the caller asserts is trusted — every decision is re-derived here:
///   1. the caller token signature verifies AND covers this exact tool + args,
///      and has not expired (`caller_token::verify`);
///   2. the token's signer fingerprint equals the authenticated peer's *pinned*
///      `mesh_peers.pubkey_fp` (a valid sig from an unpinned key is rejected);
///   3. the token nonce has not been seen before (replay guard);
///   4. the effective role is the LOWER of this host's own (replicated) `users`
///      row for `caller_user_id` and the token's signed `role`: the user row
///      caps a forged-high claim, the token role caps a narrowly-scoped API
///      token issued by an admin. Unknown user or insufficient role → refuse.
///
/// Returns that locally-resolved user, the only identity a mesh call may hand
/// on to a plugin.
///
/// See feedback_zero_trust_no_blind_trust.md + project_remote_exec_full_fix.md.
fn authorize_role_gated(
    conn: &rusqlite::Connection,
    peer_cn: &str,
    tool: &str,
    args: &serde_json::Value,
    required_role: &str,
    presented: Presented<'_>,
    now: i64,
) -> Result<contract::CallerIdentity> {
    let Presented {
        caller_token,
        ca_proof,
    } = presented;
    let env = caller_token.ok_or_else(|| {
        PeerRefusal(format!(
            "mesh/exec refused: tool '{tool}' requires role '{required_role}' but no signed caller \
             token was presented"
        ))
    })?;

    let verified = crate::mesh::caller_token::verify(
        env,
        tool,
        args,
        peer_cn,
        crate::host_identity::machine_id(),
        now,
        crate::mesh::trust_registry::unbound_allowed(conn, &pki_dir(), peer_cn),
    )
    .map_err(|e| {
        PeerRefusal(utils::pki::with_receiver_time(
            &format!("mesh/exec refused: caller token verification failed: {e:#}"),
            now,
        ))
    })?;

    let pinned = db::mesh::pinned_pubkey_fp(conn, peer_cn)?.ok_or_else(|| {
        PeerRefusal(format!(
            "mesh/exec refused: peer {peer_cn} has no pinned bootstrap key to verify against"
        ))
    })?;
    refuse_unless!(
        verified.signer_fp == pinned,
        "mesh/exec refused: caller token signer fp does not match peer {peer_cn}'s pinned key"
    );

    let user =
        identities::users::find_by_id(conn, &verified.claims.caller_user_id)?.ok_or_else(|| {
            PeerRefusal(format!(
                "mesh/exec refused: caller user {} is not in this host's replicated users",
                verified.claims.caller_user_id
            ))
        })?;
    refuse_unless!(
        !dispatch::tool_roles::is_custody(custody_target(tool, args))
            || crate::mesh::exec::proves_ca_possession(
                &pki_dir(),
                peer_cn,
                &verified.nonce,
                ca_proof,
            ),
        "mesh/exec refused: '{tool}' is not marked remote_admin and peer {peer_cn} presented no \
         valid mesh CA proof"
    );
    let role = effective_role(&user.role, &verified.claims.role);
    refuse_unless!(
        role_satisfies(role, required_role),
        "mesh/exec refused: tool '{tool}' requires role '{required_role}' but user {} acts with \
         role '{role}'",
        user.username
    );
    crate::mesh::caller_token::check_replay(
        conn,
        peer_cn,
        &verified.nonce,
        verified.expires_at,
        now,
    )?;
    if env.binding.is_some() {
        pdb::mark_bound_envelopes(conn, peer_cn)?;
    }
    let role = role.to_string();
    Ok(contract::CallerIdentity {
        user_id: user.id,
        username: user.username,
        role,
        // A caller token carries no capability grant; unknown means no.
        can_mutate: false,
    })
}

fn custody_target<'a>(tool: &'a str, args: &'a Value) -> &'a str {
    match (tool, args.get("action").and_then(Value::as_str)) {
        ("plugin.create", Some("invoke")) => {
            args.get("tool").and_then(Value::as_str).unwrap_or(tool)
        }
        _ => tool,
    }
}

use dispatch::tool_roles::role_rank;

/// The lower-ranked of the replicated user role and the caller token's role.
fn effective_role<'a>(user_role: &'a str, token_role: &'a str) -> &'a str {
    if role_rank(token_role) < role_rank(user_role) {
        token_role
    } else {
        user_role
    }
}

fn role_satisfies(role: &str, required: &str) -> bool {
    dispatch::tool_roles::satisfies(role, required)
}

/// A relayed tool's failure as a JSON-RPC error. A classified `OrcaError`
/// keeps its kind and code in `data`, so the caller re-raises it with the same
/// kind, and so the same HTTP status, rather than as a generic internal error.
fn exec_error_object(e: &anyhow::Error) -> ErrorObject {
    // `downcast_ref` sees through anyhow context layers; the chain walk finds
    // one held as a std `source()`.
    let Some(oe) = e.downcast_ref::<contract::OrcaError>().or_else(|| {
        e.chain()
            .find_map(|c| c.downcast_ref::<contract::OrcaError>())
    }) else {
        if e.downcast_ref::<RelayedToolFailed>().is_some() {
            return ErrorObject::internal(&format!("{MESH_EXEC_METHOD} failed"));
        }
        return peer_facing_error(MESH_EXEC_METHOD, e);
    };
    ErrorObject {
        code: if oe.kind == contract::ErrorKind::Invalid {
            -32602
        } else {
            -32603
        },
        message: utils::scrub::scrub_line(&oe.message).into_owned(),
        data: Some(serde_json::json!({ "kind": oe.kind, "code": oe.code })),
    }
}

fn relayed_tool_error(tool: &str, e: anyhow::Error) -> anyhow::Error {
    if e.chain()
        .any(|c| c.downcast_ref::<contract::OrcaError>().is_some())
    {
        return e;
    }
    warn!("[mesh] dispatch mesh-relayed tool '{tool}': {e:#}");
    anyhow::Error::new(RelayedToolFailed)
}

#[derive(Debug)]
struct RelayedToolFailed;

impl std::fmt::Display for RelayedToolFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("mesh-relayed tool failed")
    }
}

impl std::error::Error for RelayedToolFailed {}

/// Handle `mesh/exec`: dispatch an allowlisted local tool on this peer's behalf.
/// The mesh mTLS chain proves the caller is a paired peer; the REMOTE_OK
/// allowlist guards which tools are reachable, and role-gated tools require a
/// verified caller token bound to a replicated user (zero-trust, no asserted
/// role). Dispatch is direct in-process through the shared registry.
async fn handle_exec(request: Request, peer_cn: &str) -> Result<MeshExecResult> {
    let params: MeshExecParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse mesh/exec params")?,
        None => refuse!("mesh/exec requires params"),
    };

    // Reachability is default-allow: every tool — core, plugin, unit — is
    // callable cross-host unless it opted out via `local_only`. `is_allowed` is
    // the denylist check; `required_role` is the orthogonal auth-tightening axis,
    // covering plugin tools through the roles their manifests declared.
    reject_remote_local_only_action(&params.tool, &params.args)?;
    reject_insecure_secret_source(&params.tool, || {
        db::pool::with_pooled_or_open(db::mesh::get_secure)
    })?;

    let required_role = remote_role_gate(
        &params.tool,
        dispatch::tool_roles::known_role(&params.tool),
        is_mutating_tool(&params.tool),
    )?;
    let needs_auth = remote_ok_gate(
        &params.tool,
        dispatch::remote_ok::is_allowed(&params.tool),
        required_role,
    )?;
    let caller = if needs_auth {
        Some(db::pool::with_pooled_or_open(|conn| {
            authorize_role_gated(
                conn,
                peer_cn,
                &params.tool,
                &params.args,
                required_role,
                Presented {
                    caller_token: params.caller_token.as_ref(),
                    ca_proof: params.ca_proof.as_deref(),
                },
                utils::time::now().unix_seconds(),
            )
        })?)
    } else {
        None
    };

    let ca_proven = caller.is_some() && dispatch::tool_roles::is_custody(&params.tool);
    let result = crate::mesh::dispatcher::dispatch_proven(
        &params.tool,
        params.args.clone(),
        params.correlation_id.clone(),
        caller,
        peer_cn,
        ca_proven,
    )
    .await
    .map_err(|e| relayed_tool_error(&params.tool, e))?;

    Ok(MeshExecResult {
        tool: params.tool,
        result,
    })
}

/// Handle `mesh/replicate-export`: return this host's full view of every shared
/// entity registered via `#[derive(Replicated)]`, signed with the host
/// bootstrap key. The mTLS chain already authenticated the requesting peer; the
/// signature lets the puller bind the payload to this host's pinned bootstrap
/// fp before merging.
/// Handle `mesh/replicate-roots`: return this host's per-entity content roots.
/// No signature needed — roots are opaque hashes; the engine only uses them
/// to short-circuit identical-state bundle fetches. mTLS already authenticated
/// the caller as a paired peer.
fn handle_replicate_roots() -> Result<ReplicateRootsResult> {
    let roots = db::pool::with_pooled_or_open(db::replicate::roots)?;
    Ok(ReplicateRootsResult { roots })
}

#[derive(Debug, Deserialize)]
struct ReplicateExportParams {
    nonce: String,
}

fn handle_replicate_export(peer_cn: &str, request: Request) -> Result<utils::pki::SignedEnvelope> {
    let params: ReplicateExportParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse mesh/replicate-export params")?,
        None => refuse!("mesh/replicate-export requires a request nonce"),
    };
    refuse_unless!(
        valid_ca_holder_nonce(&params.nonce),
        "mesh/replicate-export refused: invalid request nonce"
    );
    let entities = db::pool::with_pooled_or_open(db::replicate::export_all)?;
    crate::mesh::transport::sign_bundle(
        entities,
        MESH_REPLICATE_EXPORT_METHOD,
        peer_cn,
        &params.nonce,
    )
}

/// Handle `mesh/replicate-push`: caller (the writer) sent us a signed bundle.
/// Verify against the caller's pinned bootstrap fp, then hand off to the db
/// engine to merge. Trust model mirrors `mesh/replicate-export` (mTLS proves
/// transport, signature proves bundle origin).
fn handle_replicate_push(peer_cn: &str, request: Request) -> Result<ReplicatePushResult> {
    let envelope: utils::pki::SignedEnvelope = match request.params {
        Some(v) => serde_json::from_value(v).context("parse mesh/replicate-push params")?,
        None => refuse!("mesh/replicate-push requires params"),
    };
    let now = utils::time::now().unix_seconds();
    let entities = db::pool::with_pooled_or_open(|conn| {
        refuse_unless!(
            pdb::is_active_member(conn, peer_cn)?,
            "mesh/replicate-push refused: peer {peer_cn} is not a verified member of this mesh"
        );
        if envelope.binding.is_none()
            && crate::mesh::trust_registry::unbound_allowed(conn, &pki_dir(), peer_cn)
        {
            let pinned = pdb::pinned_pubkey_fp(conn, peer_cn)?.ok_or_else(|| {
                PeerRefusal(format!(
                    "mesh/replicate-push refused: peer {peer_cn} has no pinned bootstrap fp"
                ))
            })?;
            return crate::mesh::transport::verify_legacy_bundle(&envelope, &pinned);
        }
        let bundle: super::ReplicateBundle =
            admit_bound_from_member(conn, &envelope, MESH_REPLICATE_PUSH_METHOD, peer_cn, now)?;
        Ok(bundle.entities)
    })?;
    let merged = db::replicate_engine::merge_into_local(peer_cn, entities)?;
    if merged > 0 {
        tracing::info!("[replicate.push.recv] merged {merged} row(s) from {peer_cn}");
    }
    Ok(ReplicatePushResult { merged })
}

fn handle_push_ca_state(peer_cn: &str, request: Request) -> Result<()> {
    let params: PushCaStateParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse mesh/push-ca-state params")?,
        None => refuse!("mesh/push-ca-state requires params"),
    };
    db::pool::with_pooled_or_open(|conn| {
        let pki_d = pki_dir();
        admit_ca_push(
            conn,
            &pki_d,
            MESH_PUSH_CA_STATE_METHOD,
            peer_cn,
            &params.payload,
            params.auth.as_ref(),
        )?;
        let local_current = std::fs::read_to_string(utils::pki::mesh_ca_cert_path(&pki_d))
            .context("mesh/push-ca-state: read local mesh CA cert")?;
        let local_previous =
            std::fs::read_to_string(utils::pki::mesh_ca_previous_cert_path(&pki_d)).ok();
        let params = &params.payload;
        check_ca_rotation(
            params,
            &local_current,
            local_previous.as_deref(),
            &utils::pki::retired_mesh_ca_fingerprints(&pki_d).context(PeerRefusal(
                "mesh/push-ca-state refused: retired CA history is unreadable".into(),
            ))?,
            utils::time::now().unix_seconds(),
        )?;
        utils::pki::import_mesh_ca_state(
            &pki_d,
            &params.current_cert_pem,
            &params.current_key_pem,
            params.previous_cert_pem.as_deref(),
            params.previous_key_pem.as_deref(),
        )?;
        pdb::set_ca_previous_expires_at(conn, params.previous_expires_at)?;
        drop_ca_keys_if_demoted(
            &pki_d,
            db::mesh::get_secure(conn)?,
            MESH_PUSH_CA_STATE_METHOD,
        )
    })
}

const MAX_CA_OVERLAP_SECS: i64 = 90 * 86_400;

/// A pushed CA state is accepted only as a forward rotation of the CA this host
/// pins now: the pushed `previous` slot must be our current CA cert, and its key
/// must be that cert's private key, which proves the sender holds the current
/// CA. Any other change of CA identity, a rollback to our previous CA or to any
/// CA in this host's retired history, or an unbounded overlap window is refused.
fn check_ca_rotation(
    params: &CaStatePayload,
    local_current: &str,
    local_previous: Option<&str>,
    retired: &[String],
    now: i64,
) -> Result<()> {
    let (Some(prev_cert), Some(prev_key)) = (
        params.previous_cert_pem.as_deref(),
        params.previous_key_pem.as_deref(),
    ) else {
        refuse!("mesh/push-ca-state refused: a CA state push must be a rotation of the current CA");
    };
    refuse_unless!(
        prev_cert.trim() == local_current.trim(),
        "mesh/push-ca-state refused: rotation does not chain from this host's current mesh CA"
    );
    utils::pki::ensure_key_matches_cert(local_current, prev_key).context(PeerRefusal(
        "mesh/push-ca-state refused: sender did not prove possession of the current CA key".into(),
    ))?;
    let new_current = params.current_cert_pem.trim();
    refuse_unless!(
        new_current != local_current.trim(),
        "mesh/push-ca-state refused: pushed CA is already this host's current CA"
    );
    refuse_unless!(
        local_previous.is_none_or(|p| p.trim() != new_current),
        "mesh/push-ca-state refused: pushed CA is this host's previous CA (downgrade)"
    );
    let new_fp = utils::pki::cert_fingerprint(new_current).context(PeerRefusal(
        "mesh/push-ca-state refused: pushed current cert is unreadable".into(),
    ))?;
    refuse_unless!(
        !retired.contains(&new_fp),
        "mesh/push-ca-state refused: pushed CA was previously retired by this host (downgrade)"
    );
    utils::pki::ensure_valid_ca_cert(new_current, now).context(PeerRefusal(
        "mesh/push-ca-state refused: pushed current cert is not a valid CA".into(),
    ))?;
    let Some(exp) = params.previous_expires_at else {
        refuse!("mesh/push-ca-state refused: previous CA slot requires previous_expires_at");
    };
    refuse_unless!(
        exp > now && exp - now <= MAX_CA_OVERLAP_SECS,
        "mesh/push-ca-state refused: previous CA overlap must end within 90 days"
    );
    Ok(())
}

/// Sign refreshed CSRs for a peer that doesn't hold the mesh CA key itself
/// (non-secure joiner that needs rotation before its 30-day cert expires).
/// The mTLS handshake authenticates the CN; the caller must also have a
/// non-departed `mesh_peers` row, as the bootstrap refresh requires, and no
/// forget-tombstone. The dispatch gate only rejects rows marked departed, so
/// it lets an unknown CN (forgotten, or never paired with this host) through.
fn handle_refresh_cert(peer_cn: &str, request: Request) -> Result<RefreshCertResult> {
    anyhow::ensure!(
        utils::pki::has_mesh_ca_key(&pki_dir()),
        "this host does not have the mesh CA key — cannot refresh peer certs"
    );
    let params: RefreshCertParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse mesh/refresh-cert params")?,
        None => refuse!("mesh/refresh-cert requires params"),
    };

    // Enforce that the joiner identifier matches the authenticated CN. CN is
    // `<machine_id_short>`; the param is named `joiner_hostname` for wire
    // compat but now carries the stable machine_id, not the OS hostname.
    let expected_cn = params.joiner_hostname.clone();
    refuse_unless!(
        peer_cn == expected_cn,
        "refresh refused: cert CN ({peer_cn}) does not match joiner_hostname ({expected_cn})"
    );
    let active = db::pool::with_pooled_or_open(|conn| {
        Ok(pdb::peer_exists(conn, peer_cn)?
            && !pdb::is_peer_departed(conn, peer_cn)?
            && !pdb::is_peer_forgotten(conn, peer_cn)?)
    })?;
    refuse_unless!(
        active,
        "refresh refused: {peer_cn} is not a known active mesh peer"
    );
    let csrs: RefreshCsrs = db::pool::with_pooled_or_open(|conn| match params.envelope.as_ref() {
        Some(envelope) => admit_bound_from_member(
            conn,
            envelope,
            MESH_REFRESH_CERT_METHOD,
            peer_cn,
            utils::time::now().unix_seconds(),
        ),
        None => {
            refuse_unless!(
                crate::mesh::trust_registry::unbound_allowed(conn, &pki_dir(), peer_cn),
                "refresh refused: the request envelope is not bound to a receiver and nonce"
            );
            Ok(RefreshCsrs {
                joiner_hostname: params.joiner_hostname.clone(),
                csr_client_pem: params
                    .csr_client_pem
                    .clone()
                    .context("refresh: missing csr_client_pem")?,
                csr_server_pem: params
                    .csr_server_pem
                    .clone()
                    .context("refresh: missing csr_server_pem")?,
            })
        }
    })?;
    refuse_unless!(
        csrs.joiner_hostname == peer_cn,
        "refresh refused: signed joiner_hostname does not match cert CN ({peer_cn})"
    );

    let pki_d = pki_dir();
    let (client_cert_pem, ca_cert_pem) = utils::pki::sign_peer_csr(
        &pki_d,
        &csrs.csr_client_pem,
        &csrs.joiner_hostname,
        PeerRole::Client,
    )?;
    let (server_cert_pem, _) = utils::pki::sign_peer_csr(
        &pki_d,
        &csrs.csr_server_pem,
        &csrs.joiner_hostname,
        PeerRole::Server,
    )?;
    Ok(RefreshCertResult {
        client_cert_pem,
        server_cert_pem,
        ca_cert_pem,
    })
}

/// Verify `envelope` is bound from `peer_cn` to this host for `method`, signed
/// by `peer_cn`'s pinned bootstrap key, and record its nonce.
fn admit_bound_from_member<T: serde::de::DeserializeOwned>(
    conn: &rusqlite::Connection,
    envelope: &utils::pki::SignedEnvelope,
    method: &str,
    peer_cn: &str,
    now: i64,
) -> Result<T> {
    let (body, verifying, binding) = utils::pki::verify_bound_envelope::<T>(
        envelope,
        method,
        peer_cn,
        crate::host_identity::machine_id(),
        now,
    )
    .map_err(|e| {
        PeerRefusal(utils::pki::with_receiver_time(
            &format!("{method} refused: {e:#}"),
            now,
        ))
    })?;
    let pinned = pdb::pinned_pubkey_fp(conn, peer_cn)?;
    refuse_unless!(
        pinned.as_deref() == Some(utils::pki::bootstrap_pubkey_fingerprint(&verifying).as_str()),
        "{method} refused: envelope is not signed by {peer_cn}'s pinned bootstrap key"
    );
    refuse_unless!(
        pdb::record_mesh_nonce(
            conn,
            method,
            peer_cn,
            &binding.nonce,
            binding.expires_at,
            now
        )?,
        "{method} refused: envelope was already delivered (replay)"
    );
    pdb::mark_bound_envelopes(conn, peer_cn)?;
    Ok(body)
}

fn value_response<T: Serialize>(id: Value, v: &T) -> Response {
    match serde_json::to_value(v) {
        Ok(val) => Response::ok(id, val),
        Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
    }
}

/// Read the local host's addressing rows and shape them for the
/// `mesh/ping` wire. Returns `None` if the DB is unreachable or empty —
/// callers fall back to the legacy single-address path on the receiver
/// side (Slice 4b will start consuming this snapshot).
fn build_addressing_snapshot() -> Option<HostAddressingSnapshot> {
    let rows = db::pool::with_pooled_or_open(db::host_addressing::list_host_addressing).ok()?;
    if rows.is_empty() {
        return None;
    }
    let mut display_name = String::new();
    let mut channels = Vec::with_capacity(rows.len());
    for r in rows {
        if r.kind == "display_name" {
            display_name = r.value;
        } else {
            let kind_label = crate::system_info::labels::addr_kind_label(&r.kind);
            channels.push(AddressChannel {
                kind: r.kind,
                kind_label,
                value: r.value,
            });
        }
    }
    if display_name.is_empty() {
        display_name = crate::host_identity::display_hostname().to_string();
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
    fn a_relayed_invalid_args_error_stays_invalid_across_the_mesh() {
        let e = anyhow::Error::from(contract::OrcaError::invalid("invalid args for t: id: bad"))
            .context("dispatch mesh-relayed tool 't'");
        let obj = exec_error_object(&e);
        assert_eq!(obj.code, -32602);
        let back = crate::mesh::peer_error("10.0.0.1", obj);
        let oe = back
            .downcast_ref::<contract::OrcaError>()
            .expect("re-raised as an OrcaError");
        assert_eq!(oe.kind, contract::ErrorKind::Invalid);
        assert_eq!(oe.message, "invalid args for t: id: bad");
        assert!(back.downcast_ref::<crate::mesh::PeerAnswered>().is_some());
        // A relay that re-raises it keeps the kind for the next hop.
        let relayed = back.context("dispatch mesh-relayed tool 't'");
        assert_eq!(exec_error_object(&relayed).code, -32602);

        let plain = exec_error_object(&anyhow::anyhow!("boom"));
        assert_eq!(plain.code, -32603);
        let plain = crate::mesh::peer_error("10.0.0.1", plain);
        assert!(plain.downcast_ref::<contract::OrcaError>().is_none());
        assert!(plain.downcast_ref::<crate::mesh::PeerAnswered>().is_some());
    }

    #[test]
    fn a_classified_relayed_tool_failure_reaches_the_caller() {
        let e = relayed_tool_error(
            "container.exec",
            anyhow::Error::from(contract::OrcaError::forbidden(
                "exec in syncthing refused: the container is not part of a registered stack",
            )),
        );
        let obj = exec_error_object(&e);
        assert_eq!(
            obj.message,
            "exec in syncthing refused: the container is not part of a registered stack"
        );
        assert_eq!(obj.code, -32603);
        assert_eq!(
            obj.data.unwrap()["kind"],
            serde_json::json!(contract::ErrorKind::Forbidden)
        );
        let invalid = relayed_tool_error(
            "t",
            anyhow::Error::from(contract::OrcaError::invalid("invalid args for t: id: bad")),
        );
        assert_eq!(exec_error_object(&invalid).code, -32602);
    }

    #[test]
    fn an_untyped_relayed_tool_failure_is_hidden() {
        let e = relayed_tool_error(
            "container.exec",
            anyhow::anyhow!("open /secret/path/key.pem failed").context("token=ghp_abc123SECRET"),
        );
        let obj = exec_error_object(&e);
        assert_eq!(obj.message, "internal error: mesh/exec failed");
        assert!(obj.data.is_none());
    }

    #[test]
    fn a_classified_relayed_tool_failure_drops_its_local_context() {
        let e = relayed_tool_error(
            "container.exec",
            anyhow::Error::from(contract::OrcaError::forbidden("exec refused"))
                .context("open /secret/path/key.pem"),
        );
        let obj = exec_error_object(&e);
        assert_eq!(obj.message, "exec refused");
        assert!(!obj.message.contains("/secret/path"));
    }

    #[test]
    fn a_classified_relayed_tool_failure_reaches_the_peer_scrubbed() {
        let e = relayed_tool_error(
            "container.exec",
            anyhow::Error::from(contract::OrcaError::forbidden(
                "exec refused: authorization: Bearer hunter2secret",
            )),
        );
        let msg = exec_error_object(&e).message;
        assert!(!msg.contains("hunter2secret"), "{msg}");
        assert!(msg.starts_with("exec refused: "), "{msg}");
    }

    #[test]
    fn two_tier_insecure_member_never_serves_secret_values_to_peers() {
        assert!(reject_insecure_secret_source("secrets.detail", || Ok(false)).is_err());
        assert!(reject_insecure_secret_source("secrets.detail", || Ok(true)).is_ok());
        assert!(reject_insecure_secret_source("secrets.upsert", || Ok(false)).is_ok());
        assert!(
            reject_insecure_secret_source("secrets.detail", || Err(anyhow::anyhow!("db"))).is_err()
        );
    }

    #[test]
    fn remote_ok_gate_refuses_local_only() {
        // Only a local_only tool (remote_ok=false at the gate) is refused.
        let err = remote_ok_gate("system.dev_enable", false, "any").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("local_only"), "got: {msg}");
        assert!(msg.contains("system.dev_enable"), "got: {msg}");
    }

    #[test]
    fn remote_ok_gate_any_role_needs_no_auth() {
        // Reachable + "any" → mesh membership is sufficient, no token needed.
        assert!(!remote_ok_gate("fs.search", true, "any").unwrap());
    }

    #[test]
    fn remote_ok_gate_admin_role_needs_auth() {
        // Reachable + role-gated → a verified caller token is required.
        assert!(remote_ok_gate("schedule.owner.list", true, "admin").unwrap());
    }

    #[test]
    fn role_satisfies_ranks_admin_above_member() {
        assert!(role_satisfies("admin", "admin"));
        assert!(role_satisfies("admin", "any"));
        assert!(!role_satisfies("member", "admin"));
        assert!(role_satisfies("member", "any"));
        assert!(!role_satisfies("bogus", "admin"));
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

    #[test]
    fn value_response_serialization_error_becomes_err_response() {
        use std::collections::HashMap;
        // serde_json cannot serialize a map with non-string keys → to_value
        // returns Err, which value_response must turn into an error Response.
        let mut bad: HashMap<Vec<i32>, i32> = HashMap::new();
        bad.insert(vec![1, 2], 3);
        let resp = value_response(Value::Number(7.into()), &bad);
        let text = serde_json::to_string(&resp).unwrap();
        assert!(text.contains("\"error\""), "expected error resp: {text}");
        // The id must be echoed back on the error response.
        assert!(text.contains('7'), "expected id echoed: {text}");
    }

    #[test]
    fn reject_remote_local_only_action_blocks_mesh_update_recover() {
        let args = serde_json::json!({ "action": "recover" });
        let err = reject_remote_local_only_action("system.mesh.update", &args).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("local-only"), "got: {msg}");
        assert!(msg.contains("recover"), "got: {msg}");
    }

    #[test]
    fn reject_remote_local_only_action_blocks_mesh_delete_leave() {
        let args = serde_json::json!({ "action": "leave" });
        let err = reject_remote_local_only_action("system.mesh.delete", &args).unwrap_err();
        assert!(err.to_string().contains("leave"), "got: {err}");
    }

    #[tokio::test]
    async fn two_tier_remote_demote_is_local_only() {
        for args in [
            serde_json::json!({ "action": "settings", "secure": false }),
            serde_json::json!({ "secure": false }),
        ] {
            let params = serde_json::json!({ "tool": "system.mesh.update", "args": args });
            let err = handle_exec(req_with_params(MESH_EXEC_METHOD, params), "peer-a")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("local-only"), "{args}: {err}");
        }
    }

    #[test]
    fn two_tier_remote_grant_delivery_stays_remote_dispatchable() {
        let args = serde_json::json!({ "action": "settings", "secure": true });
        assert!(reject_remote_local_only_action("system.mesh.update", &args).is_ok());
        let read = serde_json::json!({ "action": "settings" });
        assert!(reject_remote_local_only_action("system.mesh.update", &read).is_ok());
    }

    #[test]
    fn reject_remote_local_only_action_allows_other_actions() {
        // A remote-dispatchable action on the same tool is permitted.
        let args = serde_json::json!({ "action": "list" });
        assert!(reject_remote_local_only_action("system.mesh.update", &args).is_ok());
        // A wholly different tool with the "recover" action is not gated —
        // the guard keys on the (tool, action) pair, not the action alone.
        let recover = serde_json::json!({ "action": "recover" });
        assert!(reject_remote_local_only_action("other.tool", &recover).is_ok());
        // Missing action field must not panic and must be allowed.
        let empty = serde_json::json!({});
        assert!(reject_remote_local_only_action("system.mesh.delete", &empty).is_ok());
    }

    #[test]
    fn role_rank_orders_roles() {
        assert_eq!(role_rank("admin"), 3);
        assert_eq!(role_rank("member"), 2);
        assert_eq!(role_rank("user"), 2);
        assert_eq!(role_rank("any"), 0);
        assert_eq!(role_rank("nonsense"), 0);
    }

    #[test]
    fn role_satisfies_member_meets_member() {
        assert!(role_satisfies("member", "member"));
        assert!(role_satisfies("user", "member"));
        assert!(!role_satisfies("any", "member"));
    }

    fn req_no_params(method: &str) -> Request {
        Request::new(Value::Number(1.into()), method, None)
    }

    fn req_with_params(method: &str, params: Value) -> Request {
        Request::new(Value::Number(1.into()), method, Some(params))
    }

    #[test]
    fn push_ca_key_without_params_errors() {
        let err = handle_push_ca_key("peer-a", req_no_params(MESH_PUSH_CA_KEY_METHOD)).unwrap_err();
        assert!(err.to_string().contains("requires params"), "got: {err}");
    }

    #[test]
    fn peer_forget_without_params_errors() {
        let err = handle_peer_forget("peer-a", req_no_params(MESH_PEER_FORGET_METHOD)).unwrap_err();
        assert!(err.to_string().contains("requires params"), "got: {err}");
    }

    #[test]
    fn peer_forget_with_malformed_params_errors() {
        // ForgetParams requires a `peer_id` string.
        let err = handle_peer_forget(
            "peer-a",
            req_with_params(MESH_PEER_FORGET_METHOD, serde_json::json!({ "wrong": 1 })),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("parse mesh/peer-forget params"),
            "got: {err}"
        );
    }

    #[test]
    fn push_ca_state_without_params_errors() {
        let err =
            handle_push_ca_state("peer-a", req_no_params(MESH_PUSH_CA_STATE_METHOD)).unwrap_err();
        assert!(err.to_string().contains("requires params"), "got: {err}");
    }

    #[test]
    fn replicate_push_without_params_errors() {
        let err =
            handle_replicate_push("peer-a", req_no_params(MESH_REPLICATE_PUSH_METHOD)).unwrap_err();
        assert!(err.to_string().contains("requires params"), "got: {err}");
    }

    #[test]
    fn push_ca_key_with_malformed_params_errors() {
        // PushCaKeyParams needs both cert_pem and key_pem; omitting key_pem
        // fails deserialization in the Some(v) parse arm before any DB/PKI access.
        let err = handle_push_ca_key(
            "peer-a",
            req_with_params(
                MESH_PUSH_CA_KEY_METHOD,
                serde_json::json!({ "cert_pem": "x" }),
            ),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("parse mesh/push-ca-key params"),
            "got: {err}"
        );
    }

    #[test]
    fn push_ca_state_with_malformed_params_errors() {
        // PushCaStateParams requires current_cert_pem + current_key_pem.
        let err = handle_push_ca_state(
            "peer-a",
            req_with_params(MESH_PUSH_CA_STATE_METHOD, serde_json::json!({})),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("parse mesh/push-ca-state params"),
            "got: {err}"
        );
    }

    #[test]
    fn replicate_push_with_malformed_params_errors() {
        // A SignedEnvelope needs payload/signer_pubkey_b64/signature_b64; an
        // unrelated object fails to deserialize in the Some(v) parse arm.
        let err = handle_replicate_push(
            "peer-a",
            req_with_params(MESH_REPLICATE_PUSH_METHOD, serde_json::json!({ "foo": 1 })),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("parse mesh/replicate-push params"),
            "got: {err}"
        );
    }

    #[test]
    fn authorize_role_gated_with_bogus_token_fails_verification() {
        // A token that is present but structurally invalid fails at
        // caller_token::verify (base64/signature decode) before the pinned-key
        // lookup, so an empty in-memory DB is sufficient.
        let home = tempfile::tempdir().unwrap();
        crate::host_identity::init(home.path()).ok();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let env = utils::pki::SignedEnvelope {
            payload: "not-canonical-json".into(),
            signer_pubkey_b64: "!!!not-base64!!!".into(),
            signature_b64: "!!!not-base64!!!".into(),
            binding: None,
        };
        let err = authorize_role_gated(
            &conn,
            "peer-a",
            "schedule.owner.list",
            &serde_json::json!({}),
            "admin",
            Presented {
                caller_token: Some(&env),
                ca_proof: None,
            },
            0,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("caller token verification failed"),
            "got: {err}"
        );
    }

    #[test]
    fn effective_role_is_capped_by_the_token_scope() {
        // An admin's read-scoped token must not act as admin on a peer.
        assert_eq!(effective_role("admin", "member"), "member");
        assert!(!role_satisfies(effective_role("admin", "member"), "admin"));
        assert!(!role_satisfies(effective_role("admin", "read"), "member"));
        assert_eq!(effective_role("super-admin", "admin"), "admin");
        // A token cannot claim more than the replicated user holds.
        assert_eq!(effective_role("member", "admin"), "member");
        assert_eq!(effective_role("admin", "admin"), "admin");
    }

    #[test]
    fn role_satisfies_admin_meets_all_and_unknown_meets_any_only() {
        // admin satisfies member and user-tier requirements.
        assert!(role_satisfies("admin", "member"));
        assert!(role_satisfies("admin", "user"));
        // an unknown role ranks as "any" — meets only an "any" requirement.
        assert!(role_satisfies("ghost", "any"));
        assert!(!role_satisfies("ghost", "member"));
        assert!(!role_satisfies("ghost", "phantom"));
        assert!(!role_satisfies("admin", "phantom"));
    }

    #[test]
    fn remote_ok_gate_local_only_takes_priority_over_role() {
        // Even a role-gated tool is refused outright when it is local_only —
        // the reachability check runs before the auth-axis decision.
        let err = remote_ok_gate("mesh.internal", false, "admin").unwrap_err();
        assert!(err.to_string().contains("local_only"), "got: {err}");
    }

    #[tokio::test]
    async fn dev_sync_skipped_when_not_in_dev_mode() {
        // A production peer (not in Dev/Parked mode) short-circuits to a
        // "skipped" status with an explanatory detail and no commit count —
        // dev_sync is a no-op, not an error, on production-only hosts.
        // Mode is passed in rather than read from the host: this test used to
        // fail on any machine that was itself in dev mode.
        let r = handle_dev_sync_with(false).await.unwrap();
        assert_eq!(r.status, "skipped");
        assert_eq!(r.detail.as_deref(), Some("peer not in dev mode"));
        assert!(r.commits_pulled.is_none());
    }

    #[tokio::test]
    async fn exec_without_params_errors() {
        let err = handle_exec(req_no_params(MESH_EXEC_METHOD), "peer-a")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("mesh/exec requires params"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn exec_with_malformed_params_errors() {
        // MeshExecParams requires a `tool` string; an object without it fails
        // deserialization in the Some(v) parse arm before any DB/PKI access.
        let err = handle_exec(
            req_with_params(MESH_EXEC_METHOD, serde_json::json!({ "args": {} })),
            "peer-a",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("parse mesh/exec params"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn exec_rejects_remote_local_only_action() {
        // system.mesh.update/recover is a local-only action; handle_exec must reject it
        // at the per-action guard, which runs before any DB or dispatch access.
        let params = serde_json::json!({
            "tool": "system.mesh.update",
            "args": { "action": "recover" }
        });
        let err = handle_exec(req_with_params(MESH_EXEC_METHOD, params), "peer-a")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("local-only"), "got: {msg}");
        assert!(msg.contains("recover"), "got: {msg}");
    }

    const PLUGIN_ADMIN_TOOL: &str = "listener_test_plugin.adopt";
    const PLUGIN_ANY_TOOL: &str = "listener_test_plugin.status";
    const PLUGIN_LOCAL_TOOL: &str = "listener_test_plugin.engine_update";

    /// Callers the fake plugin saw, by username (`None` = no caller).
    static SEEN_CALLERS: std::sync::Mutex<Vec<Option<String>>> = std::sync::Mutex::new(Vec::new());
    static SEEN_TRANSPORT: std::sync::Mutex<Vec<(bool, bool)>> = std::sync::Mutex::new(Vec::new());
    static SEEN_PROOFS: std::sync::Mutex<Vec<bool>> = std::sync::Mutex::new(Vec::new());

    /// Load a fake plugin owning two tools whose manifest declared `admin` and
    /// `any`, recording the caller each invoke carries.
    fn install_fake_plugin() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            dispatch::set_dynamic_dispatch(
                Box::new(|name, _args, ctx: &contract::ToolCtx| {
                    name.starts_with("listener_test_plugin.").then(|| {
                        SEEN_CALLERS
                            .lock()
                            .unwrap()
                            .push(ctx.verified_caller().map(|c| c.username.clone()));
                        SEEN_TRANSPORT
                            .lock()
                            .unwrap()
                            .push((ctx.local_transport(), ctx.second_factor()));
                        SEEN_PROOFS.lock().unwrap().push(
                            ctx.service::<std::sync::Arc<dyn contract::RemoteExec>>()
                                .is_ok_and(|r| r.attaches_ca_proof()),
                        );
                        Ok(serde_json::json!({ "ran": name }))
                    })
                }),
                Box::new(Vec::new),
            );
        });
        dispatch::tool_roles::install_plugin_tools([
            (
                PLUGIN_ADMIN_TOOL.to_string(),
                dispatch::tool_roles::PluginToolPolicy::from_manifest(
                    PLUGIN_ADMIN_TOOL,
                    Some("admin"),
                    None,
                    None,
                ),
            ),
            (
                PLUGIN_ANY_TOOL.to_string(),
                dispatch::tool_roles::PluginToolPolicy::from_manifest(
                    PLUGIN_ANY_TOOL,
                    Some("any"),
                    None,
                    None,
                ),
            ),
            (
                PLUGIN_LOCAL_TOOL.to_string(),
                dispatch::tool_roles::PluginToolPolicy::from_manifest(
                    PLUGIN_LOCAL_TOOL,
                    Some("any"),
                    Some(false),
                    Some(false),
                )
                .local_only(Some(true)),
            ),
        ]);
    }

    #[tokio::test]
    async fn exec_refuses_secret_writes() {
        use auth as _;
        dispatch::remote_ok::install(dispatch::local_only_names());
        for tool in [
            "secrets.put",
            "secrets.restore",
            "secrets.upsert",
            "secrets.delete",
        ] {
            let params = serde_json::json!({ "tool": tool, "args": { "name": "s", "value": "v" } });
            let err = handle_exec(req_with_params(MESH_EXEC_METHOD, params), "peer-a")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("local_only"), "{tool}: {err}");
        }
    }

    #[tokio::test]
    async fn exec_refuses_a_local_only_plugin_tool() {
        install_fake_plugin();
        let params = serde_json::json!({ "tool": PLUGIN_LOCAL_TOOL, "args": {} });
        let err = handle_exec(req_with_params(MESH_EXEC_METHOD, params), "peer-a")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("local_only"), "got: {err}");
    }

    #[tokio::test]
    async fn exec_refuses_a_local_only_plugin_tool_through_plugin_create_invoke() {
        install_fake_plugin();
        let params = serde_json::json!({
            "tool": "plugin.create",
            "args": { "action": "invoke", "tool": PLUGIN_LOCAL_TOOL, "args": {} },
        });
        let err = handle_exec(req_with_params(MESH_EXEC_METHOD, params), "peer-a")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("local_only"), "got: {err}");
    }

    #[test]
    fn plugin_create_invoke_of_a_remote_ok_verb_passes_the_local_only_guard() {
        install_fake_plugin();
        let args = serde_json::json!({ "action": "invoke", "tool": PLUGIN_ANY_TOOL });
        assert!(reject_remote_local_only_action("plugin.create", &args).is_ok());
    }

    #[test]
    fn exec_refuses_an_admin_plugin_tool_without_a_caller_token() {
        // An ephemeral DB, so the role check never opens the shared default db
        // other tests are migrating.
        with_db(async {
            install_fake_plugin();
            let params = serde_json::json!({ "tool": PLUGIN_ADMIN_TOOL, "args": {} });
            let err = handle_exec(req_with_params(MESH_EXEC_METHOD, params), "peer-a")
                .await
                .unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("no signed caller"), "got: {msg}");
            assert!(msg.contains("'admin'"), "got: {msg}");
        });
    }

    #[tokio::test]
    async fn exec_without_a_token_reaches_the_plugin_with_no_caller() {
        let _g = crate::mesh::dispatcher::test_guard().await;
        install_fake_plugin();
        crate::mesh::dispatcher::reset_for_tests();
        // The shared ctx carries the host operator; a peer must never inherit it.
        let mut host = (*crate::mesh::dispatcher::tests::make_ctx())
            .clone()
            .with_auth(contract::CallerIdentity {
                user_id: "u_host".into(),
                username: "host_admin".into(),
                role: "admin".into(),
                can_mutate: true,
            });
        host.set_local_transport(true);
        host.set_second_factor(true);
        crate::mesh::dispatcher::install(std::sync::Arc::new(host));
        SEEN_CALLERS.lock().unwrap().clear();
        SEEN_TRANSPORT.lock().unwrap().clear();

        let params = serde_json::json!({ "tool": PLUGIN_ANY_TOOL, "args": {} });
        let out = handle_exec(req_with_params(MESH_EXEC_METHOD, params), "peer-a")
            .await
            .expect("an any-role plugin tool needs no token");
        assert_eq!(out.result["ran"], PLUGIN_ANY_TOOL);
        assert_eq!(*SEEN_CALLERS.lock().unwrap(), vec![None]);
        assert_eq!(*SEEN_TRANSPORT.lock().unwrap(), vec![(false, false)]);
        crate::mesh::dispatcher::reset_for_tests();
    }

    #[test]
    fn refresh_cert_cn_mismatch_or_no_ca_key_errors() {
        // Without the mesh CA key present, handle_refresh_cert fails fast on the
        // CA-key ensure. If a key does happen to exist in this host's pki_dir,
        // the params still fail to parse (no params) — either way it errors and
        // never signs a cert. This exercises the early guard path.
        let err =
            handle_refresh_cert("peer-a", req_no_params(MESH_REFRESH_CERT_METHOD)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("mesh CA key") || msg.contains("requires params"),
            "got: {msg}"
        );
    }

    #[test]
    fn two_tier_ca_key_written_after_a_demotion_is_deleted() {
        let pki = tempfile::tempdir().unwrap();
        utils::pki::init_mesh_ca(pki.path(), "founder").unwrap();
        drop_ca_keys_if_demoted(pki.path(), true, MESH_PUSH_CA_KEY_METHOD).unwrap();
        assert!(utils::pki::has_mesh_ca_key(pki.path()));
        let err = drop_ca_keys_if_demoted(pki.path(), false, MESH_PUSH_CA_KEY_METHOD).unwrap_err();
        assert!(err.to_string().contains("demoted"), "got: {err}");
        assert!(!utils::pki::has_mesh_ca_key(pki.path()));
    }

    const ROLE_PEER: &str = "019e7105-0000-7000-8000-0000000abc0f";

    fn with_role_gate_fixture(
        user_role: &'static str,
        body: impl FnOnce(
            &rusqlite::Connection,
            &dyn Fn(&str) -> utils::pki::SignedEnvelope,
            &dyn Fn(&utils::pki::SignedEnvelope) -> String,
        ) + Send
        + 'static,
    ) {
        let _env = crate::test_env::snapshot();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::mesh::pin_home(home.path());
        with_db(async move {
            let conn = db::open_default().unwrap();
            utils::pki::init_mesh_ca(&pki_dir(), "founder").unwrap();
            let signer = tempfile::tempdir().unwrap();
            let key = utils::pki::load_or_init_bootstrap_key(signer.path()).unwrap();
            let fp = utils::pki::bootstrap_pubkey_fingerprint(&key.verifying_key());
            pdb::upsert_peer(&conn, ROLE_PEER, "hf", "127.0.0.1", 1, Some(&fp), "").unwrap();
            identities::users::insert(&conn, "u-admin", "root", "h", user_role, "t0").unwrap();
            let claimed = contract::CallerIdentity {
                user_id: "u-admin".into(),
                username: "root".into(),
                role: "admin".into(),
                can_mutate: true,
            };
            let mint = |tool: &str| {
                crate::mesh::caller_token::mint(
                    signer.path(),
                    &claimed,
                    tool,
                    &serde_json::json!({}),
                    ROLE_PEER,
                    crate::host_identity::machine_id(),
                    utils::time::now().unix_seconds(),
                    60,
                )
                .unwrap()
            };
            let proof_for = |env: &utils::pki::SignedEnvelope| {
                let nonce = env.binding.as_ref().unwrap().nonce.clone();
                utils::pki::sign_with_mesh_ca(
                    &pki_dir(),
                    &crate::mesh::exec::ca_holder_message(ROLE_PEER, &nonce),
                )
                .unwrap()
            };
            body(&conn, &mint, &proof_for);
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_refused_caller_token_does_not_spend_its_nonce() {
        with_role_gate_fixture("admin", |conn, mint, _| {
            let env = mint("schedule.owner.list");
            conn.execute("DELETE FROM users WHERE id = 'u-admin'", [])
                .unwrap();
            let err =
                authorize_as_peer(conn, "schedule.owner.list", "admin", &env, None).unwrap_err();
            assert!(err.to_string().contains("replicated users"), "{err}");
            identities::users::insert(conn, "u-admin", "root", "h", "admin", "t0").unwrap();
            let err = authorize_as_peer(conn, "schedule.owner.list", "super-admin", &env, None)
                .unwrap_err();
            assert!(err.to_string().contains("requires role"), "{err}");
            authorize_as_peer(conn, "schedule.owner.list", "admin", &env, None).unwrap();
            let err =
                authorize_as_peer(conn, "schedule.owner.list", "admin", &env, None).unwrap_err();
            assert!(format!("{err:#}").contains("replay"), "{err:#}");
        });
    }

    fn authorize_as_peer(
        conn: &rusqlite::Connection,
        tool: &str,
        required: &str,
        env: &utils::pki::SignedEnvelope,
        proof: Option<&str>,
    ) -> Result<contract::CallerIdentity> {
        authorize_role_gated(
            conn,
            ROLE_PEER,
            tool,
            &serde_json::json!({}),
            required,
            Presented {
                caller_token: Some(env),
                ca_proof: proof,
            },
            utils::time::now().unix_seconds(),
        )
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_remote_admin_safe_tool_runs_as_admin_without_a_ca_proof() {
        with_role_gate_fixture("admin", |conn, mint, _| {
            let env = mint("schedule.owner.list");
            assert_eq!(
                authorize_as_peer(conn, "schedule.owner.list", "admin", &env, None)
                    .unwrap()
                    .role,
                "admin"
            );
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn an_admin_token_for_a_member_row_is_refused_a_remote_admin_safe_tool() {
        with_role_gate_fixture("member", |conn, mint, _| {
            let env = mint("schedule.owner.list");
            let err =
                authorize_as_peer(conn, "schedule.owner.list", "admin", &env, None).unwrap_err();
            assert!(err.to_string().contains("role 'member'"), "got: {err}");
        });
    }

    fn assert_custody_refused_without_a_ca_proof(tool: &'static str, required: &'static str) {
        with_role_gate_fixture("admin", move |conn, mint, proof_for| {
            let refused = |proof: Option<&str>, why: &str| {
                let env = mint(tool);
                let err = authorize_as_peer(conn, tool, required, &env, proof).unwrap_err();
                assert!(err.to_string().contains("CA proof"), "{tool} {why}: {err}");
            };
            refused(None, "no proof");
            refused(Some("bogus"), "bad proof");
            let other = utils::pki::sign_with_mesh_ca(
                &pki_dir(),
                &crate::mesh::exec::ca_holder_message("someone-else", "n"),
            )
            .unwrap();
            refused(Some(&other), "foreign proof");
            let env = mint(tool);
            let proof = proof_for(&env);
            assert_eq!(
                authorize_as_peer(conn, tool, required, &env, Some(&proof))
                    .unwrap()
                    .role,
                "admin",
                "{tool} with a CA proof"
            );
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_remote_system_update_is_refused_without_a_ca_proof() {
        assert_custody_refused_without_a_ca_proof("system.update", "admin");
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_remote_plugin_update_is_refused_without_a_ca_proof() {
        assert_custody_refused_without_a_ca_proof("plugin.update", "admin");
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_cross_machine_secret_read_is_refused_without_a_ca_proof() {
        assert_custody_refused_without_a_ca_proof("secrets.detail", "admin");
    }

    #[serial_test::serial(env)]
    #[test]
    fn schedule_create_run_is_refused_without_a_ca_proof() {
        assert_custody_refused_without_a_ca_proof("schedule.create", "admin");
    }

    #[serial_test::serial(env)]
    #[test]
    fn real_deploy_and_container_tools_are_refused_without_a_ca_proof() {
        for tool in [
            "service.create",
            "lxc.push",
            "container.create",
            "config.upsert",
        ] {
            assert_custody_refused_without_a_ca_proof(tool, "admin");
        }
    }

    #[serial_test::serial(env)]
    #[test]
    fn suffix_variants_of_install_verbs_are_refused_without_a_ca_proof() {
        for tool in [
            "docker.engine_update_v2",
            "ntfy.upgrade_now",
            "x.reinstall",
            "plugin.update.batch",
        ] {
            assert_custody_refused_without_a_ca_proof(tool, "admin");
        }
    }

    #[serial_test::serial(env)]
    #[test]
    fn an_unmarked_plugin_tool_with_a_user_role_is_refused_without_a_ca_proof() {
        const TOOL: &str = "listener_test_plugin.user_tool";
        dispatch::tool_roles::install_plugin_tools([(
            TOOL.to_string(),
            dispatch::tool_roles::PluginToolPolicy {
                role: "user",
                ..dispatch::tool_roles::PluginToolPolicy::from_manifest(TOOL, None, None, None)
            },
        )]);
        assert_custody_refused_without_a_ca_proof(TOOL, "user");
        dispatch::tool_roles::remove_plugin_tools([TOOL]);
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_plugin_marked_remote_admin_runs_without_a_ca_proof() {
        const TOOL: &str = "listener_test_plugin.safe_tool";
        dispatch::tool_roles::install_plugin_tools([(
            TOOL.to_string(),
            dispatch::tool_roles::PluginToolPolicy::from_manifest(TOOL, Some("admin"), None, None)
                .remote_admin(Some(true)),
        )]);
        with_role_gate_fixture("admin", |conn, mint, _| {
            let env = mint(TOOL);
            assert_eq!(
                authorize_as_peer(conn, TOOL, "admin", &env, None)
                    .unwrap()
                    .role,
                "admin"
            );
        });
        dispatch::tool_roles::remove_plugin_tools([TOOL]);
    }

    #[serial_test::serial(env)]
    #[test]
    fn plugin_create_invoke_end_to_end_is_refused_without_a_ca_proof() {
        let _env = crate::test_env::snapshot();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::mesh::pin_home(home.path());
        with_db(async {
            install_fake_plugin();
            dispatch::remote_ok::install(dispatch::local_only_names());
            let conn = db::open_default().unwrap();
            utils::pki::init_mesh_ca(&pki_dir(), "founder").unwrap();
            let signer = tempfile::tempdir().unwrap();
            let key = utils::pki::load_or_init_bootstrap_key(signer.path()).unwrap();
            let fp = utils::pki::bootstrap_pubkey_fingerprint(&key.verifying_key());
            pdb::upsert_peer(&conn, ROLE_PEER, "hf", "127.0.0.1", 1, Some(&fp), "").unwrap();
            identities::users::insert(&conn, "u-admin", "root", "h", "admin", "t0").unwrap();
            drop(conn);
            let caller = contract::CallerIdentity {
                user_id: "u-admin".into(),
                username: "root".into(),
                role: "admin".into(),
                can_mutate: true,
            };
            let args =
                serde_json::json!({ "action": "invoke", "tool": PLUGIN_ANY_TOOL, "args": {} });
            let token = crate::mesh::caller_token::mint(
                signer.path(),
                &caller,
                "plugin.create",
                &args,
                ROLE_PEER,
                crate::host_identity::machine_id(),
                utils::time::now().unix_seconds(),
                60,
            )
            .unwrap();
            let params = serde_json::json!({
                "tool": "plugin.create",
                "args": args,
                "caller_token": token,
            });
            let err = handle_exec(req_with_params(MESH_EXEC_METHOD, params), ROLE_PEER)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("CA proof"), "got: {err}");
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn secrets_detail_is_refused_on_an_insecure_member_even_with_a_ca_proof() {
        let _env = crate::test_env::snapshot();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::mesh::pin_home(home.path());
        with_db(async {
            let conn = db::open_default().unwrap();
            pdb::set_secure(&conn, false).unwrap();
            drop(conn);
            let params = serde_json::json!({ "tool": "secrets.detail", "args": { "name": "s" } });
            let err = handle_exec(req_with_params(MESH_EXEC_METHOD, params), ROLE_PEER)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("insecure member"), "got: {err}");
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_mesh_relayed_call_never_carries_this_hosts_ca_proof() {
        let _env = crate::test_env::snapshot();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::mesh::pin_home(home.path());
        utils::pki::init_mesh_ca(&pki_dir(), "founder").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _g = crate::mesh::dispatcher::test_guard().await;
            install_fake_plugin();
            crate::mesh::dispatcher::reset_for_tests();
            let mut host = (*crate::mesh::dispatcher::tests::make_ctx()).clone();
            host.register_service::<std::sync::Arc<dyn contract::RemoteExec>>(std::sync::Arc::new(
                crate::mesh::MeshRemoteExec,
            ));
            assert!(contract::RemoteExec::attaches_ca_proof(
                &crate::mesh::MeshRemoteExec
            ));
            assert!(crate::mesh::exec::outbound_ca_proof(&pki_dir(), "n").is_some());
            crate::mesh::dispatcher::install(std::sync::Arc::new(host));
            SEEN_PROOFS.lock().unwrap().clear();
            let params = serde_json::json!({ "tool": PLUGIN_ANY_TOOL, "args": {} });
            handle_exec(req_with_params(MESH_EXEC_METHOD, params), "peer-a")
                .await
                .unwrap();
            assert_eq!(*SEEN_PROOFS.lock().unwrap(), vec![false]);
            crate::mesh::dispatcher::reset_for_tests();
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn exec_as_params_prove_ca_possession_from_the_key_not_the_secure_flag() {
        let _env = crate::test_env::snapshot();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::mesh::pin_home(home.path());
        with_db(async {
            let conn = db::open_default().unwrap();
            let pki = pki_dir();
            utils::pki::init_mesh_ca(&pki, "founder").unwrap();
            let key = utils::pki::load_or_init_bootstrap_key(&pki).unwrap();
            let fp = utils::pki::bootstrap_pubkey_fingerprint(&key.verifying_key());
            let me = crate::host_identity::machine_id();
            pdb::upsert_peer(&conn, me, "self", "127.0.0.1", 1, Some(&fp), "").unwrap();
            identities::users::insert(&conn, "u-admin", "root", "h", "admin", "t0").unwrap();
            pdb::set_secure(&conn, false).unwrap();
            let caller = contract::CallerIdentity {
                user_id: "u-admin".into(),
                username: "root".into(),
                role: "admin".into(),
                can_mutate: true,
            };
            const CUSTODY: &str = "system.update";
            let args = serde_json::json!({});
            let params = || {
                let token = crate::mesh::caller_token::mint(
                    &pki,
                    &caller,
                    CUSTODY,
                    &args,
                    me,
                    me,
                    utils::time::now().unix_seconds(),
                    crate::mesh::caller_token::DEFAULT_TTL_SECS,
                )
                .unwrap();
                let nonce = token.binding.as_ref().unwrap().nonce.clone();
                let proof = crate::mesh::exec::ca_holder_proof(&pki, me, &nonce);
                (token, proof)
            };
            let authorize = |token: &utils::pki::SignedEnvelope, proof: Option<&str>| {
                authorize_role_gated(
                    &conn,
                    me,
                    CUSTODY,
                    &args,
                    "admin",
                    Presented {
                        caller_token: Some(token),
                        ca_proof: proof,
                    },
                    utils::time::now().unix_seconds(),
                )
            };

            let (token, proof) = params();
            assert!(proof.is_some());
            assert_eq!(authorize(&token, proof.as_deref()).unwrap().role, "admin");

            utils::pki::remove_mesh_ca_keys(&pki).unwrap();
            let (token, proof) = params();
            assert!(proof.is_none());
            let err = authorize(&token, proof.as_deref()).unwrap_err();
            assert!(err.to_string().contains("CA proof"), "no key: {err}");
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn two_tier_has_ca_key_without_a_valid_proof_does_not_prove_a_holder() {
        let _env = crate::test_env::snapshot();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::mesh::pin_home(home.path());
        let pki = pki_dir();
        utils::pki::init_mesh_ca(&pki, "founder").unwrap();
        const PEER: &str = "019e7105-0000-7000-8000-0000000abc11";
        let nonce = "n-1";
        let ok = |reply: serde_json::Value| {
            let reply: crate::mesh::exec::CaHolderReply = serde_json::from_value(reply).unwrap();
            crate::mesh::exec::reply_proves_ca_possession(&pki, PEER, nonce, &reply)
        };
        assert!(!ok(serde_json::json!({ "has_key": true })));
        assert!(!ok(
            serde_json::json!({ "has_key": true, "proof": "bogus" })
        ));
        let other_peer = utils::pki::sign_with_mesh_ca(
            &pki,
            &crate::mesh::exec::ca_holder_message("someone-else", nonce),
        )
        .unwrap();
        assert!(!ok(
            serde_json::json!({ "has_key": true, "proof": other_peer })
        ));
        let stale =
            utils::pki::sign_with_mesh_ca(&pki, &crate::mesh::exec::ca_holder_message(PEER, "old"))
                .unwrap();
        assert!(!ok(serde_json::json!({ "has_key": true, "proof": stale })));
        let good =
            utils::pki::sign_with_mesh_ca(&pki, &crate::mesh::exec::ca_holder_message(PEER, nonce))
                .unwrap();
        assert!(ok(serde_json::json!({ "has_key": true, "proof": good })));
    }

    #[test]
    fn authorize_role_gated_without_token_refuses() {
        // The no-token branch returns before touching the connection, so an
        // empty in-memory DB is sufficient to exercise it.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let err = authorize_role_gated(
            &conn,
            "peer-a",
            "schedule.owner.list",
            &serde_json::json!({}),
            "admin",
            Presented::default(),
            0,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no signed caller"), "got: {msg}");
        assert!(msg.contains("schedule.owner.list"), "got: {msg}");
    }

    // Drive `body` on a current-thread runtime with an ephemeral DB pointed at a
    // temp file. `db::with_db_path` installs a task-local override that
    // `with_pooled_or_open` / `open_default` honor on the single-threaded
    // executor, so both the sync handlers under test and our seed/assert queries
    // share one throwaway database.
    fn with_db<T>(body: impl std::future::Future<Output = T>) -> T {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("listener-test.db");
        crate::host_identity::init(dir.path()).ok();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(db::with_db_path(db_path, body))
    }

    #[test]
    fn peer_leaving_marks_departed() {
        with_db(async {
            let cn = utils::id::new();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(&conn, &cn, "host-l", "127.0.0.1", 1, Some("fp-l"), "").unwrap();
            assert!(!pdb::is_peer_departed(&conn, &cn).unwrap());
            drop(conn);

            handle_peer_leaving(&cn).unwrap();

            let conn = db::open_default().unwrap();
            assert!(pdb::is_peer_departed(&conn, &cn).unwrap());
        });
    }

    #[test]
    fn peer_forget_removes_rows_for_peer() {
        with_db(async {
            let cn = utils::id::new();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(&conn, &cn, "host-g", "127.0.0.1", 1, Some("fp-g"), "").unwrap();
            drop(conn);

            let removed = handle_peer_forget(
                &cn,
                req_with_params(
                    MESH_PEER_FORGET_METHOD,
                    serde_json::json!({ "peer_id": cn }),
                ),
            )
            .unwrap();
            assert!(removed > 0, "expected at least one row removed");

            let conn = db::open_default().unwrap();
            let peers = pdb::list_peers(&conn).unwrap();
            assert!(!peers.iter().any(|p| p.peer_id == cn));
        });
    }

    #[test]
    fn peer_forget_unknown_peer_removes_nothing() {
        with_db(async {
            let removed = handle_peer_forget(
                "never-existed",
                req_with_params(
                    MESH_PEER_FORGET_METHOD,
                    serde_json::json!({ "peer_id": "never-existed" }),
                ),
            )
            .unwrap();
            assert_eq!(removed, 0);
        });
    }

    #[test]
    fn replicate_push_refused_without_pinned_fp() {
        with_db(async {
            // A structurally-valid envelope parses, but with no pinned bootstrap
            // fp for the peer the merge is refused before touching the engine.
            let env = utils::pki::SignedEnvelope {
                payload: "{}".into(),
                signer_pubkey_b64: "AA==".into(),
                signature_b64: "AA==".into(),
                binding: None,
            };
            let err = handle_replicate_push(
                "peer-nopin",
                req_with_params(
                    MESH_REPLICATE_PUSH_METHOD,
                    serde_json::to_value(&env).unwrap(),
                ),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("not a verified member"),
                "got: {err}"
            );
        });
    }

    #[test]
    fn replicate_roots_on_empty_db_returns_roots_map() {
        with_db(async {
            // A fresh DB still yields a roots map (one entry per replicated
            // entity kind); the call must succeed and return that structure.
            let r = handle_replicate_roots().unwrap();
            let v = serde_json::to_value(&r).unwrap();
            assert!(v.get("roots").is_some(), "missing roots: {v}");
        });
    }

    #[test]
    fn build_addressing_snapshot_none_on_empty_db() {
        with_db(async {
            assert!(build_addressing_snapshot().is_none());
        });
    }

    #[test]
    fn build_addressing_snapshot_uses_display_name_and_channels() {
        with_db(async {
            let conn = db::open_default().unwrap();
            db::host_addressing::upsert_host_addressing(&conn, "display_name", "willow", "test")
                .unwrap();
            db::host_addressing::upsert_host_addressing(&conn, "lan_v4", "10.0.0.9", "test")
                .unwrap();
            drop(conn);

            let snap = build_addressing_snapshot().expect("snapshot present");
            assert_eq!(snap.display_name, "willow");
            // The display_name row is folded into display_name, not a channel;
            // only the lan_v4 row becomes a channel.
            assert_eq!(snap.channels.len(), 1);
            assert_eq!(snap.channels[0].kind, "lan_v4");
            assert_eq!(snap.channels[0].value, "10.0.0.9");
        });
    }

    #[test]
    fn build_addressing_snapshot_falls_back_when_no_display_name_row() {
        let iddir = tempfile::tempdir().unwrap();
        crate::host_identity::init(iddir.path()).unwrap();
        with_db(async {
            let conn = db::open_default().unwrap();
            // Only a channel row, no display_name → display_name falls back to
            // the host identity's display hostname (non-empty).
            db::host_addressing::upsert_host_addressing(&conn, "lan_v4", "10.0.0.10", "test")
                .unwrap();
            drop(conn);

            let snap = build_addressing_snapshot().expect("snapshot present");
            assert!(!snap.display_name.is_empty());
            assert_eq!(snap.channels.len(), 1);
        });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_unknown_method_returns_method_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("dispatch-test.db");
        db::with_db_path(db_path, async {
            let resp = dispatch(req_no_params("mesh/bogus-method"), "peer-a").await;
            let text = serde_json::to_string(&resp).unwrap();
            assert!(text.contains("\"error\""), "expected error: {text}");
            assert!(text.contains("not supported"), "got: {text}");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_rejects_departed_peer() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("dispatch-departed.db");
        db::with_db_path(db_path, async {
            let cn = utils::id::new();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(&conn, &cn, "hg", "127.0.0.1", 1, Some("fp-x"), "").unwrap();
            pdb::mark_peer_departed(&conn, &cn).unwrap();
            drop(conn);

            // A departed peer hitting any method other than mesh/peer-leaving is
            // rejected at the gate with a re-pair message.
            let resp = dispatch(req_no_params(MESH_HAS_CA_KEY_METHOD), &cn).await;
            let text = serde_json::to_string(&resp).unwrap();
            assert!(text.contains("has departed this mesh"), "got: {text}");
        })
        .await;
    }

    #[test]
    fn ca_holder_nonce_must_be_a_uuid_within_64_bytes() {
        assert!(valid_ca_holder_nonce(&utils::id::new()));
        assert!(!valid_ca_holder_nonce("not-a-uuid"));
        assert!(!valid_ca_holder_nonce(&"a".repeat(65)));
        assert!(!valid_ca_holder_nonce(&format!(
            "{}{}",
            utils::id::new(),
            " ".repeat(40)
        )));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_has_ca_key_rejects_malformed_nonce() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("dispatch-hasca-nonce.db");
        db::with_db_path(db_path, async {
            let resp = dispatch(
                req_with_params(
                    MESH_HAS_CA_KEY_METHOD,
                    serde_json::json!({ "nonce": "x".repeat(4096) }),
                ),
                "peer-ok",
            )
            .await;
            let text = serde_json::to_string(&resp).unwrap();
            assert!(text.contains("\"error\""), "got: {text}");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_has_ca_key_for_active_peer() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("dispatch-hasca.db");
        db::with_db_path(db_path, async {
            // Non-departed peer, no CA key on this host → has_key: false, and the
            // departed gate lets the call through to the handler.
            let resp = dispatch(req_no_params(MESH_HAS_CA_KEY_METHOD), "peer-ok").await;
            let text = serde_json::to_string(&resp).unwrap();
            assert!(text.contains("has_key"), "got: {text}");
            assert!(!text.contains("\"error\""), "unexpected error: {text}");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_peer_removed_is_ok_and_does_not_depart_caller() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("dispatch-removed.db");
        db::with_db_path(db_path, async {
            let cn = utils::id::new();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(&conn, &cn, "hk", "127.0.0.1", 1, Some("fp-k"), "").unwrap();
            drop(conn);

            let resp = dispatch(req_no_params(MESH_PEER_REMOVED_METHOD), &cn).await;
            let text = serde_json::to_string(&resp).unwrap();
            assert!(!text.contains("\"error\""), "unexpected error: {text}");
            // mesh/peer-removed must NOT mark the caller departed (that's the
            // 2026-05-28 regression this method guards against).
            let conn = db::open_default().unwrap();
            assert!(!pdb::is_peer_departed(&conn, &cn).unwrap());
        })
        .await;
    }

    #[serial_test::serial(env)]
    #[test]
    fn refresh_cert_cn_mismatch_rejected_with_ca_key_present() {
        let _env = crate::test_env::snapshot();
        // With the mesh CA key present but the joiner_hostname param not matching
        // the authenticated CN, the handler must refuse on the CN check rather
        // than sign anything.
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::mesh::pin_home(dir.path());
        let pki = pki_dir();
        utils::pki::init_mesh_ca(&pki, "24647a14a251e863cdf8dcee692f2915").unwrap();
        let params = serde_json::json!({
            "joiner_hostname": "someone-else",
            "csr_client_pem": "x",
            "csr_server_pem": "y",
        });
        let err = handle_refresh_cert("peer-a", req_with_params(MESH_REFRESH_CERT_METHOD, params))
            .unwrap_err();
        assert!(
            err.to_string().contains("does not match joiner_hostname"),
            "got: {err}"
        );
    }

    fn refresh_params(cn: &str) -> Value {
        let (csr_client_pem, _, csr_server_pem, _) = utils::pki::build_refresh_csrs(cn).unwrap();
        serde_json::json!({
            "joiner_hostname": cn,
            "csr_client_pem": csr_client_pem,
            "csr_server_pem": csr_server_pem,
        })
    }

    #[serial_test::serial(env)]
    #[test]
    fn refresh_cert_signs_for_matching_cn_with_ca_key() {
        let _env = crate::test_env::snapshot();
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::mesh::pin_home(dir.path());
        db::with_thread_db_path(&dir.path().join("refresh-ok.db"), || {
            utils::pki::init_mesh_ca(&pki_dir(), "24647a14a251e863cdf8dcee692f2915").unwrap();
            let cn = utils::id::new();
            crate::host_identity::init(&pki_dir()).ok();
            let key = member_key(&cn);
            let params = bound_refresh(
                &cn,
                &key,
                crate::host_identity::machine_id(),
                utils::time::now().unix_seconds(),
            );
            let res = handle_refresh_cert(&cn, req_with_params(MESH_REFRESH_CERT_METHOD, params))
                .unwrap();
            assert_eq!(
                utils::pki::cert_summary(&res.client_cert_pem).unwrap().cn,
                cn
            );
            assert_eq!(
                utils::pki::cert_summary(&res.server_cert_pem).unwrap().cn,
                "orca-mesh-server"
            );
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn refresh_cert_refuses_departed_or_unknown_cn() {
        let _env = crate::test_env::snapshot();
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::mesh::pin_home(dir.path());
        db::with_thread_db_path(&dir.path().join("refresh-refused.db"), || {
            utils::pki::init_mesh_ca(&pki_dir(), "24647a14a251e863cdf8dcee692f2915").unwrap();
            let departed = utils::id::new();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(&conn, &departed, "hd", "127.0.0.1", 1, Some("fp-d"), "").unwrap();
            pdb::mark_peer_departed(&conn, &departed).unwrap();
            drop(conn);

            for cn in [departed, utils::id::new()] {
                let err = handle_refresh_cert(
                    &cn,
                    req_with_params(MESH_REFRESH_CERT_METHOD, refresh_params(&cn)),
                )
                .unwrap_err();
                assert!(
                    err.to_string().contains("not a known active mesh peer"),
                    "got: {err}"
                );
            }
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn forgotten_peer_cannot_call_anything() {
        let _env = crate::test_env::snapshot();
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::mesh::pin_home(dir.path());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(db::with_db_path(dir.path().join("forgotten.db"), async {
            utils::pki::init_mesh_ca(&pki_dir(), "24647a14a251e863cdf8dcee692f2915").unwrap();
            let cn = utils::id::new();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(&conn, &cn, "hf", "127.0.0.1", 1, Some("fp-f"), "").unwrap();
            pdb::forget_peer(&conn, &cn).unwrap();
            drop(conn);

            let ping = dispatch(req_no_params(MESH_PING_METHOD), &cn).await;
            let ping = serde_json::to_string(&ping).unwrap();
            assert!(ping.contains("not a member"), "ping: {ping}");

            let refresh = dispatch(
                req_with_params(MESH_REFRESH_CERT_METHOD, refresh_params(&cn)),
                &cn,
            )
            .await;
            let refresh = serde_json::to_string(&refresh).unwrap();
            assert!(refresh.contains("not a member"), "refresh: {refresh}");
            assert!(!refresh.contains("client_cert_pem"), "refresh: {refresh}");
        }));
    }

    #[serial_test::serial(env)]
    #[test]
    fn refresh_cert_refuses_tombstoned_cn_even_with_a_peer_row() {
        let _env = crate::test_env::snapshot();
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::mesh::pin_home(dir.path());
        db::with_thread_db_path(&dir.path().join("refresh-tombstoned.db"), || {
            utils::pki::init_mesh_ca(&pki_dir(), "24647a14a251e863cdf8dcee692f2915").unwrap();
            let cn = utils::id::new();
            let conn = db::open_default().unwrap();
            // A row resurrected by any path other than join-confirm keeps the tombstone.
            pdb::upsert_peer(&conn, &cn, "hz", "127.0.0.1", 1, Some("fp-z"), "").unwrap();
            pdb::write_forget_tombstone(&conn, &cn).unwrap();
            drop(conn);

            let err = handle_refresh_cert(
                &cn,
                req_with_params(MESH_REFRESH_CERT_METHOD, refresh_params(&cn)),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("not a known active mesh peer"),
                "got: {err}"
            );
        });
    }

    #[test]
    fn peer_forget_refuses_another_peers_id_and_keeps_its_rows() {
        with_db(async {
            let victim = utils::id::new();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(&conn, &victim, "host-v", "127.0.0.1", 1, Some("fp-v"), "").unwrap();
            drop(conn);

            let err = handle_peer_forget(
                &utils::id::new(),
                req_with_params(
                    MESH_PEER_FORGET_METHOD,
                    serde_json::json!({ "peer_id": victim }),
                ),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("may only retire itself"),
                "got: {err}"
            );

            let conn = db::open_default().unwrap();
            assert!(
                pdb::list_peers(&conn)
                    .unwrap()
                    .iter()
                    .any(|p| p.peer_id == victim)
            );
        });
    }

    #[test]
    fn peer_forget_never_forgets_the_local_host() {
        let own = "0190a000-0000-7000-8000-000000000001";
        let err = check_peer_forget(own, own, own).unwrap_err();
        assert!(err.to_string().contains("forget itself"), "got: {err}");
        assert!(check_peer_forget("peer-a", "peer-a", own).is_ok());
    }

    struct CaFixture {
        old_cert: String,
        old_key: String,
        new_cert: String,
        new_key: String,
        _dir: tempfile::TempDir,
    }

    fn ca_fixture() -> CaFixture {
        let dir = tempfile::tempdir().unwrap();
        utils::pki::init_mesh_ca(dir.path(), "24647a14a251e863cdf8dcee692f2915").unwrap();
        let (old_cert, old_key) = utils::pki::export_mesh_ca_keypair(dir.path()).unwrap();
        utils::pki::rotate_mesh_ca(dir.path()).unwrap();
        let (new_cert, new_key) = utils::pki::export_mesh_ca_keypair(dir.path()).unwrap();
        CaFixture {
            old_cert,
            old_key,
            new_cert,
            new_key,
            _dir: dir,
        }
    }

    fn rotation(f: &CaFixture, prev_key: &str, expires_at: Option<i64>) -> CaStatePayload {
        CaStatePayload {
            current_cert_pem: f.new_cert.clone(),
            current_key_pem: f.new_key.clone(),
            previous_cert_pem: Some(f.old_cert.clone()),
            previous_key_pem: Some(prev_key.to_string()),
            previous_expires_at: expires_at,
        }
    }

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn ca_rotation_chained_from_the_current_ca_is_accepted() {
        let f = ca_fixture();
        let p = rotation(&f, &f.old_key, Some(NOW + 7 * 86_400));
        check_ca_rotation(&p, &f.old_cert, None, &[], NOW).unwrap();
    }

    #[test]
    fn ca_state_push_without_a_previous_slot_is_refused() {
        let f = ca_fixture();
        let mut p = rotation(&f, &f.old_key, None);
        p.previous_cert_pem = None;
        p.previous_key_pem = None;
        let err = check_ca_rotation(&p, &f.old_cert, None, &[], NOW).unwrap_err();
        assert!(
            err.to_string().contains("rotation of the current CA"),
            "got: {err}"
        );
    }

    #[test]
    fn ca_rotation_from_a_foreign_ca_is_refused() {
        let f = ca_fixture();
        let foreign = ca_fixture();
        let p = rotation(&f, &f.old_key, None);
        let err = check_ca_rotation(&p, &foreign.old_cert, None, &[], NOW).unwrap_err();
        assert!(err.to_string().contains("does not chain"), "got: {err}");
    }

    #[test]
    fn ca_rotation_without_the_current_ca_key_is_refused() {
        let f = ca_fixture();
        let p = rotation(&f, &f.new_key, None);
        let err = check_ca_rotation(&p, &f.old_cert, None, &[], NOW).unwrap_err();
        let msg = peer_facing_error(MESH_PUSH_CA_STATE_METHOD, &err).message;
        assert!(msg.contains("did not prove possession"), "got: {msg}");
    }

    #[test]
    fn ca_rotation_back_to_the_previous_ca_is_refused() {
        let f = ca_fixture();
        let p = CaStatePayload {
            current_cert_pem: f.old_cert.clone(),
            current_key_pem: f.old_key.clone(),
            previous_cert_pem: Some(f.new_cert.clone()),
            previous_key_pem: Some(f.new_key.clone()),
            previous_expires_at: None,
        };
        let err = check_ca_rotation(&p, &f.new_cert, Some(&f.old_cert), &[], NOW).unwrap_err();
        assert!(err.to_string().contains("downgrade"), "got: {err}");
    }

    #[test]
    fn ca_rotation_to_any_retired_ca_is_refused() {
        let f = ca_fixture();
        let p = rotation(&f, &f.old_key, Some(NOW + 7 * 86_400));
        let retired = vec![
            "00:11".to_string(),
            utils::pki::cert_fingerprint(&f.new_cert).unwrap(),
        ];
        let err = check_ca_rotation(&p, &f.old_cert, None, &retired, NOW).unwrap_err();
        assert!(err.to_string().contains("previously retired"), "got: {err}");
        check_ca_rotation(&p, &f.old_cert, None, &retired[..1], NOW).unwrap();
    }

    #[test]
    fn ca_rotation_to_the_same_ca_is_refused() {
        let f = ca_fixture();
        let p = CaStatePayload {
            current_cert_pem: f.old_cert.clone(),
            current_key_pem: f.old_key.clone(),
            previous_cert_pem: Some(f.old_cert.clone()),
            previous_key_pem: Some(f.old_key.clone()),
            previous_expires_at: None,
        };
        let err = check_ca_rotation(&p, &f.old_cert, None, &[], NOW).unwrap_err();
        assert!(err.to_string().contains("already"), "got: {err}");
    }

    #[test]
    fn ca_rotation_with_an_unbounded_overlap_is_refused() {
        let f = ca_fixture();
        for exp in [NOW - 1, NOW + 91 * 86_400] {
            let p = rotation(&f, &f.old_key, Some(exp));
            let err = check_ca_rotation(&p, &f.old_cert, None, &[], NOW).unwrap_err();
            assert!(err.to_string().contains("90 days"), "got: {err}");
        }
    }

    #[test]
    fn ca_rotation_without_previous_expires_at_is_refused() {
        let f = ca_fixture();
        let p = rotation(&f, &f.old_key, None);
        let err = check_ca_rotation(&p, &f.old_cert, None, &[], NOW).unwrap_err();
        assert!(
            err.to_string().contains("previous_expires_at"),
            "got: {err}"
        );
    }

    #[test]
    fn ca_rotation_to_a_non_ca_cert_is_refused() {
        let f = ca_fixture();
        let (leaf, leaf_key) = utils::pki::load_or_init_bootstrap_cert(f._dir.path()).unwrap();
        let mut p = rotation(&f, &f.old_key, Some(NOW + 86_400));
        p.current_cert_pem = leaf;
        p.current_key_pem = leaf_key;
        let err = check_ca_rotation(&p, &f.old_cert, None, &[], NOW).unwrap_err();
        assert!(format!("{err:#}").contains("not a CA"), "got: {err:#}");
    }

    #[test]
    fn ca_rotation_to_a_ca_outside_its_validity_is_refused() {
        let f = ca_fixture();
        let late = i64::from(i32::MAX) * 4;
        let p = rotation(&f, &f.old_key, Some(late + 86_400));
        let err = check_ca_rotation(&p, &f.old_cert, None, &[], late).unwrap_err();
        assert!(format!("{err:#}").contains("validity"), "got: {err:#}");
    }

    const KEY_GRANT_NONCE: &str = "019f0000-0000-7000-8000-0000000000aa";

    fn member_peer(cn: &str) {
        crate::host_identity::init(&pki_dir()).ok();
        let conn = db::open_default().unwrap();
        pdb::upsert_peer(&conn, cn, "hc", "127.0.0.1", 1, Some("fp-c"), "").unwrap();
        pdb::set_secure(&conn, true).unwrap();
        let now = utils::time::now().unix_seconds();
        pdb::record_mesh_nonce(
            &conn,
            pdb::MESH_PROMOTE_NONCE_METHOD,
            cn,
            KEY_GRANT_NONCE,
            now + 600,
            now,
        )
        .unwrap();
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_ca_key_push_that_answers_no_grant_is_refused() {
        in_ca_home("unasked-key.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.new_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let mut p = key_payload(&f.new_cert, &f.new_key);
            p.grant_nonce = utils::id::new();
            let params = with_auth(&p, &key_auth(&cn, &p, &f.new_key));
            utils::pki::remove_mesh_ca_keys(&pki).unwrap();
            let err = handle_push_ca_key(&cn, req_with_params(MESH_PUSH_CA_KEY_METHOD, params))
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("promotion grant"),
                "got: {err:#}"
            );
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }

    fn with_auth<T: Serialize>(payload: &T, auth: &crate::mesh::exec::CaPushAuth) -> Value {
        let mut v = serde_json::to_value(payload).unwrap();
        v["auth"] = serde_json::to_value(auth).unwrap();
        v
    }

    fn key_auth(sender: &str, p: &CaKeyPayload, signer: &str) -> crate::mesh::exec::CaPushAuth {
        crate::mesh::exec::ca_push_auth(
            signer,
            MESH_PUSH_CA_KEY_METHOD,
            crate::host_identity::machine_id(),
            sender,
            p,
            utils::time::now().unix_seconds(),
        )
        .unwrap()
    }

    fn state_auth(sender: &str, p: &CaStatePayload, signer: &str) -> crate::mesh::exec::CaPushAuth {
        crate::mesh::exec::ca_push_auth(
            signer,
            MESH_PUSH_CA_STATE_METHOD,
            crate::host_identity::machine_id(),
            sender,
            p,
            utils::time::now().unix_seconds(),
        )
        .unwrap()
    }

    fn key_payload(cert: &str, key: &str) -> CaKeyPayload {
        CaKeyPayload {
            cert_pem: cert.to_string(),
            key_pem: key.to_string(),
            grant_nonce: KEY_GRANT_NONCE.to_string(),
        }
    }

    fn signed_key_push(sender: &str, cert: &str, key: &str) -> Value {
        let p = key_payload(cert, key);
        with_auth(&p, &key_auth(sender, &p, key))
    }

    fn state_payload(f: &CaFixture) -> CaStatePayload {
        rotation(
            f,
            &f.old_key,
            Some(utils::time::now().unix_seconds() + 7 * 86_400),
        )
    }

    fn signed_state_push(sender: &str, f: &CaFixture) -> Value {
        let p = state_payload(f);
        with_auth(&p, &state_auth(sender, &p, &f.old_key))
    }

    fn flip_one_byte(pem: &str) -> String {
        let mut b = pem.as_bytes().to_vec();
        let i = b.len() / 2;
        b[i] = if b[i] == b'A' { b'B' } else { b'A' };
        String::from_utf8(b).unwrap()
    }

    fn in_ca_home(name: &str, body: impl FnOnce()) {
        let _env = crate::test_env::snapshot();
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::mesh::pin_home(dir.path());
        db::with_thread_db_path(&dir.path().join(name), body);
    }

    fn pin_ca(cert: &str) -> std::path::PathBuf {
        let pki = pki_dir();
        std::fs::create_dir_all(utils::pki::mesh_dir(&pki)).unwrap();
        std::fs::write(utils::pki::mesh_ca_cert_path(&pki), cert).unwrap();
        pki
    }

    fn pinned_ca(pki: &std::path::Path) -> String {
        std::fs::read_to_string(utils::pki::mesh_ca_cert_path(pki)).unwrap()
    }

    #[serial_test::serial(env)]
    #[test]
    fn push_ca_key_without_a_ca_proof_is_refused() {
        in_ca_home("unproven-key.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.new_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let params = serde_json::to_value(key_payload(&f.new_cert, &f.new_key)).unwrap();
            let err = handle_push_ca_key(&cn, req_with_params(MESH_PUSH_CA_KEY_METHOD, params))
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("no CA-possession proof"),
                "got: {err:#}"
            );
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn push_ca_state_without_a_ca_proof_is_refused() {
        in_ca_home("unproven-state.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.old_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let params = serde_json::to_value(state_payload(&f)).unwrap();
            let err = handle_push_ca_state(&cn, req_with_params(MESH_PUSH_CA_STATE_METHOD, params))
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("no CA-possession proof"),
                "got: {err:#}"
            );
            assert_eq!(pinned_ca(&pki), f.old_cert);
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn push_ca_key_with_one_flipped_byte_is_refused() {
        in_ca_home("tampered-key.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.new_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let p = key_payload(&f.new_cert, &f.new_key);
            let auth = key_auth(&cn, &p, &f.new_key);
            let tampered = key_payload(&flip_one_byte(&f.new_cert), &f.new_key);
            let err = handle_push_ca_key(
                &cn,
                req_with_params(MESH_PUSH_CA_KEY_METHOD, with_auth(&tampered, &auth)),
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("did not verify"),
                "got: {err:#}"
            );
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn push_ca_state_with_one_flipped_byte_is_refused() {
        in_ca_home("tampered-state.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.old_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let p = state_payload(&f);
            let auth = state_auth(&cn, &p, &f.old_key);
            let mut tampered = p.clone();
            tampered.current_cert_pem = flip_one_byte(&p.current_cert_pem);
            let err = handle_push_ca_state(
                &cn,
                req_with_params(MESH_PUSH_CA_STATE_METHOD, with_auth(&tampered, &auth)),
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("did not verify"),
                "got: {err:#}"
            );
            assert_eq!(pinned_ca(&pki), f.old_cert);
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_replayed_ca_key_push_is_refused() {
        in_ca_home("replay-key.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.new_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let params = signed_key_push(&cn, &f.new_cert, &f.new_key);
            handle_push_ca_key(
                &cn,
                req_with_params(MESH_PUSH_CA_KEY_METHOD, params.clone()),
            )
            .unwrap();
            utils::pki::remove_mesh_ca_keys(&pki).unwrap();
            let err = handle_push_ca_key(&cn, req_with_params(MESH_PUSH_CA_KEY_METHOD, params))
                .unwrap_err();
            assert!(format!("{err:#}").contains("replay"), "got: {err:#}");
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_replayed_ca_state_push_is_refused() {
        in_ca_home("replay-state.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.old_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let params = signed_state_push(&cn, &f);
            handle_push_ca_state(
                &cn,
                req_with_params(MESH_PUSH_CA_STATE_METHOD, params.clone()),
            )
            .unwrap();
            std::fs::write(utils::pki::mesh_ca_cert_path(&pki), &f.old_cert).unwrap();
            let err = handle_push_ca_state(&cn, req_with_params(MESH_PUSH_CA_STATE_METHOD, params))
                .unwrap_err();
            assert!(format!("{err:#}").contains("replay"), "got: {err:#}");
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn the_ca_key_is_never_pushed_onto_an_insecure_receiver() {
        in_ca_home("insecure-receiver.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.new_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            pdb::set_secure(&db::open_default().unwrap(), false).unwrap();
            let params = signed_key_push(&cn, &f.new_cert, &f.new_key);
            let err = handle_push_ca_key(&cn, req_with_params(MESH_PUSH_CA_KEY_METHOD, params))
                .unwrap_err();
            assert!(format!("{err:#}").contains("insecure"), "got: {err:#}");
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn ca_state_is_never_pushed_onto_an_insecure_receiver() {
        in_ca_home("insecure-state.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.old_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            pdb::set_secure(&db::open_default().unwrap(), false).unwrap();
            let params = signed_state_push(&cn, &f);
            let err = handle_push_ca_state(&cn, req_with_params(MESH_PUSH_CA_STATE_METHOD, params))
                .unwrap_err();
            assert!(format!("{err:#}").contains("insecure"), "got: {err:#}");
            assert_eq!(pinned_ca(&pki), f.old_cert);
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_ca_push_from_a_non_member_is_refused() {
        in_ca_home("non-member.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.new_cert);
            crate::host_identity::init(&pki).ok();
            pdb::set_secure(&db::open_default().unwrap(), true).unwrap();
            let cn = utils::id::new();
            let params = signed_key_push(&cn, &f.new_cert, &f.new_key);
            let err = handle_push_ca_key(&cn, req_with_params(MESH_PUSH_CA_KEY_METHOD, params))
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("not a verified member"),
                "got: {err:#}"
            );
            let err = handle_push_ca_state(
                &cn,
                req_with_params(MESH_PUSH_CA_STATE_METHOD, signed_state_push(&cn, &f)),
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("not a verified member"),
                "got: {err:#}"
            );
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }

    fn member_key(cn: &str) -> ed25519_dalek::SigningKey {
        let key = ed25519_dalek::SigningKey::from_bytes(&utils::hash::sha256(cn.as_bytes()));
        let fp = utils::pki::bootstrap_pubkey_fingerprint(&key.verifying_key());
        let conn = db::open_default().unwrap();
        pdb::upsert_peer(&conn, cn, "hm", "127.0.0.1", 1, Some(&fp), "").unwrap();
        key
    }

    fn bound_bundle(
        key: &ed25519_dalek::SigningKey,
        sender: &str,
        receiver: &str,
        now: i64,
    ) -> utils::pki::SignedEnvelope {
        bound_bundle_with(key, sender, receiver, now, Default::default())
    }

    fn bound_bundle_with(
        key: &ed25519_dalek::SigningKey,
        sender: &str,
        receiver: &str,
        now: i64,
        entities: std::collections::BTreeMap<String, Value>,
    ) -> utils::pki::SignedEnvelope {
        let body = super::super::ReplicateBundle {
            peer_id: sender.to_string(),
            issued_at: now,
            entities,
        };
        utils::pki::sign_bound_envelope(
            key,
            &body,
            MESH_REPLICATE_PUSH_METHOD,
            sender,
            receiver,
            &utils::id::new(),
            now,
            60,
        )
        .unwrap()
    }

    fn push_bundle(cn: &str, env: &utils::pki::SignedEnvelope) -> String {
        match handle_replicate_push(
            cn,
            req_with_params(
                MESH_REPLICATE_PUSH_METHOD,
                serde_json::to_value(env).unwrap(),
            ),
        ) {
            Ok(_) => "accepted".into(),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn a_bound_replicate_push_is_merged_once() {
        with_db(async {
            let cn = utils::id::new();
            let key = member_key(&cn);
            let env = bound_bundle(
                &key,
                &cn,
                crate::host_identity::machine_id(),
                utils::time::now().unix_seconds(),
            );
            assert_eq!(push_bundle(&cn, &env), "accepted");
            let again = push_bundle(&cn, &env);
            assert!(again.contains("replay"), "{again}");
        });
    }

    #[test]
    fn a_replicate_push_for_another_receiver_expired_tampered_or_unbound_is_refused() {
        with_db(async {
            let cn = utils::id::new();
            let key = member_key(&cn);
            let me = crate::host_identity::machine_id();
            let now = utils::time::now().unix_seconds();
            let other = push_bundle(&cn, &bound_bundle(&key, &cn, &utils::id::new(), now));
            assert!(other.contains("another receiver"), "{other}");
            let expired = push_bundle(&cn, &bound_bundle(&key, &cn, me, now - 3_600));
            assert!(expired.contains("expired"), "{expired}");
            let mut tampered = bound_bundle(&key, &cn, me, now);
            tampered.payload = tampered.payload.replacen("{", "{\"x\":1,", 1);
            let tampered = push_bundle(&cn, &tampered);
            assert!(tampered.contains("did not verify"), "{tampered}");
            let mut unbound = bound_bundle(&key, &cn, me, now);
            unbound.binding = None;
            pdb::mark_bound_envelopes(&db::open_default().unwrap(), &cn).unwrap();
            let unbound = push_bundle(&cn, &unbound);
            assert!(unbound.contains("not bound"), "{unbound}");
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_revocation_arriving_by_replicate_push_purges_the_key_without_a_restart() {
        in_ca_home("revoked-by-push.db", || {
            let pki = pki_dir();
            utils::pki::init_mesh_ca(&pki, "holder").unwrap();
            crate::host_identity::init(&pki).ok();
            let me = crate::host_identity::machine_id();
            let conn = db::open_default().unwrap();
            pdb::set_secure(&conn, true).unwrap();
            crate::mesh::trust_registry::grant(&conn, &pki, me).unwrap();
            let scratch_dir = tempfile::tempdir().unwrap();
            let scratch = db::open_unencrypted(&scratch_dir.path().join("holder.db")).unwrap();
            crate::mesh::trust_registry::revoke(&scratch, &pki, me).unwrap();
            let rows = serde_json::to_value(pdb::registry_entries(&scratch).unwrap()).unwrap();
            let cn = utils::id::new();
            let key = member_key(&cn);
            let env = bound_bundle_with(
                &key,
                &cn,
                me,
                utils::time::now().unix_seconds(),
                [("mesh_secure_registry".to_string(), rows)].into(),
            );
            assert_eq!(push_bundle(&cn, &env), "accepted");
            assert!(!utils::pki::has_mesh_ca_key(&pki));
            assert!(!pdb::get_secure(&conn).unwrap());
        });
    }

    #[test]
    fn an_unbound_replicate_push_is_accepted_only_from_a_pre_binding_member() {
        with_db(async {
            let cn = utils::id::new();
            let key = member_key(&cn);
            let me = crate::host_identity::machine_id();
            let now = utils::time::now().unix_seconds();
            let mut unbound = bound_bundle(&key, &cn, me, now);
            unbound.binding = None;
            assert_eq!(push_bundle(&cn, &unbound), "accepted");
            assert_eq!(
                push_bundle(&cn, &bound_bundle(&key, &cn, me, now)),
                "accepted"
            );
            let after = push_bundle(&cn, &unbound);
            assert!(after.contains("not bound"), "{after}");
        });
    }

    #[test]
    fn a_replicate_push_from_a_departed_member_is_refused() {
        with_db(async {
            let cn = utils::id::new();
            let key = member_key(&cn);
            let conn = db::open_default().unwrap();
            conn.execute(
                "UPDATE mesh_peers SET departed_at = 1 WHERE peer_id = ?1",
                [&cn],
            )
            .unwrap();
            let env = bound_bundle(
                &key,
                &cn,
                crate::host_identity::machine_id(),
                utils::time::now().unix_seconds(),
            );
            let out = push_bundle(&cn, &env);
            assert!(out.contains("not a verified member"), "{out}");
        });
    }

    #[test]
    fn a_replicate_export_is_bound_to_the_requester_and_its_nonce() {
        with_db(async {
            let requester = utils::id::new();
            let nonce = utils::id::new();
            let env = handle_replicate_export(
                &requester,
                req_with_params(
                    MESH_REPLICATE_EXPORT_METHOD,
                    serde_json::json!({ "nonce": nonce }),
                ),
            )
            .unwrap();
            let me = crate::host_identity::machine_id().to_string();
            let fp = utils::pki::bootstrap_pubkey_fingerprint(
                &utils::pki::load_or_init_bootstrap_key(&pki_dir())
                    .unwrap()
                    .verifying_key(),
            );
            crate::mesh::transport::verify_export(&env, &fp, &me, &requester, &nonce).unwrap();
            let err =
                crate::mesh::transport::verify_export(&env, &fp, &me, &utils::id::new(), &nonce)
                    .unwrap_err();
            assert!(format!("{err:#}").contains("another receiver"), "{err:#}");
            let err = crate::mesh::transport::verify_export(
                &env,
                &fp,
                &me,
                &requester,
                &utils::id::new(),
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains("nonce"), "{err:#}");
            let mut tampered = env.clone();
            tampered.payload = tampered.payload.replacen('{', "{\"x\":1,", 1);
            let err =
                crate::mesh::transport::verify_export(&tampered, &fp, &me, &requester, &nonce)
                    .unwrap_err();
            assert!(format!("{err:#}").contains("did not verify"), "{err:#}");
        });
    }

    fn bound_refresh(cn: &str, key: &ed25519_dalek::SigningKey, receiver: &str, now: i64) -> Value {
        let body = refresh_params(cn);
        let env = utils::pki::sign_bound_envelope(
            key,
            &body,
            MESH_REFRESH_CERT_METHOD,
            cn,
            receiver,
            &utils::id::new(),
            now,
            60,
        )
        .unwrap();
        let mut v = body;
        v["envelope"] = serde_json::to_value(&env).unwrap();
        v
    }

    fn refresh(cn: &str, params: Value) -> String {
        match handle_refresh_cert(cn, req_with_params(MESH_REFRESH_CERT_METHOD, params)) {
            Ok(_) => "accepted".into(),
            Err(e) => format!("{e:#}"),
        }
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_bound_refresh_cert_is_signed_once() {
        in_ca_home("bound-refresh.db", || {
            utils::pki::init_mesh_ca(&pki_dir(), "24647a14a251e863cdf8dcee692f2915").unwrap();
            crate::host_identity::init(&pki_dir()).ok();
            let cn = utils::id::new();
            let key = member_key(&cn);
            let params = bound_refresh(
                &cn,
                &key,
                crate::host_identity::machine_id(),
                utils::time::now().unix_seconds(),
            );
            assert_eq!(refresh(&cn, params.clone()), "accepted");
            let again = refresh(&cn, params);
            assert!(again.contains("replay"), "{again}");
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_refresh_cert_for_another_receiver_expired_tampered_or_unbound_is_refused() {
        in_ca_home("bound-refresh-refused.db", || {
            utils::pki::init_mesh_ca(&pki_dir(), "24647a14a251e863cdf8dcee692f2915").unwrap();
            crate::host_identity::init(&pki_dir()).ok();
            let cn = utils::id::new();
            let key = member_key(&cn);
            let me = crate::host_identity::machine_id();
            let now = utils::time::now().unix_seconds();
            let other = refresh(&cn, bound_refresh(&cn, &key, &utils::id::new(), now));
            assert!(other.contains("another receiver"), "{other}");
            let expired = refresh(&cn, bound_refresh(&cn, &key, me, now - 3_600));
            assert!(expired.contains("expired"), "{expired}");
            let mut tampered = bound_refresh(&cn, &key, me, now);
            tampered["envelope"]["binding"]["expires_at"] = serde_json::json!(now + 61);
            let tampered = refresh(&cn, tampered);
            assert!(tampered.contains("did not verify"), "{tampered}");
            pdb::mark_bound_envelopes(&db::open_default().unwrap(), &cn).unwrap();
            let unbound = refresh(&cn, refresh_params(&cn));
            assert!(unbound.contains("not bound"), "{unbound}");
            let stranger = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
            let wrong_key = refresh(&cn, bound_refresh(&cn, &stranger, me, now));
            assert!(wrong_key.contains("pinned"), "{wrong_key}");
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn unbound_refresh_follows_the_require_bound_switch() {
        in_ca_home("legacy-refresh.db", || {
            utils::pki::init_mesh_ca(&pki_dir(), "24647a14a251e863cdf8dcee692f2915").unwrap();
            crate::host_identity::init(&pki_dir()).ok();
            let cn = utils::id::new();
            member_key(&cn);
            assert_eq!(refresh(&cn, refresh_params(&cn)), "accepted");
            let conn = db::open_default().unwrap();
            pdb::mark_bound_envelopes(&conn, &cn).unwrap();
            crate::mesh::trust_registry::enable_require_bound(&conn, &pki_dir()).unwrap();
            conn.execute(
                "UPDATE mesh_peers SET bound_envelopes = 0 WHERE peer_id = ?1",
                [&cn],
            )
            .unwrap();
            let refused = refresh(&cn, refresh_params(&cn));
            assert!(refused.contains("not bound"), "{refused}");
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_ca_proof_for_another_receiver_is_refused() {
        in_ca_home("other-receiver.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.new_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let p = key_payload(&f.new_cert, &f.new_key);
            let auth = crate::mesh::exec::ca_push_auth(
                &f.new_key,
                MESH_PUSH_CA_KEY_METHOD,
                &utils::id::new(),
                &cn,
                &p,
                utils::time::now().unix_seconds(),
            )
            .unwrap();
            let err = handle_push_ca_key(
                &cn,
                req_with_params(MESH_PUSH_CA_KEY_METHOD, with_auth(&p, &auth)),
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("did not verify"),
                "got: {err:#}"
            );
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_ca_key_proof_replayed_against_push_ca_state_is_refused() {
        in_ca_home("cross-method.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.old_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let state = state_payload(&f);
            let key_proof = crate::mesh::exec::ca_push_auth(
                &f.old_key,
                MESH_PUSH_CA_KEY_METHOD,
                crate::host_identity::machine_id(),
                &cn,
                &state,
                utils::time::now().unix_seconds(),
            )
            .unwrap();
            let err = handle_push_ca_state(
                &cn,
                req_with_params(MESH_PUSH_CA_STATE_METHOD, with_auth(&state, &key_proof)),
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("did not verify"),
                "got: {err:#}"
            );
            assert_eq!(pinned_ca(&pki), f.old_cert);
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_ca_proof_valid_beyond_the_push_window_is_refused() {
        in_ca_home("far-future.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.new_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let p = key_payload(&f.new_cert, &f.new_key);
            let auth = crate::mesh::exec::ca_push_auth(
                &f.new_key,
                MESH_PUSH_CA_KEY_METHOD,
                crate::host_identity::machine_id(),
                &cn,
                &p,
                utils::time::now().unix_seconds() + 60,
            )
            .unwrap();
            let err = handle_push_ca_key(
                &cn,
                req_with_params(MESH_PUSH_CA_KEY_METHOD, with_auth(&p, &auth)),
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains("expired"), "got: {err:#}");
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn an_expired_ca_proof_is_refused_before_it_is_verified() {
        in_ca_home("expired-proof.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.new_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let p = key_payload(&f.new_cert, &f.new_key);
            let auth = crate::mesh::exec::ca_push_auth(
                &f.new_key,
                MESH_PUSH_CA_KEY_METHOD,
                crate::host_identity::machine_id(),
                &cn,
                &p,
                utils::time::now().unix_seconds() - 3_600,
            )
            .unwrap();
            let err = handle_push_ca_key(
                &cn,
                req_with_params(MESH_PUSH_CA_KEY_METHOD, with_auth(&p, &auth)),
            )
            .unwrap_err();
            let err = format!("{err:#}");
            assert!(
                err.contains("proof is expired or not yet valid"),
                "got: {err}"
            );
            assert!(utils::pki::receiver_time_in(&err).is_some(), "got: {err}");
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_ca_push_from_a_departed_member_is_refused() {
        in_ca_home("departed.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.new_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            pdb::mark_peer_departed(&db::open_default().unwrap(), &cn).unwrap();
            let params = signed_key_push(&cn, &f.new_cert, &f.new_key);
            let err = handle_push_ca_key(&cn, req_with_params(MESH_PUSH_CA_KEY_METHOD, params))
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("not a verified member"),
                "got: {err:#}"
            );
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }

    #[test]
    fn has_ca_key_does_not_report_a_tier() {
        let r = serde_json::to_string(&HasCaKeyResult {
            has_key: false,
            proof: None,
        })
        .unwrap();
        assert!(!r.contains("secure"), "{r}");
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_ca_proof_signed_for_another_sender_is_refused() {
        in_ca_home("other-sender.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.new_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let params = signed_key_push(&utils::id::new(), &f.new_cert, &f.new_key);
            let err = handle_push_ca_key(&cn, req_with_params(MESH_PUSH_CA_KEY_METHOD, params))
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("did not verify"),
                "got: {err:#}"
            );
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn an_expired_ca_proof_is_refused() {
        in_ca_home("expired-proof.db", || {
            let f = ca_fixture();
            let pki = pin_ca(&f.new_cert);
            let cn = utils::id::new();
            member_peer(&cn);
            let p = key_payload(&f.new_cert, &f.new_key);
            let auth = crate::mesh::exec::ca_push_auth(
                &f.new_key,
                MESH_PUSH_CA_KEY_METHOD,
                crate::host_identity::machine_id(),
                &cn,
                &p,
                utils::time::now().unix_seconds() - 3_600,
            )
            .unwrap();
            let err = handle_push_ca_key(
                &cn,
                req_with_params(MESH_PUSH_CA_KEY_METHOD, with_auth(&p, &auth)),
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains("expired"), "got: {err:#}");
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn push_ca_state_installs_a_proven_rotation_from_a_member() {
        let _env = crate::test_env::snapshot();
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::mesh::pin_home(dir.path());
        db::with_thread_db_path(&dir.path().join("ca-state-ok.db"), || {
            let f = ca_fixture();
            let pki = pki_dir();
            std::fs::create_dir_all(utils::pki::mesh_dir(&pki)).unwrap();
            std::fs::write(utils::pki::mesh_ca_cert_path(&pki), &f.old_cert).unwrap();
            let cn = utils::id::new();
            member_peer(&cn);

            let params = signed_state_push(&cn, &f);
            handle_push_ca_state(&cn, req_with_params(MESH_PUSH_CA_STATE_METHOD, params)).unwrap();
            assert_eq!(
                std::fs::read_to_string(utils::pki::mesh_ca_cert_path(&pki)).unwrap(),
                f.new_cert
            );
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn a_foreign_ca_state_is_refused() {
        let _env = crate::test_env::snapshot();
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::mesh::pin_home(dir.path());
        db::with_thread_db_path(&dir.path().join("ca-state-swap.db"), || {
            let ours = ca_fixture();
            let attacker = ca_fixture();
            let pki = pki_dir();
            std::fs::create_dir_all(utils::pki::mesh_dir(&pki)).unwrap();
            std::fs::write(utils::pki::mesh_ca_cert_path(&pki), &ours.old_cert).unwrap();
            let cn = utils::id::new();
            member_peer(&cn);

            let params = signed_state_push(&cn, &attacker);
            let err = handle_push_ca_state(&cn, req_with_params(MESH_PUSH_CA_STATE_METHOD, params))
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("did not verify"),
                "got: {err:#}"
            );
            assert_eq!(
                std::fs::read_to_string(utils::pki::mesh_ca_cert_path(&pki)).unwrap(),
                ours.old_cert
            );
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn push_ca_key_installs_the_pinned_ca_key_on_a_keyless_host() {
        let _env = crate::test_env::snapshot();
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::mesh::pin_home(dir.path());
        db::with_thread_db_path(&dir.path().join("ca-key-ok.db"), || {
            let f = ca_fixture();
            let pki = pki_dir();
            std::fs::create_dir_all(utils::pki::mesh_dir(&pki)).unwrap();
            std::fs::write(utils::pki::mesh_ca_cert_path(&pki), &f.new_cert).unwrap();
            let cn = utils::id::new();
            member_peer(&cn);

            let params = signed_key_push(&cn, &f.new_cert, &f.new_key);
            handle_push_ca_key(&cn, req_with_params(MESH_PUSH_CA_KEY_METHOD, params)).unwrap();
            assert!(utils::pki::has_mesh_ca_key(&pki));
            let params = signed_key_push(&cn, &f.new_cert, &f.new_key);
            let err = handle_push_ca_key(&cn, req_with_params(MESH_PUSH_CA_KEY_METHOD, params))
                .unwrap_err();
            assert!(format!("{err:#}").contains("already holds"), "got: {err:#}");
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn push_ca_key_refused_when_this_host_already_holds_a_ca_key() {
        let _env = crate::test_env::snapshot();
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::mesh::pin_home(dir.path());
        db::with_thread_db_path(&dir.path().join("ca-key-held.db"), || {
            let pki = pki_dir();
            utils::pki::init_mesh_ca(&pki, "24647a14a251e863cdf8dcee692f2915").unwrap();
            let (cert, key) = utils::pki::export_mesh_ca_keypair(&pki).unwrap();
            let cn = utils::id::new();
            member_peer(&cn);

            let params = signed_key_push(&cn, &cert, &key);
            let err = handle_push_ca_key(&cn, req_with_params(MESH_PUSH_CA_KEY_METHOD, params))
                .unwrap_err();
            assert!(format!("{err:#}").contains("already holds"), "got: {err:#}");
            assert_eq!(
                std::fs::read_to_string(utils::pki::mesh_ca_key_path(&pki)).unwrap(),
                key
            );
        });
    }

    #[test]
    fn remote_role_gate_refuses_unroutable_and_mutating_any_tools() {
        let err = remote_role_gate("ghost.tool", None, false).unwrap_err();
        assert!(err.to_string().contains("not routable"), "got: {err}");
        let err = remote_role_gate("x.apply", Some("any"), true).unwrap_err();
        assert!(err.to_string().contains("changes state"), "got: {err}");
        let err = remote_role_gate("auth.login", Some("any"), false).unwrap_err();
        assert!(err.to_string().contains("changes state"), "got: {err}");
        assert_eq!(
            remote_role_gate("system.list", Some("any"), false).unwrap(),
            "any"
        );
        assert_eq!(
            remote_role_gate("schedule.owner.list", Some("admin"), true).unwrap(),
            "admin"
        );
    }

    #[tokio::test]
    async fn exec_refuses_an_any_role_plugin_tool_that_is_execute_gated() {
        install_fake_plugin();
        let tool = "listener_test_plugin.any_apply";
        dispatch::tool_roles::install_plugin_tools([(
            tool.to_string(),
            dispatch::tool_roles::PluginToolPolicy::from_manifest(
                tool,
                Some("any"),
                Some(true),
                None,
            ),
        )]);
        let params = serde_json::json!({ "tool": tool, "args": {} });
        let err = handle_exec(req_with_params(MESH_EXEC_METHOD, params), "peer-a")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("changes state"), "got: {err}");
    }

    #[tokio::test]
    async fn exec_refuses_an_any_role_plugin_tool_with_a_write_verb_and_no_flags() {
        install_fake_plugin();
        let tool = "listener_test_plugin.update";
        dispatch::tool_roles::install_plugin_tools([(
            tool.to_string(),
            dispatch::tool_roles::PluginToolPolicy::from_manifest(tool, Some("any"), None, None),
        )]);
        let params = serde_json::json!({ "tool": tool, "args": {} });
        let err = handle_exec(req_with_params(MESH_EXEC_METHOD, params), "peer-a")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("changes state"), "got: {err}");
    }

    #[test]
    fn a_dry_run_aware_plugin_tool_counts_as_mutating() {
        let tool = "listener_test_plugin.aware_status";
        dispatch::tool_roles::install_plugin_tools([(
            tool.to_string(),
            dispatch::tool_roles::PluginToolPolicy::from_manifest(
                tool,
                Some("any"),
                Some(false),
                Some(false),
            )
            .dry_run_aware(true),
        )]);
        assert!(is_mutating_tool(tool));
        dispatch::tool_roles::remove_plugin_tools([tool]);
    }

    #[test]
    fn any_role_plugin_tool_explicitly_ungated_needs_an_explicit_read_for_mesh() {
        let unconfirmed = "listener_test_plugin.status";
        let confirmed = "listener_test_plugin.detail";
        dispatch::tool_roles::install_plugin_tools([
            (
                unconfirmed.to_string(),
                dispatch::tool_roles::PluginToolPolicy::from_manifest(
                    unconfirmed,
                    Some("any"),
                    Some(false),
                    None,
                ),
            ),
            (
                confirmed.to_string(),
                dispatch::tool_roles::PluginToolPolicy::from_manifest(
                    confirmed,
                    Some("any"),
                    Some(false),
                    Some(false),
                ),
            ),
        ]);
        let err = remote_role_gate(
            unconfirmed,
            dispatch::tool_roles::known_role(unconfirmed),
            is_mutating_tool(unconfirmed),
        )
        .unwrap_err();
        assert!(err.to_string().contains("changes state"), "got: {err}");
        assert_eq!(
            remote_role_gate(
                confirmed,
                dispatch::tool_roles::known_role(confirmed),
                is_mutating_tool(confirmed)
            )
            .unwrap(),
            "any"
        );
    }

    #[test]
    fn any_role_plugin_tool_with_a_read_verb_and_no_flags_is_not_gated() {
        let tool = "listener_test_plugin.list";
        let policy =
            dispatch::tool_roles::PluginToolPolicy::from_manifest(tool, Some("any"), None, None);
        assert!(!policy.execute_gated);
        dispatch::tool_roles::install_plugin_tools([(tool.to_string(), policy)]);
        assert!(!is_mutating_tool(tool));
        assert_eq!(
            remote_role_gate(
                tool,
                dispatch::tool_roles::known_role(tool),
                is_mutating_tool(tool)
            )
            .unwrap(),
            "any"
        );
    }

    #[test]
    fn every_tier_meets_exactly_the_requirements_at_or_below_it() {
        let tiers = ["any", "read-only", "user", "admin", "super-admin"];
        for (ci, caller) in tiers.iter().enumerate() {
            for (ri, required) in tiers.iter().enumerate() {
                assert_eq!(
                    role_satisfies(caller, required),
                    ci >= ri,
                    "{caller} vs {required}"
                );
            }
        }
        assert!(role_satisfies("read", "read-only"));
        assert!(role_satisfies("read-only", "read"));
        assert!(role_satisfies("member", "read"));
        assert!(!role_satisfies("read", "member"));
        assert!(!role_satisfies("any", "read"));
    }

    #[test]
    fn peer_facing_error_hides_local_io_detail() {
        let io = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let e = anyhow::Error::from(io).context("open tmp /secret/path/ca.key.pem.tmp");
        let obj = peer_facing_error(MESH_PUSH_CA_KEY_METHOD, &e);
        assert!(!obj.message.contains("/secret"), "got: {}", obj.message);
        assert_eq!(obj.message, "internal error: mesh/push-ca-key failed");
        let refusal =
            anyhow::Error::new(PeerRefusal("mesh/push-ca-key refused: not mutual".into()));
        let obj = peer_facing_error(MESH_PUSH_CA_KEY_METHOD, &refusal);
        assert_eq!(
            obj.message,
            "internal error: mesh/push-ca-key refused: not mutual"
        );
        let bailed: anyhow::Result<()> = (|| anyhow::bail!("parse /etc/orca/pki/x.pem failed"))();
        let obj = peer_facing_error(MESH_EXEC_METHOD, &bailed.unwrap_err());
        assert!(!obj.message.contains("/etc"), "got: {}", obj.message);
        let wrapped = anyhow::anyhow!("open /secret/ca.key.pem").context(PeerRefusal(
            "mesh/push-ca-state refused: retired CA history is unreadable".into(),
        ));
        let obj = peer_facing_error(MESH_PUSH_CA_STATE_METHOD, &wrapped);
        assert_eq!(
            obj.message,
            "internal error: mesh/push-ca-state refused: retired CA history is unreadable"
        );
        let missing = anyhow::Error::new(PeerRefusal("mesh/exec requires params".into()));
        let untyped = anyhow::anyhow!("mesh/exec refused: forged by a plain string");
        assert_eq!(
            peer_facing_error(MESH_EXEC_METHOD, &untyped).message,
            "internal error: mesh/exec failed"
        );
        assert_eq!(
            peer_facing_error(MESH_EXEC_METHOD, &missing).message,
            "internal error: mesh/exec requires params"
        );
    }

    #[serial_test::serial(env)]
    #[test]
    fn push_ca_state_refused_when_retired_history_is_unreadable() {
        let _env = crate::test_env::snapshot();
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::mesh::pin_home(dir.path());
        db::with_thread_db_path(&dir.path().join("ca-state-unreadable.db"), || {
            let f = ca_fixture();
            let pki = pki_dir();
            std::fs::create_dir_all(utils::pki::mesh_ca_retired_path(&pki)).unwrap();
            std::fs::write(utils::pki::mesh_ca_cert_path(&pki), &f.old_cert).unwrap();
            let cn = utils::id::new();
            member_peer(&cn);

            let params = signed_state_push(&cn, &f);
            let err = handle_push_ca_state(&cn, req_with_params(MESH_PUSH_CA_STATE_METHOD, params))
                .unwrap_err();
            assert!(format!("{err:#}").contains("unreadable"), "got: {err:#}");
            assert_eq!(
                std::fs::read_to_string(utils::pki::mesh_ca_cert_path(&pki)).unwrap(),
                f.old_cert
            );
            assert!(utils::pki::mesh_ca_retired_path(&pki).is_dir());
        });
    }

    #[serial_test::serial(env)]
    #[test]
    fn push_ca_key_refuses_a_retired_ca() {
        let _env = crate::test_env::snapshot();
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::mesh::pin_home(dir.path());
        db::with_thread_db_path(&dir.path().join("ca-key-retired.db"), || {
            let f = ca_fixture();
            let pki = pki_dir();
            std::fs::create_dir_all(utils::pki::mesh_dir(&pki)).unwrap();
            std::fs::write(utils::pki::mesh_ca_cert_path(&pki), &f.old_cert).unwrap();
            let fp = utils::pki::cert_fingerprint(&f.old_cert).unwrap();
            std::fs::write(utils::pki::mesh_ca_retired_path(&pki), format!("{fp}\n")).unwrap();
            let cn = utils::id::new();
            member_peer(&cn);

            let params = signed_key_push(&cn, &f.old_cert, &f.old_key);
            let err = handle_push_ca_key(&cn, req_with_params(MESH_PUSH_CA_KEY_METHOD, params))
                .unwrap_err();
            assert!(format!("{err:#}").contains("retired"), "got: {err:#}");
            assert!(!utils::pki::has_mesh_ca_key(&pki));
        });
    }
}
