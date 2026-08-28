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
    AddressChannel, HostAddressingSnapshot, POD_DEV_DISABLE_METHOD, POD_DEV_ENABLE_METHOD,
    POD_DEV_SYNC_METHOD, POD_EXEC_METHOD, POD_PING_METHOD, POD_REPLICATE_EXPORT_METHOD,
    POD_REPLICATE_PUSH_METHOD, POD_REPLICATE_ROOTS_METHOD, PodDevDisableResult, PodDevEnableResult,
    PodDevSyncResult, PodExecParams, PodExecResult, PodPingResult, ReplicatePushResult,
    ReplicateRootsResult, pki_dir,
};
use db::pod as pdb;

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
    if request.method == crate::subscribe_wire::METHOD {
        let own_peer_id = system::host_identity::machine_id().to_string();
        return crate::subscribe_wire::serve_session_with_request(tls, request, &own_peer_id).await;
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
        // DB unavailable (fallback open error) or a query error falls through to
        // the method handlers, which will fail with a clearer error.
        let departed = db::pool::with_pooled_or_open(|conn| {
            Ok(pdb::is_peer_departed(conn, peer_cn).unwrap_or(false))
        })
        .unwrap_or(false);
        if departed {
            return Response::err(
                id,
                ErrorObject::method_not_found(&format!(
                    "peer {peer_cn} has departed this pod; re-pair to re-establish trust"
                )),
            );
        }
    }

    match method.as_str() {
        POD_PING_METHOD => {
            let result = PodPingResult {
                peer_id: peer_cn.to_string(),
                version: option_env!("ORCA_VERSION")
                    .unwrap_or(env!("CARGO_PKG_VERSION"))
                    .to_string(),
                hostname: system::host_identity::hostname().to_string(),
                addressing: build_addressing_snapshot(),
            };
            value_response(id, &result)
        }
        POD_DEV_SYNC_METHOD => match handle_dev_sync().await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
        },
        POD_DEV_ENABLE_METHOD => match handle_dev_enable().await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
        },
        POD_DEV_DISABLE_METHOD => match handle_dev_disable().await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
        },
        POD_EXEC_METHOD => match handle_exec(request, peer_cn).await {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
        },
        POD_REPLICATE_EXPORT_METHOD => match handle_replicate_export() {
            Ok(env) => value_response(id, &env),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
        },
        POD_REPLICATE_PUSH_METHOD => match handle_replicate_push(peer_cn, request) {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
        },
        POD_REPLICATE_ROOTS_METHOD => match handle_replicate_roots() {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
        },
        POD_NOTIFY_TRUST_METHOD => match handle_notify_trust(peer_cn, peer_addr, request) {
            Ok(()) => Response::ok(id, Value::Null),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
        },
        POD_HAS_CA_KEY_METHOD => {
            let has = utils::pki::has_mesh_ca_key(&pki_dir());
            value_response(id, &HasCaKeyResult { has_key: has })
        }
        POD_PUSH_CA_KEY_METHOD => match handle_push_ca_key(peer_cn, request) {
            Ok(()) => Response::ok(id, Value::Null),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
        },
        POD_PEER_LEAVING_METHOD => match handle_peer_leaving(peer_cn) {
            Ok(()) => Response::ok(id, Value::Null),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
        },
        POD_PEER_REMOVED_METHOD => {
            // Caller (peer_cn) is telling us they've kicked us from their pod.
            // Log it; do NOT mark the caller as departed — that's
            // `pod/peer-leaving`'s job. Reusing this method for kick was the
            // 2026-05-28 bug that departed mint on alpha/echo.
            tracing::info!("[pod] peer {peer_cn} removed us from their pod");
            Response::ok(id, Value::Null)
        }
        POD_PEER_FORGET_METHOD => match handle_peer_forget(peer_cn, request) {
            Ok(removed) => value_response(id, &serde_json::json!({ "rows_removed": removed })),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
        },
        POD_REFRESH_CERT_METHOD => match handle_refresh_cert(peer_cn, request) {
            Ok(r) => value_response(id, &r),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
        },
        POD_PUSH_CA_STATE_METHOD => match handle_push_ca_state(peer_cn, request) {
            Ok(()) => Response::ok(id, Value::Null),
            Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
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
    // Share one pooled connection for the whole self-heal + trust sequence.
    db::pool::with_pooled_or_open(|conn| {
        // Self-heal: the mTLS layer validated this CN against the mesh CA, so we
        // can trust it. If no pod_peers row exists yet (legacy rc.≤24 joiner that
        // landed as peer_id="unknown", or CN/peer_id drift), materialize a stub
        // keyed by the CN so the FK on pod_trust.peer_id is satisfied.
        let addr_ip = peer_addr.ip().to_string();
        pdb::ensure_peer_stub(conn, peer_cn, &addr_ip, db::ports::mesh_port())?;
        // Self-heal identity drift: this CN is CA-validated ground truth for the
        // host at `addr_ip`, so fold any stale sibling rows at that address (a
        // legacy `peer.<id>` CN, or a re-keyed identity) into this canonical id.
        // Keeps `pod list` and `--peer <hostname>` converged automatically.
        match pdb::reconcile_addr_to_canonical(conn, peer_cn, &addr_ip) {
            Ok(n) if n > 0 => {
                tracing::info!("[pod] converged {n} stale peer row(s) into {peer_cn}")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("[pod] peer identity reconcile for {peer_cn}: {e:#}"),
        }
        pdb::set_trust(conn, peer_cn, None, Some(params.trust))?;
        Ok(())
    })
}

fn handle_push_ca_key(peer_cn: &str, request: Request) -> Result<()> {
    let params: PushCaKeyParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/push-ca-key params")?,
        None => anyhow::bail!("pod/push-ca-key requires params"),
    };
    let t = db::pool::with_pooled_or_open(|conn| pdb::get_trust(conn, peer_cn))?;
    if !pdb::is_mutual_secure(t) {
        anyhow::bail!(
            "pod/push-ca-key refused: peer {peer_cn} is not mutually secure with this host"
        );
    }
    utils::pki::import_mesh_ca_keypair(&pki_dir(), &params.cert_pem, &params.key_pem)?;
    Ok(())
}

fn handle_peer_leaving(peer_cn: &str) -> Result<()> {
    db::pool::with_pooled_or_open(|conn| pdb::mark_peer_departed(conn, peer_cn))?;
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
    let removed = db::pool::with_pooled_or_open(|conn| pdb::forget_peer(conn, &params.peer_id))?;
    crate::peer_info::remove(&params.peer_id);
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
    let in_dev_mode = utils::state::read()
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
    let token = system::update::resolve_github_token();
    match tokio::task::spawn_blocking(move || cmd_dev_enable(&token)).await {
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

/// Whether `tool` is callable at all over `pod/exec` (i.e. not `local_only`)
/// and whether it needs an authenticated caller (role-gated). Everything is
/// REMOTE_OK by default; only `local_only` opt-outs are refused. Pure so the
/// gate logic is unit-testable without a DB or PKI.
fn remote_ok_gate(tool: &str, remote_ok: bool, required_role: &str) -> Result<bool> {
    if !remote_ok {
        anyhow::bail!("pod/exec refused: tool '{tool}' is local_only on this peer");
    }
    // "any" tools are authorized by pod membership alone (the mTLS chain already
    // proved a paired peer). Role-gated tools additionally require a verified
    // caller token bound to a user whose replicated role satisfies the gate.
    Ok(required_role != "any")
}

/// Per-action local-only guard for `pod/exec`. `pod.update` and `pod.delete`
/// are remote-dispatchable as a whole, but two of their actions repair or exit
/// THIS host's own membership (formerly the `local_only` `pod.recover` /
/// `pod.leave` verbs) and must never be driven by a remote peer. The mTLS chain
/// proving a paired peer is not enough — these act on local identity, so they
/// stay local-origin only. Pure over `(tool, args)` so it's unit-testable.
fn reject_remote_local_only_action(tool: &str, args: &Value) -> Result<()> {
    let action = args.get("action").and_then(|v| v.as_str());
    let forbidden = matches!(
        (tool, action),
        ("pod.update", Some("recover")) | ("pod.delete", Some("leave"))
    );
    anyhow::ensure!(
        !forbidden,
        "pod/exec refused: '{tool}' action '{}' is local-only and cannot be invoked by a remote peer",
        action.unwrap_or_default()
    );
    Ok(())
}

/// Zero-trust authorization gate for a role-gated `pod/exec` call.
///
/// The mTLS chain proves which *peer* is on the wire; this proves which *user*
/// the call acts for and that the executing host independently agrees they may.
/// Nothing the caller asserts is trusted — every decision is re-derived here:
///   1. the caller token signature verifies AND covers this exact tool + args,
///      and has not expired (`caller_token::verify`);
///   2. the token's signer fingerprint equals the authenticated peer's *pinned*
///      `pod_peers.pubkey_fp` (a valid sig from an unpinned key is rejected);
///   3. the token nonce has not been seen before (replay guard);
///   4. the effective role comes ONLY from this host's own (replicated) `users`
///      row for `caller_user_id` — never the token's asserted `role`. Unknown
///      user or insufficient role → refuse.
///
/// See feedback_zero_trust_no_blind_trust.md + project_remote_exec_full_fix.md.
fn authorize_role_gated(
    conn: &rusqlite::Connection,
    peer_cn: &str,
    tool: &str,
    args: &serde_json::Value,
    required_role: &str,
    caller_token: Option<&utils::pki::SignedEnvelope>,
    now: i64,
) -> Result<()> {
    let env = caller_token.ok_or_else(|| {
        anyhow::anyhow!(
            "pod/exec refused: tool '{tool}' requires role '{required_role}' but no signed caller \
             token was presented"
        )
    })?;

    let verified = crate::caller_token::verify(env, tool, args, now)
        .context("pod/exec refused: caller token verification failed")?;

    let pinned = db::pod::pinned_pubkey_fp(conn, peer_cn)?.ok_or_else(|| {
        anyhow::anyhow!(
            "pod/exec refused: peer {peer_cn} has no pinned bootstrap key to verify against"
        )
    })?;
    anyhow::ensure!(
        verified.signer_fp == pinned,
        "pod/exec refused: caller token signer fp does not match peer {peer_cn}'s pinned key"
    );

    crate::caller_token::check_replay(&verified.token.nonce, verified.token.expires_at, now)?;

    let user = auth::users::find_by_id(conn, &verified.token.caller_user_id)?.ok_or_else(|| {
        anyhow::anyhow!(
            "pod/exec refused: caller user {} is not in this host's replicated users",
            verified.token.caller_user_id
        )
    })?;
    anyhow::ensure!(
        role_satisfies(&user.role, required_role),
        "pod/exec refused: tool '{tool}' requires role '{required_role}' but user {} has role '{}'",
        user.username,
        user.role
    );
    Ok(())
}

/// Returns true when the user's *replicated* role meets the required role.
/// Order: `any` < `member` < `admin`. Unknown roles compare as `any`.
fn role_rank(role: &str) -> u8 {
    match role {
        "admin" => 2,
        "member" | "user" => 1,
        _ => 0,
    }
}

fn role_satisfies(role: &str, required: &str) -> bool {
    role_rank(role) >= role_rank(required)
}

/// Handle `pod/exec`: dispatch an allowlisted local tool on this peer's behalf.
/// The mesh mTLS chain proves the caller is a paired peer; the REMOTE_OK
/// allowlist guards which tools are reachable, and role-gated tools require a
/// verified caller token bound to a replicated user (zero-trust, no asserted
/// role). Dispatch is direct in-process through the shared registry.
async fn handle_exec(request: Request, peer_cn: &str) -> Result<PodExecResult> {
    let params: PodExecParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/exec params")?,
        None => anyhow::bail!("pod/exec requires params"),
    };

    // Reachability is default-allow: every tool — core, plugin, unit — is
    // callable cross-host unless it opted out via `local_only`. `is_allowed` is
    // the denylist check; `required_role` is the orthogonal auth-tightening axis
    // (unknown/plugin tools default to "any" today — narrowing writes to admin
    // is a tracked follow-up).
    reject_remote_local_only_action(&params.tool, &params.args)?;

    let required_role = dispatch::tool_roles::required_role(&params.tool);
    let needs_auth = remote_ok_gate(
        &params.tool,
        dispatch::remote_ok::is_allowed(&params.tool),
        required_role,
    )?;
    if needs_auth {
        db::pool::with_pooled_or_open(|conn| {
            authorize_role_gated(
                conn,
                peer_cn,
                &params.tool,
                &params.args,
                required_role,
                params.caller_token.as_ref(),
                utils::time::now().unix_seconds(),
            )
        })?;
    }

    let result = crate::dispatcher::dispatch(
        &params.tool,
        params.args.clone(),
        params.correlation_id.clone(),
    )
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
/// Handle `pod/replicate-roots`: return this host's per-entity content roots.
/// No signature needed — roots are opaque hashes; the engine only uses them
/// to short-circuit identical-state bundle fetches. mTLS already authenticated
/// the caller as a paired peer.
fn handle_replicate_roots() -> Result<ReplicateRootsResult> {
    let roots = db::pool::with_pooled_or_open(db::replicate::roots)?;
    Ok(ReplicateRootsResult { roots })
}

fn handle_replicate_export() -> Result<utils::pki::SignedEnvelope> {
    let entities = db::pool::with_pooled_or_open(db::replicate::export_all)?;
    crate::transport::sign_bundle(entities)
}

/// Handle `pod/replicate-push`: caller (the writer) sent us a signed bundle.
/// Verify against the caller's pinned bootstrap fp, then hand off to the db
/// engine to merge. Trust model mirrors `pod/replicate-export` (mTLS proves
/// transport, signature proves bundle origin).
fn handle_replicate_push(peer_cn: &str, request: Request) -> Result<ReplicatePushResult> {
    let envelope: utils::pki::SignedEnvelope = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/replicate-push params")?,
        None => anyhow::bail!("pod/replicate-push requires params"),
    };
    // Resolve the pinned fp under the pool, then release it BEFORE
    // `merge_into_local`, which acquires the pool itself (nesting would
    // deadlock the non-reentrant mutex).
    let pinned_fp = db::pool::with_pooled_or_open(|conn| pdb::pinned_pubkey_fp(conn, peer_cn))?
        .ok_or_else(|| {
            anyhow::anyhow!("pod/replicate-push refused: peer {peer_cn} has no pinned bootstrap fp")
        })?;
    let entities = crate::transport::verify_envelope(&envelope, &pinned_fp)?;
    let merged = db::replicate_engine::merge_into_local(entities)?;
    if merged > 0 {
        tracing::info!("[replicate.push.recv] merged {merged} row(s) from {peer_cn}");
    }
    Ok(ReplicatePushResult { merged })
}

fn handle_push_ca_state(peer_cn: &str, request: Request) -> Result<()> {
    let params: PushCaStateParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/push-ca-state params")?,
        None => anyhow::bail!("pod/push-ca-state requires params"),
    };
    // One pooled connection for the whole trust-check + optional expiry write.
    db::pool::with_pooled_or_open(|conn| {
        let t = pdb::get_trust(conn, peer_cn)?;
        if !pdb::is_mutual_secure(t) {
            anyhow::bail!(
                "pod/push-ca-state refused: peer {peer_cn} is not mutually secure with this host"
            );
        }
        utils::pki::import_mesh_ca_state(
            &pki_dir(),
            &params.current_cert_pem,
            &params.current_key_pem,
            params.previous_cert_pem.as_deref(),
            params.previous_key_pem.as_deref(),
        )?;
        if let Some(exp) = params.previous_expires_at {
            pdb::set_ca_previous_expires_at(conn, Some(exp))?;
        }
        Ok(())
    })
}

/// Sign refreshed CSRs for a peer that doesn't hold the mesh CA key itself
/// (non-secure joiner that needs rotation before its 30-day cert expires).
/// Requires the requesting peer to be a known, non-departed pod member —
/// the mTLS handshake already authenticated the CN, and the departed-peer
/// gate above blocks departed CNs from reaching this method.
fn handle_refresh_cert(peer_cn: &str, request: Request) -> Result<RefreshCertResult> {
    anyhow::ensure!(
        utils::pki::has_mesh_ca_key(&pki_dir()),
        "this host does not have the mesh CA key — cannot refresh peer certs"
    );
    let params: RefreshCertParams = match request.params {
        Some(v) => serde_json::from_value(v).context("parse pod/refresh-cert params")?,
        None => anyhow::bail!("pod/refresh-cert requires params"),
    };

    // Enforce that the joiner identifier matches the authenticated CN. CN is
    // `<machine_id_short>`; the param is named `joiner_hostname` for wire
    // compat but now carries the stable machine_id, not the OS hostname.
    let expected_cn = params.joiner_hostname.clone();
    anyhow::ensure!(
        peer_cn == expected_cn,
        "refresh refused: cert CN ({peer_cn}) does not match joiner_hostname ({expected_cn})"
    );

    let pki_d = pki_dir();
    let (client_cert_pem, ca_cert_pem) = utils::pki::sign_peer_csr(
        &pki_d,
        &params.csr_client_pem,
        &params.joiner_hostname,
        PeerRole::Client,
    )?;
    let (server_cert_pem, _) = utils::pki::sign_peer_csr(
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
        Err(e) => Response::err(id, ErrorObject::internal(&format!("{e:#}"))),
    }
}

/// Read the local host's addressing rows and shape them for the
/// `pod/ping` wire. Returns `None` if the DB is unreachable or empty —
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
            let kind_label = system::system_info::labels::addr_kind_label(&r.kind);
            channels.push(AddressChannel {
                kind: r.kind,
                kind_label,
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
    fn remote_ok_gate_refuses_local_only() {
        // Only a local_only tool (remote_ok=false at the gate) is refused.
        let err = remote_ok_gate("system.dev_enable", false, "any").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("local_only"), "got: {msg}");
        assert!(msg.contains("system.dev_enable"), "got: {msg}");
    }

    #[test]
    fn remote_ok_gate_any_role_needs_no_auth() {
        // Reachable + "any" → pod membership is sufficient, no token needed.
        assert!(!remote_ok_gate("fs.search", true, "any").unwrap());
    }

    #[test]
    fn remote_ok_gate_admin_role_needs_auth() {
        // Reachable + role-gated → a verified caller token is required.
        assert!(remote_ok_gate("system.update.create", true, "admin").unwrap());
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
    fn reject_remote_local_only_action_blocks_pod_update_recover() {
        let args = serde_json::json!({ "action": "recover" });
        let err = reject_remote_local_only_action("pod.update", &args).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("local-only"), "got: {msg}");
        assert!(msg.contains("recover"), "got: {msg}");
    }

    #[test]
    fn reject_remote_local_only_action_blocks_pod_delete_leave() {
        let args = serde_json::json!({ "action": "leave" });
        let err = reject_remote_local_only_action("pod.delete", &args).unwrap_err();
        assert!(err.to_string().contains("leave"), "got: {err}");
    }

    #[test]
    fn reject_remote_local_only_action_allows_other_actions() {
        // A remote-dispatchable action on the same tool is permitted.
        let args = serde_json::json!({ "action": "list" });
        assert!(reject_remote_local_only_action("pod.update", &args).is_ok());
        // A wholly different tool with the "recover" action is not gated —
        // the guard keys on the (tool, action) pair, not the action alone.
        let recover = serde_json::json!({ "action": "recover" });
        assert!(reject_remote_local_only_action("other.tool", &recover).is_ok());
        // Missing action field must not panic and must be allowed.
        let empty = serde_json::json!({});
        assert!(reject_remote_local_only_action("pod.delete", &empty).is_ok());
    }

    #[test]
    fn role_rank_orders_roles() {
        assert_eq!(role_rank("admin"), 2);
        assert_eq!(role_rank("member"), 1);
        assert_eq!(role_rank("user"), 1);
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
    fn notify_trust_without_params_errors() {
        let addr: std::net::SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let err = handle_notify_trust("peer-a", addr, req_no_params(POD_NOTIFY_TRUST_METHOD))
            .unwrap_err();
        assert!(err.to_string().contains("requires params"), "got: {err}");
    }

    #[test]
    fn notify_trust_with_malformed_params_errors() {
        // NotifyTrustParams requires a bool `trust` field; an empty object
        // fails deserialization before any DB access.
        let addr: std::net::SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let err = handle_notify_trust(
            "peer-a",
            addr,
            req_with_params(POD_NOTIFY_TRUST_METHOD, serde_json::json!({})),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("parse pod/notify-trust params"),
            "got: {err}"
        );
    }

    #[test]
    fn push_ca_key_without_params_errors() {
        let err = handle_push_ca_key("peer-a", req_no_params(POD_PUSH_CA_KEY_METHOD)).unwrap_err();
        assert!(err.to_string().contains("requires params"), "got: {err}");
    }

    #[test]
    fn peer_forget_without_params_errors() {
        let err = handle_peer_forget("peer-a", req_no_params(POD_PEER_FORGET_METHOD)).unwrap_err();
        assert!(err.to_string().contains("requires params"), "got: {err}");
    }

    #[test]
    fn peer_forget_with_malformed_params_errors() {
        // ForgetParams requires a `peer_id` string.
        let err = handle_peer_forget(
            "peer-a",
            req_with_params(POD_PEER_FORGET_METHOD, serde_json::json!({ "wrong": 1 })),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("parse pod/peer-forget params"),
            "got: {err}"
        );
    }

    #[test]
    fn push_ca_state_without_params_errors() {
        let err =
            handle_push_ca_state("peer-a", req_no_params(POD_PUSH_CA_STATE_METHOD)).unwrap_err();
        assert!(err.to_string().contains("requires params"), "got: {err}");
    }

    #[test]
    fn replicate_push_without_params_errors() {
        let err =
            handle_replicate_push("peer-a", req_no_params(POD_REPLICATE_PUSH_METHOD)).unwrap_err();
        assert!(err.to_string().contains("requires params"), "got: {err}");
    }

    #[test]
    fn push_ca_key_with_malformed_params_errors() {
        // PushCaKeyParams needs both cert_pem and key_pem; omitting key_pem
        // fails deserialization in the Some(v) parse arm before any DB/PKI access.
        let err = handle_push_ca_key(
            "peer-a",
            req_with_params(
                POD_PUSH_CA_KEY_METHOD,
                serde_json::json!({ "cert_pem": "x" }),
            ),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("parse pod/push-ca-key params"),
            "got: {err}"
        );
    }

    #[test]
    fn push_ca_state_with_malformed_params_errors() {
        // PushCaStateParams requires current_cert_pem + current_key_pem.
        let err = handle_push_ca_state(
            "peer-a",
            req_with_params(POD_PUSH_CA_STATE_METHOD, serde_json::json!({})),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("parse pod/push-ca-state params"),
            "got: {err}"
        );
    }

    #[test]
    fn replicate_push_with_malformed_params_errors() {
        // A SignedEnvelope needs payload/signer_pubkey_b64/signature_b64; an
        // unrelated object fails to deserialize in the Some(v) parse arm.
        let err = handle_replicate_push(
            "peer-a",
            req_with_params(POD_REPLICATE_PUSH_METHOD, serde_json::json!({ "foo": 1 })),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("parse pod/replicate-push params"),
            "got: {err}"
        );
    }

    #[test]
    fn authorize_role_gated_with_bogus_token_fails_verification() {
        // A token that is present but structurally invalid fails at
        // caller_token::verify (base64/signature decode) before the pinned-key
        // lookup, so an empty in-memory DB is sufficient.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let env = utils::pki::SignedEnvelope {
            payload: "not-canonical-json".into(),
            signer_pubkey_b64: "!!!not-base64!!!".into(),
            signature_b64: "!!!not-base64!!!".into(),
        };
        let err = authorize_role_gated(
            &conn,
            "peer-a",
            "system.update.create",
            &serde_json::json!({}),
            "admin",
            Some(&env),
            0,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("caller token verification failed"),
            "got: {err}"
        );
    }

    #[test]
    fn role_satisfies_admin_meets_all_and_unknown_meets_any_only() {
        // admin satisfies member and user-tier requirements.
        assert!(role_satisfies("admin", "member"));
        assert!(role_satisfies("admin", "user"));
        // an unknown role ranks as "any" — meets only an "any" requirement.
        assert!(role_satisfies("ghost", "any"));
        assert!(!role_satisfies("ghost", "member"));
        // equal unknown-vs-unknown: rank 0 >= rank 0 holds.
        assert!(role_satisfies("ghost", "phantom"));
    }

    #[test]
    fn remote_ok_gate_local_only_takes_priority_over_role() {
        // Even a role-gated tool is refused outright when it is local_only —
        // the reachability check runs before the auth-axis decision.
        let err = remote_ok_gate("pod.internal", false, "admin").unwrap_err();
        assert!(err.to_string().contains("local_only"), "got: {err}");
    }

    #[tokio::test]
    async fn dev_sync_skipped_when_not_in_dev_mode() {
        // A production peer (not in Dev/Parked mode) short-circuits to a
        // "skipped" status with an explanatory detail and no commit count —
        // dev_sync is a no-op, not an error, on production-only hosts.
        let r = handle_dev_sync().await.unwrap();
        assert_eq!(r.status, "skipped");
        assert_eq!(r.detail.as_deref(), Some("peer not in dev mode"));
        assert!(r.commits_pulled.is_none());
    }

    #[tokio::test]
    async fn exec_without_params_errors() {
        let err = handle_exec(req_no_params(POD_EXEC_METHOD), "peer-a")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("pod/exec requires params"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn exec_with_malformed_params_errors() {
        // PodExecParams requires a `tool` string; an object without it fails
        // deserialization in the Some(v) parse arm before any DB/PKI access.
        let err = handle_exec(
            req_with_params(POD_EXEC_METHOD, serde_json::json!({ "args": {} })),
            "peer-a",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("parse pod/exec params"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn exec_rejects_remote_local_only_action() {
        // pod.update/recover is a local-only action; handle_exec must reject it
        // at the per-action guard, which runs before any DB or dispatch access.
        let params = serde_json::json!({
            "tool": "pod.update",
            "args": { "action": "recover" }
        });
        let err = handle_exec(req_with_params(POD_EXEC_METHOD, params), "peer-a")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("local-only"), "got: {msg}");
        assert!(msg.contains("recover"), "got: {msg}");
    }

    #[test]
    fn refresh_cert_cn_mismatch_or_no_ca_key_errors() {
        // Without the mesh CA key present, handle_refresh_cert fails fast on the
        // CA-key ensure. If a key does happen to exist in this host's pki_dir,
        // the params still fail to parse (no params) — either way it errors and
        // never signs a cert. This exercises the early guard path.
        let err =
            handle_refresh_cert("peer-a", req_no_params(POD_REFRESH_CERT_METHOD)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("mesh CA key") || msg.contains("requires params"),
            "got: {msg}"
        );
    }

    #[test]
    fn authorize_role_gated_without_token_refuses() {
        // The no-token branch returns before touching the connection, so an
        // empty in-memory DB is sufficient to exercise it.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let err = authorize_role_gated(
            &conn,
            "peer-a",
            "system.update.create",
            &serde_json::json!({}),
            "admin",
            None,
            0,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no signed caller"), "got: {msg}");
        assert!(msg.contains("system.update.create"), "got: {msg}");
    }

    // Drive `body` on a current-thread runtime with an ephemeral DB pointed at a
    // temp file. `db::with_db_path` installs a task-local override that
    // `with_pooled_or_open` / `open_default` honor on the single-threaded
    // executor, so both the sync handlers under test and our seed/assert queries
    // share one throwaway database.
    fn with_db<T>(body: impl std::future::Future<Output = T>) -> T {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("listener-test.db");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(db::with_db_path(db_path, body))
    }

    fn addr() -> std::net::SocketAddr {
        "10.1.2.3:9000".parse().unwrap()
    }

    #[test]
    fn notify_trust_success_materializes_peer_and_sets_trust() {
        with_db(async {
            // Fresh DB: no peer row for this CN. A successful notify-trust must
            // self-heal by inserting a stub peer keyed on the (uuidv7) CN, then
            // record trust — so the peer becomes visible in the roster after.
            let cn = utils::id::new();
            handle_notify_trust(
                &cn,
                addr(),
                req_with_params(
                    POD_NOTIFY_TRUST_METHOD,
                    serde_json::json!({ "trust": true }),
                ),
            )
            .unwrap();
            let conn = db::open_default().unwrap();
            let peers = pdb::list_peers(&conn).unwrap();
            assert!(
                peers.iter().any(|p| p.peer_id == cn),
                "stub peer not materialized: {:?}",
                peers.iter().map(|p| &p.peer_id).collect::<Vec<_>>()
            );
        });
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
                "asker",
                req_with_params(POD_PEER_FORGET_METHOD, serde_json::json!({ "peer_id": cn })),
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
                "asker",
                req_with_params(
                    POD_PEER_FORGET_METHOD,
                    serde_json::json!({ "peer_id": "never-existed" }),
                ),
            )
            .unwrap();
            assert_eq!(removed, 0);
        });
    }

    #[test]
    fn push_ca_key_refused_for_non_mutual_secure_peer() {
        with_db(async {
            // Valid params but the peer has no mutual-secure trust → the handler
            // must refuse before importing any keypair.
            let err = handle_push_ca_key(
                "peer-untrusted",
                req_with_params(
                    POD_PUSH_CA_KEY_METHOD,
                    serde_json::json!({ "cert_pem": "c", "key_pem": "k" }),
                ),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("not mutually secure"),
                "got: {err}"
            );
        });
    }

    #[test]
    fn push_ca_state_refused_for_non_mutual_secure_peer() {
        with_db(async {
            let err = handle_push_ca_state(
                "peer-untrusted",
                req_with_params(
                    POD_PUSH_CA_STATE_METHOD,
                    serde_json::json!({ "current_cert_pem": "c", "current_key_pem": "k" }),
                ),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("not mutually secure"),
                "got: {err}"
            );
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
            };
            let err = handle_replicate_push(
                "peer-nopin",
                req_with_params(
                    POD_REPLICATE_PUSH_METHOD,
                    serde_json::to_value(&env).unwrap(),
                ),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("no pinned bootstrap fp"),
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
        system::host_identity::init(iddir.path()).unwrap();
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
            let resp = dispatch(req_no_params("pod/bogus-method"), "peer-a", addr()).await;
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

            // A departed peer hitting any method other than pod/peer-leaving is
            // rejected at the gate with a re-pair message.
            let resp = dispatch(req_no_params(POD_HAS_CA_KEY_METHOD), &cn, addr()).await;
            let text = serde_json::to_string(&resp).unwrap();
            assert!(text.contains("has departed this pod"), "got: {text}");
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
            let resp = dispatch(req_no_params(POD_HAS_CA_KEY_METHOD), "peer-ok", addr()).await;
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

            let resp = dispatch(req_no_params(POD_PEER_REMOVED_METHOD), &cn, addr()).await;
            let text = serde_json::to_string(&resp).unwrap();
            assert!(!text.contains("\"error\""), "unexpected error: {text}");
            // pod/peer-removed must NOT mark the caller departed (that's the
            // 2026-05-28 regression this method guards against).
            let conn = db::open_default().unwrap();
            assert!(!pdb::is_peer_departed(&conn, &cn).unwrap());
        })
        .await;
    }

    #[test]
    fn refresh_cert_cn_mismatch_rejected_with_ca_key_present() {
        // With the mesh CA key present but the joiner_hostname param not matching
        // the authenticated CN, the handler must refuse on the CN check rather
        // than sign anything. Uses the crate HOME lock so pki_dir points at temp.
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("HOME").ok();
        // SAFETY: HOME restored below; serialized behind HOME_ENV_LOCK.
        unsafe { std::env::set_var("HOME", dir.path()) };
        let pki = pki_dir();
        utils::pki::init_mesh_ca(&pki, "24647a14a251e863cdf8dcee692f2915").unwrap();
        let params = serde_json::json!({
            "joiner_hostname": "someone-else",
            "csr_client_pem": "x",
            "csr_server_pem": "y",
        });
        let err = handle_refresh_cert("peer-a", req_with_params(POD_REFRESH_CERT_METHOD, params))
            .unwrap_err();
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert!(
            err.to_string().contains("does not match joiner_hostname"),
            "got: {err}"
        );
    }
}
