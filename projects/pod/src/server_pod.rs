use crate::{
    CertInfo, PodAcceptOutput, PodCertStatusOutput, PodDiscoveryRowDto, PodExecDispatch,
    PodLeaveOutput, PodOfferOutput, PodPeerDto, PodPendingOfferDto, PodPingOutput, PodTrustOutput,
    Route, Routes,
};
use anyhow::{Context, Result};
use db::ports::mesh_port;
use std::time::Instant;
use system::update_state::read_channel_marker;

use crate::cli::dial_bootstrap_pub;
use crate::pki_dir;
use crate::scheduler::{OFFER_TTL_SECS, mint_pairing_code, push_offer};
use db::pod as pdb;

pub async fn list_enriched() -> Result<Vec<PodPeerDto>> {
    list_enriched_impl().await
}

/// Refuse to install a mesh leaf cert whose Subject CN carries the legacy
/// `peer.<id>` prefix retired in rc.16. A lagging inviter that hasn't been
/// upgraded would otherwise re-introduce the duplicate-pod_peers flip — fail
/// loud per feedback_no_id_prefixes so the operator notices and upgrades the
/// inviting peer rather than silently re-pairing through a stale CN.
fn reject_legacy_peer_cn(cert_pem: &str, role: &str) -> Result<()> {
    let summary = utils::pki::cert_summary(cert_pem)
        .with_context(|| format!("parse received {role} cert"))?;
    anyhow::ensure!(
        !summary.cn.starts_with("peer."),
        "stale peer issued legacy `peer.<id>` {role} CN ({}); refusing to install — upgrade the inviting peer to rc.16+ and retry",
        summary.cn
    );
    Ok(())
}

pub async fn accept(code: &str) -> Result<PodAcceptOutput> {
    let conn = db::open_default()?;
    let offer = pdb::find_pending_offer_by_code(&conn, code)?
        .context("no pending offer matches that code (mistyped, expired, or already used?)")?;
    drop(conn);

    let pki_d = pki_dir();
    std::fs::create_dir_all(utils::pki::mesh_dir(&pki_d))?;
    let ca_pem = offer
        .mesh_ca_cert_pem
        .as_deref()
        .context("offer has no mesh CA cert")?;
    std::fs::write(utils::pki::mesh_ca_cert_path(&pki_d), ca_pem.as_bytes())?;

    let peer_cn = system::host_identity::machine_id().to_string();
    let display_name = system::host_identity::display_hostname().to_string();
    let (csr_client_pem, client_key_pem) =
        utils::pki::build_peer_csr(&peer_cn, utils::pki::PeerRole::Client)?;
    let (csr_server_pem, server_key_pem) =
        utils::pki::build_peer_csr(&peer_cn, utils::pki::PeerRole::Server)?;

    let signing = utils::pki::load_or_init_bootstrap_key(&pki_d)?;
    #[derive(serde::Serialize)]
    struct ConfirmBody<'a> {
        code: &'a str,
        joiner_hostname: &'a str,
        csr_client_pem: &'a str,
        csr_server_pem: &'a str,
        joiner_display_name: &'a str,
    }
    let body = ConfirmBody {
        code,
        joiner_hostname: &peer_cn,
        csr_client_pem: &csr_client_pem,
        csr_server_pem: &csr_server_pem,
        joiner_display_name: &display_name,
    };
    let env = utils::pki::sign_envelope(&signing, &body)?;

    // Try each candidate address the inviter advertised (fallback: the single
    // stored addr), pinned to the inviter's bootstrap fp. The pin is the
    // security anchor — a candidate pointing at the wrong host fails the
    // handshake and we move on — so re-pair works as long as ANY advertised
    // address reaches the real inviter, even when the offer-push TLS source IP
    // was a tunnel address rather than the inviter's bootstrap listener.
    let params = serde_json::to_value(&env)?;
    let candidates: Vec<String> = if offer.candidate_addrs.is_empty() {
        vec![offer.peer_addr.clone()]
    } else {
        offer.candidate_addrs.clone()
    };
    let mut resp_value = None;
    let mut dialed_addr = offer.peer_addr.clone();
    let mut last_err: Option<anyhow::Error> = None;
    for addr in &candidates {
        match dial_bootstrap_pub(
            addr,
            offer.peer_port,
            &offer.peer_pubkey_fp,
            "pod/join-confirm",
            params.clone(),
        )
        .await
        {
            Ok(v) => {
                dialed_addr = addr.clone();
                resp_value = Some(v);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let resp_value = resp_value.ok_or_else(|| {
        last_err
            .unwrap_or_else(|| anyhow::anyhow!("no candidate addresses to dial"))
            .context(format!(
                "pod/join-confirm over bootstrap channel failed (tried {} address(es): {})",
                candidates.len(),
                candidates.join(", ")
            ))
    })?;

    #[derive(serde::Deserialize)]
    struct Resp {
        client_cert_pem: String,
        server_cert_pem: String,
        ca_cert_pem: String,
        inviter_peer_id: String,
        pod_id: String,
    }
    let r: Resp = serde_json::from_value(resp_value)?;

    // Defensive: a lagging peer (pre-rc.16) may still sign certs with the
    // old `peer.<id>` CN convention. Installing one re-introduces the
    // duplicate-pod_peers flip that rc.16 fixed. Fail loud per
    // feedback_no_id_prefixes — operator re-pairs against a current peer
    // rather than silently swallowing a legacy CN.
    reject_legacy_peer_cn(&r.server_cert_pem, "server")?;
    reject_legacy_peer_cn(&r.client_cert_pem, "client")?;

    let server_dir = utils::pki::mesh_dir(&pki_d).join("server");
    let client_dir = utils::pki::mesh_dir(&pki_d).join("client");
    std::fs::create_dir_all(&server_dir)?;
    std::fs::create_dir_all(&client_dir)?;
    std::fs::write(
        utils::pki::mesh_server_cert_path(&pki_d),
        &r.server_cert_pem,
    )?;
    std::fs::write(utils::pki::mesh_server_key_path(&pki_d), &server_key_pem)?;
    std::fs::write(
        utils::pki::mesh_client_cert_path(&pki_d),
        &r.client_cert_pem,
    )?;
    std::fs::write(utils::pki::mesh_client_key_path(&pki_d), &client_key_pem)?;

    let conn = db::open_default()?;
    pdb::set_self_secure(&conn, false)?;
    pdb::set_pod_id(&conn, &r.pod_id)?;
    pdb::upsert_peer(
        &conn,
        &r.inviter_peer_id,
        &offer.peer_hostname,
        &dialed_addr,
        offer.peer_port,
        Some(&offer.peer_pubkey_fp),
        &r.ca_cert_pem,
    )?;
    pdb::delete_pending_offer(&conn, &offer.offer_id)?;

    Ok(PodAcceptOutput {
        pod_id: r.pod_id,
        inviter_peer_id: r.inviter_peer_id,
        inviter_hostname: offer.peer_hostname,
        inviter_addr: dialed_addr,
        inviter_port: offer.peer_port,
        self_secure: false,
    })
}

/// Ordered dial targets for a peer: the `addresses[]` channels (via the
/// dialer) then the legacy `peer_addr` fallback. A stale legacy `peer_addr` —
/// e.g. left behind by a wired→wireless interface change — no longer
/// dead-ends a notify as long as the peer advertises any live address.
///
/// Synchronous by design: the `rusqlite::Connection` is not `Sync`, so callers
/// must build targets *before* awaiting the dial (holding `&conn` across an
/// `.await` would make the enclosing future non-`Send`).
fn dial_targets(conn: &rusqlite::Connection, peer: &pdb::PeerRow) -> Vec<String> {
    crate::dialer::dial_targets_for_peer(conn, &peer.peer_id, &peer.peer_addr)
        .unwrap_or_else(|_| vec![peer.peer_addr.clone()])
}

/// Send a one-shot pod method to the first reachable target, retrying the rest
/// on connect failure. Holds no DB handle, so it's safe to `.await`.
// JSON-RPC params/result are opaque at the wire boundary — same rationale as
// the file-level allow in listener.rs / bootstrap.rs.
#[allow(clippy::disallowed_types)]
async fn notify_targets(
    targets: &[String],
    port: u16,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    crate::dialer::try_targets(targets, |t| {
        let method = method.to_string();
        let params = params.clone();
        async move { crate::cli::call_pod_method_pub(&t, port, &method, params).await }
    })
    .await
}

pub async fn trust(peer_id: &str, on: bool) -> Result<PodTrustOutput> {
    let conn = db::open_default()?;
    let peer = pdb::list_peers(&conn)?
        .into_iter()
        .find(|p| p.peer_id == peer_id)
        .with_context(|| format!("no such peer: {peer_id}"))?;
    let new = pdb::set_trust(&conn, peer_id, Some(on), None)?;
    let targets = dial_targets(&conn, &peer);
    drop(conn);

    let notify_result = match notify_targets(
        &targets,
        peer.peer_port,
        "pod/notify-trust",
        serde_json::json!({ "trust": on }),
    )
    .await
    {
        Ok(_) => "ok".to_string(),
        Err(e) => format!("warn: {e:#}"),
    };

    if pdb::is_mutual_secure(new)
        && let Err(e) = crate::cli::replicate_ca_key_if_needed_pub(&peer).await
    {
        tracing::warn!("CA-key replication: {e}");
    }

    Ok(PodTrustOutput {
        peer_id: peer_id.to_string(),
        local_secure: new.local_secure,
        peer_secure: new.peer_secure,
        mutual: new.local_secure && new.peer_secure,
        notify_result,
    })
}

pub async fn push_trust(
    peer_id: &str,
    on: bool,
    caller: Option<contract::CallerIdentity>,
) -> Result<PodTrustOutput> {
    // Our own peer_id as the remote knows us.
    let own_id = system::host_identity::machine_id().to_string();
    // Execute pod.trust on the remote host, making THEM set their
    // local_secure for us. `push: false` prevents recursion. The caller is the
    // local admin who invoked `pod.trust` — the recipient authorizes the
    // mutating handshake against that admin's replicated users row (zero-trust:
    // no synthetic/asserted identity).
    #[allow(clippy::disallowed_types)] // exec is the wire-level dispatch boundary
    let dispatch = exec(
        peer_id,
        "pod.update",
        serde_json::json!({ "action": "trust", "peer_id": own_id, "on": on, "push": false }),
        caller,
        None,
    )
    .await?;
    let remote: PodTrustOutput = serde_json::from_value(dispatch.result)?;
    // remote.local_secure = they now trust us (= our peer_secure for this peer).
    // Our own local_secure for them is unchanged — read it from DB.
    let conn = db::open_default()?;
    let our_local_secure = pdb::list_peers(&conn)?
        .into_iter()
        .find(|p| p.peer_id == peer_id)
        .map(|p| p.local_secure)
        .unwrap_or(false);
    drop(conn);
    let peer_secure = remote.local_secure;
    Ok(PodTrustOutput {
        peer_id: peer_id.to_string(),
        local_secure: our_local_secure,
        peer_secure,
        mutual: our_local_secure && peer_secure,
        notify_result: remote.notify_result,
    })
}

pub async fn ping(peer_id: &str) -> PodPingOutput {
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => {
            return PodPingOutput {
                ok: false,
                latency_ms: 0,
                error: Some(e.to_string()),
                peer_id: None,
                hostname: None,
                version: None,
            };
        }
    };
    let peer = match pdb::list_peers(&conn)
        .ok()
        .and_then(|ps| ps.into_iter().find(|p| p.peer_id == peer_id))
    {
        Some(p) => p,
        None => {
            return PodPingOutput {
                ok: false,
                latency_ms: 0,
                error: Some(format!("no such peer: {peer_id}")),
                peer_id: None,
                hostname: None,
                version: None,
            };
        }
    };

    let targets = crate::dialer::dial_targets_for_peer(&conn, peer_id, &peer.peer_addr)
        .unwrap_or_else(|_| vec![peer.peer_addr.clone()]);
    let start = Instant::now();
    match crate::dialer::try_targets_tracked(Some(peer_id), &targets, |t| async move {
        crate::ping(&t).await
    })
    .await
    {
        Ok(r) => PodPingOutput {
            ok: true,
            latency_ms: start.elapsed().as_millis() as u32,
            error: None,
            peer_id: Some(r.peer_id),
            hostname: Some(r.hostname),
            version: Some(r.version),
        },
        Err(e) => PodPingOutput {
            ok: false,
            latency_ms: start.elapsed().as_millis() as u32,
            error: Some(e.to_string()),
            peer_id: None,
            hostname: None,
            version: None,
        },
    }
}

pub fn discover() -> Result<Vec<PodDiscoveryRowDto>> {
    let conn = db::open_default()?;
    let rows = pdb::list_discovery(&conn)?;
    Ok(rows
        .into_iter()
        .map(|r| PodDiscoveryRowDto {
            pubkey_fp: r.pubkey_fp,
            peer_id: r.peer_id,
            hostname: r.hostname,
            addr: r.addr,
            port: r.port,
            discovery_state: r.state,
            can_invite: r.can_invite,
            first_seen_at: r.first_seen_at,
            last_seen_at: r.last_seen_at,
        })
        .collect())
}

pub fn pending() -> Result<Vec<PodPendingOfferDto>> {
    let conn = db::open_default()?;
    let rows = pdb::list_pending_offers(&conn, "in")?;
    let now = utils::time::now_secs_since_epoch();
    Ok(rows
        .into_iter()
        .map(|r| PodPendingOfferDto {
            offer_id: r.offer_id,
            direction: r.direction,
            peer_pubkey_fp: r.peer_pubkey_fp,
            peer_hostname: r.peer_hostname,
            peer_addr: r.peer_addr,
            peer_port: r.peer_port,
            inviter_peer_id: r.inviter_peer_id,
            pod_id: r.pod_id,
            expires_at: r.expires_at,
            ttl_secs: (r.expires_at - now).max(0),
            created_at: r.created_at,
        })
        .collect())
}

pub async fn offer(addr: &str, port: Option<u16>) -> Result<PodOfferOutput> {
    let port = port.unwrap_or_else(mesh_port);

    // Look up the joiner in the discovery table by addr.
    let conn = db::open_default()?;
    let discovery = pdb::list_discovery(&conn)?;
    let d = discovery
        .into_iter()
        .find(|r| r.addr == addr || format!("{}:{}", r.addr, r.port) == addr)
        .with_context(|| {
            format!("{addr} not found in pod_discovery — is the joiner visible via mDNS?")
        })?;

    // User-driven invites are idempotent: if an outbound offer to this
    // address is already pending, drop it and mint a fresh one. The
    // stale-offer guard belongs to the auto-offer scheduler
    // ([[scheduler.rs:83]]), not to operator-triggered +Add clicks —
    // the operator's intent is clear: send a NEW invite now.
    let replaced = pdb::delete_outbound_offers_by_addr(&conn, &d.addr)?;
    if replaced > 0 {
        tracing::info!(addr = %d.addr, replaced, "replaced stale outbound offer(s)");
    }

    let pod_id = pdb::get_pod_id(&conn)?.unwrap_or_else(|| "default".to_string());
    let code = mint_pairing_code();
    let code_hash = pdb::hash_code(&code);
    let offer_id = utils::id::new();
    let now = utils::time::now_secs_since_epoch();
    pdb::insert_pending_offer(
        &conn,
        &offer_id,
        "out",
        &d.pubkey_fp,
        &d.hostname,
        &d.addr,
        port,
        &code_hash,
        None,
        None,
        None,
        OFFER_TTL_SECS,
        None,
        &[], // outbound offer: the joiner dials us, not the reverse
    )?;
    drop(conn);

    push_offer(&d.hostname, &d.addr, port, &d.pubkey_fp, &code, &pod_id).await?;

    Ok(PodOfferOutput {
        code,
        joiner_hostname: d.hostname,
        joiner_addr: d.addr,
        joiner_port: port,
        joiner_pubkey_fp: d.pubkey_fp,
        offer_id,
        expires_at: now + OFFER_TTL_SECS,
    })
}

/// Cancel every outbound pending offer pinned to `addr`. Used by the
/// `pod.cancel_offer` tool when an operator wants to clear a stuck
/// pairing handshake without waiting for the TTL. Returns the number of
/// rows removed (0 if none matched).
pub fn cancel_offer(addr: &str) -> Result<u32> {
    let conn = db::open_default()?;
    let n = pdb::delete_outbound_offers_by_addr(&conn, addr)?;
    Ok(n)
}

/// Joiner-initiated pairing: dial the inviter directly (no mDNS), request an
/// offer, and auto-accept — returning the established membership. Delegates to
/// [`crate::cli::pod_join_core`], the single implementation shared with the CLI
/// so every surface (CLI / MCP / REST) pairs identically. `port` overrides any
/// port embedded in `inviter_addr`.
pub async fn join(inviter_addr: &str, port: Option<u16>) -> Result<PodAcceptOutput> {
    crate::cli::pod_join_core(inviter_addr, port).await
}

/// Kick a peer: drop its rows locally and send a one-way "you've been removed"
/// notice. The recipient logs the removal but does NOT mark the caller as
/// departed (that's what `pod/peer-leaving` is for — the voluntary-exit path
/// from `leave_self`). Reusing `pod/peer-leaving` here was the 2026-05-28
/// bug that departed mint on alpha/echo.
pub async fn leave_peer(peer_id: &str) -> Result<PodLeaveOutput> {
    let conn = db::open_default()?;
    let peer = pdb::list_peers(&conn)?
        .into_iter()
        .find(|p| p.peer_id == peer_id)
        .with_context(|| format!("no such peer: {peer_id}"))?;
    let targets = dial_targets(&conn, &peer);
    drop(conn);

    let notify_result = match notify_targets(
        &targets,
        peer.peer_port,
        "pod/peer-removed",
        serde_json::json!({}),
    )
    .await
    {
        Ok(_) => "notified".to_string(),
        Err(e) => format!("warn: {e:#}"),
    };

    let conn = db::open_default()?;
    conn.execute("DELETE FROM pod_peers WHERE peer_id = ?", [peer_id])?;
    conn.execute("DELETE FROM pod_trust WHERE peer_id = ?", [peer_id])?;
    // Durable, replicated forget-tombstone so a straggler that missed the
    // `pod/peer-removed` notice cannot re-gossip the kicked peer back into the
    // mesh on the next roster tick (issue #232).
    if let Err(e) = pdb::write_forget_tombstone(&conn, peer_id) {
        tracing::warn!("[pod] kick tombstone for {peer_id} failed: {e:#}");
    }

    Ok(PodLeaveOutput {
        peer_id: peer_id.to_string(),
        notify_result,
        rows_removed: 2,
    })
}

#[allow(clippy::disallowed_types)] // mirrors PodService::exec — peer-mesh wire payload
pub async fn exec(
    peer: &str,
    tool: &str,
    args: serde_json::Value,
    caller: Option<contract::CallerIdentity>,
    correlation_id: Option<String>,
) -> Result<PodExecDispatch> {
    // "local" / "localhost" → loopback round-trip via the same /api/v1
    // path peers use. Lets the same code path validate the allowlist
    // without leaving the host.
    let is_local = matches!(peer.to_ascii_lowercase().as_str(), "local" | "localhost");

    // Build the ordered dial-target list up front (single "127.0.0.1" for the
    // loopback case, else the peer's full multi-channel address set) so we can
    // try each in turn — a peer reachable on tailscale_v4 but not lan_v4 (e.g.
    // behind an exit-node / subnet-route quirk) still connects instead of
    // failing on the single legacy `peer_addr`.
    let (targets, peer_id): (Vec<String>, Option<String>) = if is_local {
        (vec!["127.0.0.1".to_string()], None)
    } else {
        let conn = db::open_default()?;
        let peers = pdb::list_peers(&conn)?;
        let row = resolve_peer_row(&peers, peer)?;
        let peer_id = row.peer_id.clone();
        let targets = dial_targets(&conn, row);
        drop(conn);
        (targets, Some(peer_id))
    };

    // Track per-address health against the resolved peer_id so the winning
    // address sorts first next time and a dead one sinks (loopback is untracked).
    let r = crate::dialer::try_targets_tracked(peer_id.as_deref(), &targets, |addr| {
        let tool = tool.to_string();
        let args = args.clone();
        let caller = caller.clone();
        let correlation_id = correlation_id.clone();
        async move { crate::exec_as(&addr, &tool, args, caller, correlation_id).await }
    })
    .await?;
    Ok(PodExecDispatch {
        peer: peer.to_string(),
        tool: r.tool,
        result: r.result,
    })
}

/// Voluntary pod exit: notify every paired peer we're leaving (best-effort
/// per peer), then drop all `pod_peers` + `pod_trust` rows. Returns a
/// per-peer notify result so the operator can see who heard from us. PKI
/// material is left in place — call `system bootstrap` to fully reset.
/// Clear a stale `departed_at` flag for a peer on this host. Used to recover
/// from the 2026-05-28 kick/peer-leaving bug (and any future false-depart).
/// No network call — purely local row repair.
pub fn recover(peer_id: &str) -> Result<crate::PodRecoverOutput> {
    let conn = db::open_default()?;
    let cleared = pdb::unmark_peer_departed(&conn, peer_id)?;
    Ok(crate::PodRecoverOutput {
        peer_id: peer_id.to_string(),
        cleared,
    })
}

/// Pod-wide forget: hard-delete a stale/orphan peer_id locally AND tell every
/// live member to drop it too. Unlike `kick` (targets one live peer) or
/// `recover` (purely local), forget fans a one-way `pod/peer-forget` notice to
/// each reachable member so an orphaned identity (machine_id churn,
/// decommissioned host) disappears from the whole mesh, not just here.
pub async fn forget(peer_id: &str) -> Result<crate::PodForgetOutput> {
    let conn = db::open_default()?;
    let members = pdb::list_peers(&conn)?;
    // Build dial targets for every recipient while the DB handle is live, then
    // drop it before awaiting (Connection is not Sync — can't cross `.await`).
    let plans: Vec<(String, u16, Vec<String>)> = members
        .iter()
        // Skip the target itself and any already-departed members.
        .filter(|m| m.peer_id != peer_id && m.departed_at.is_none())
        .map(|m| (m.peer_id.clone(), m.peer_port, dial_targets(&conn, m)))
        .collect();
    drop(conn);

    let mut notified = Vec::new();
    for (member_id, port, targets) in &plans {
        let result = match notify_targets(
            targets,
            *port,
            "pod/peer-forget",
            serde_json::json!({ "peer_id": peer_id }),
        )
        .await
        {
            Ok(_) => "notified".to_string(),
            Err(e) => format!("warn: {e:#}"),
        };
        notified.push(crate::PodForgetNotice {
            peer_id: member_id.clone(),
            result,
        });
    }

    let conn = db::open_default()?;
    let rows_removed = pdb::forget_peer(&conn, peer_id)?;
    crate::peer_info::remove(peer_id);

    Ok(crate::PodForgetOutput {
        peer_id: peer_id.to_string(),
        rows_removed,
        notified,
    })
}

/// One-shot boot pass: fan a best-effort mesh-wide `pod forget` for every
/// identity THIS host has shed (a non-UUIDv7 → UUIDv7 migration, or a
/// wipe/nuke). Without it a re-minted host's old id lingers as an orphan row
/// in every peer's roster — the identity-churn residue that scrambled the
/// roster. Idempotent: each id is recorded on success so it's never
/// re-forgotten; a failed fan-out (peer offline at boot) is retried next boot.
/// The durable-tombstone hardening (suppress resurrection from an offline
/// straggler) is tracked separately.
pub async fn retire_superseded_identities() {
    let old_ids = system::host_identity::superseded_machine_ids();
    if old_ids.is_empty() {
        return;
    }
    tracing::info!(
        "[pod] retiring {} superseded identity(ies) shed by this host",
        old_ids.len()
    );
    for old in old_ids {
        match forget(&old).await {
            Ok(out) => {
                tracing::info!(
                    "[pod] retired superseded identity {old}: {} local row(s) removed, {} peer(s) notified",
                    out.rows_removed,
                    out.notified.len()
                );
                system::host_identity::mark_identity_retired(&old);
            }
            Err(e) => tracing::warn!(
                "[pod] retire of superseded identity {old} failed (retry next boot): {e:#}"
            ),
        }
    }
}

pub async fn leave_self() -> Result<crate::PodLeaveSelfOutput> {
    let conn = db::open_default()?;
    let peers = pdb::list_peers(&conn)?;
    drop(conn);
    let mut results = Vec::with_capacity(peers.len());
    for p in &peers {
        let r = leave_peer(&p.peer_id).await;
        results.push(crate::PodLeaveSelfResult {
            peer_id: p.peer_id.clone(),
            notify_result: match &r {
                Ok(o) => o.notify_result.clone(),
                Err(e) => format!("error: {e:#}"),
            },
        });
    }
    Ok(crate::PodLeaveSelfOutput {
        rows_removed: results.len() as u32,
        peers: results,
    })
}

/// Full pod-detail status: every mesh cert's rotation state plus the current
/// `self_secure` (Tier-2 secrets-storage) flag, in one read. Single entry
/// point for `system.pod.detail` — no separate cert/self_secure round-trip.
pub fn status() -> Result<PodCertStatusOutput> {
    let mut out = cert_status()?;
    out.self_secure = get_self_secure().unwrap_or(false);
    Ok(out)
}

pub fn cert_status() -> Result<PodCertStatusOutput> {
    let pki_d = pki_dir();
    let founder = utils::pki::has_mesh_ca_key(&pki_d);
    let member = utils::pki::mesh_ca_cert_path(&pki_d).exists();

    let parse = |path: std::path::PathBuf| -> Option<CertInfo> {
        let pem = std::fs::read_to_string(&path).ok()?;
        let days = utils::pki::cert_days_remaining(&pem).ok()?;
        Some(CertInfo {
            cn: String::new(),
            fingerprint: String::new(),
            issued_at: 0,
            expires_at: 0,
            days_remaining: days,
        })
    };

    Ok(PodCertStatusOutput {
        founder,
        member,
        version: option_env!("ORCA_VERSION")
            .unwrap_or(env!("CARGO_PKG_VERSION"))
            .to_string(),
        self_secure: false,
        mesh_ca: parse(utils::pki::mesh_ca_cert_path(&pki_d)),
        leaf_server: parse(utils::pki::mesh_server_cert_path(&pki_d)),
        leaf_client: parse(utils::pki::mesh_client_cert_path(&pki_d)),
        ca_previous: parse(utils::pki::mesh_ca_previous_cert_path(&pki_d)),
        bootstrap: parse(utils::pki::bootstrap_cert_path(&pki_d)),
    })
}

pub fn get_self_secure() -> Result<bool> {
    let conn = db::open_default()?;
    db::pod::get_self_secure(&conn)
}

pub async fn set_self_secure(on: bool) -> Result<bool> {
    let conn = db::open_default()?;
    pdb::set_self_secure(&conn, on)?;
    Ok(on)
}

/// Build the local-host row for `pod.list`. Uses the in-process lifecycle
/// service so the synthetic local entry stays in lock-step with what every
/// remote peer would self-report via `system.runtime-spec`.
async fn local_peer_row() -> PodPeerDto {
    let frontend = "embedded";
    let mode = utils::state::read().ok().flatten().map(|s| match s.mode {
        utils::state::DaemonMode::Daemon => "daemon".to_string(),
        utils::state::DaemonMode::Parked => "parked".to_string(),
        utils::state::DaemonMode::Dev => "dev".to_string(),
    });
    let channel = read_channel_marker().map(|c| c.as_marker().to_string());
    // Pin removed: hosts always track channel-latest. Always None.
    let pinned_to: Option<String> = None;
    // The local row must surface this host's REAL network identity, never
    // loopback: locality is a flag (`local: true`), it must not hide the
    // address (the same masking bug as hiding the id/version). Pull our own
    // autodetected addressing — the same rows remote peers publish — and prefer
    // the LAN IPv4 as the primary `addr`.
    let routes: Routes = db::open_default()
        .ok()
        .and_then(|conn| db::host_addressing::list_host_addressing(&conn).ok())
        .unwrap_or_default()
        .iter()
        .map(Route::from)
        .map(crate::labeled)
        .collect();
    let addr = routes
        .iter()
        .find(|a| a.kind == "lan_v4")
        .or_else(|| routes.first())
        .map(|a| a.value.clone())
        .unwrap_or_else(|| "127.0.0.1".into());
    // update-check is intentionally skipped for the local row: it requires
    // the secrets service to mint a GitHub token, and we don't want pod.list
    // to fail (or hang on GitHub) when called before the daemon is fully
    // wired. Remote peers go through their own service registration so it's
    // available for them via the fanout path.
    PodPeerDto {
        // The local row carries this host's real machine identity; `local: true`
        // (below) is what marks it as local — never mask the id to "local".
        peer_id: system::host_identity::machine_id().to_string(),
        hostname: system::host_identity::display_hostname().to_string(),
        addr,
        port: db::ports::mesh_port(),
        last_seen_at: utils::time::now().unix_seconds(),
        local_secure: true,
        peer_secure: true,
        status: "active".into(),
        routes,
        local: true,
        reachable: Some(true),
        latency_ms: Some(0),
        probe_error: None,
        version: Some(
            option_env!("ORCA_VERSION")
                .unwrap_or(env!("CARGO_PKG_VERSION"))
                .into(),
        ),
        target: Some(
            option_env!("ORCA_BUILD_TARGET")
                .unwrap_or("unknown-target")
                .into(),
        ),
        frontend: Some(frontend.into()),
        mode,
        channel,
        pinned_to,
        update_latest: None,
        update_available: None,
        update_checked_secs: None,
        system: Some(system::system::TopologyFacts::from(
            &*system::system_info::current_or_collect(),
        )),
        // The local row publishes our own bootstrap-pubkey fp so peers that
        // learn about us via roster-sync can pin it transitively (otherwise
        // every cross-host pod/exec lands on "no pinned bootstrap key").
        pubkey_fp: utils::pki::load_or_init_bootstrap_key(&pki_dir())
            .ok()
            .map(|k| utils::pki::bootstrap_pubkey_fingerprint(&k.verifying_key())),
    }
}

/// Canonical peer-id for the local host. Mirrors the value the listener
/// publishes in its mTLS CN and on the wire (`<machine_id_short>`), so any
/// DB row matching this id is unambiguously a self-reference (e.g. mDNS
/// discovered us at our own LAN IP and stub'd us in via `ensure_peer_stub`).
pub fn local_peer_id() -> String {
    system::host_identity::machine_id().to_string()
}

/// Raw pod membership from `pod_peers` with the local flag set — NO on-demand
/// enrichment fan-out. Backs `pod.list`, the thin membership read that mesh
/// address-propagation (roster_sync) uses so a 60s roster tick does not trigger
/// every peer's detail/update fan-out. `list_enriched` is the UI path; this is
/// the discovery path. Identity + addressing only; every observed/telemetry
/// field is left at its default (None).
pub async fn list_raw() -> Result<Vec<PodPeerDto>> {
    let own_for_blocking = local_peer_id();
    let (mut members, saw_self) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<PodPeerDto>, bool)> {
            let conn = db::open_default()?;
            let peers = db::pod::list_peer_summaries(&conn)?;
            let mut saw_self = false;
            let members = peers
                .into_iter()
                .map(|p| {
                    let mut dto: PodPeerDto = p.into();
                    if dto.peer_id == own_for_blocking {
                        dto.local = true;
                        saw_self = true;
                    }
                    dto
                })
                .collect::<Vec<_>>();
            Ok((members, saw_self))
        })
        .await??;
    // First-boot fallback: prepend the synthesized local row so the source's own
    // identity is represented (clients skip local=true rows on ingest anyway).
    if !saw_self {
        members.insert(0, local_peer_row().await);
    }
    Ok(members)
}

/// Thin pod membership roster. Identity + addressing come from the cached
/// `pod_peers` row (how we reach each host); reachability + version come from
/// the in-memory liveness cache the background refresher maintains
/// ([`spawn_liveness_refresher`]).
///
/// This read NEVER dials. The previous implementation fanned out one live
/// `pod/ping` per remote peer INLINE, with a 5s per-dial timeout and no
/// concurrency bound, so the whole read blocked on the slowest/unreachable peer
/// — the 3+s `pod.list`. Probing is now decoupled into the refresher; here we
/// serve whatever is cached and fresh (younger than [`peer_info::PING_TTL`]).
/// A peer with no fresh probe renders `reachable = None` (unknown), never a
/// blocking dial and never a stale mirror value. Topology facts and channel/pin
/// live on the enriched views (`pod.snapshot`/`pod.instances`), not this thin
/// roster. The LOCAL row is built locally from local sources.
pub async fn list_lite() -> Result<Vec<PodPeerDto>> {
    let mut rows = list_raw().await?;
    for p in rows.iter_mut() {
        if p.local {
            // Local telemetry is fresh (built locally), but this roster is thin:
            // drop the topology facts — they ride the enriched views. Cheap
            // version/channel stay.
            p.system = None;
            continue;
        }
        // The cached `pod_peers` row carries NO trustworthy telemetry — wipe
        // every telemetry field so a stale mirror value never leaks.
        p.version = None;
        p.target = None;
        p.frontend = None;
        p.mode = None;
        p.channel = None;
        p.pinned_to = None;
        p.system = None;
        p.update_latest = None;
        p.update_available = None;
        p.update_checked_secs = None;
        // READ-ONLY, NO DIAL: serve reachability + version from the liveness
        // cache. Absent/expired => reachability unknown (`None`), sub-ms return.
        if let Some(live) = crate::peer_info::liveness_if_fresh(&p.peer_id) {
            p.reachable = Some(live.reachable);
            p.version = live.version;
            p.probe_error = live.probe_error;
        } else {
            p.reachable = None;
        }
    }
    Ok(rows)
}

/// Spawn the background liveness refresher. Probes every remote peer on an
/// interval with bounded concurrency and a tight per-dial timeout, storing each
/// result in the [`peer_info`] liveness cache. This is what decouples mesh
/// probing from the `pod.list`/`systems.list` read path so those reads stay
/// within the latency budget. Runs until the process exits.
pub fn spawn_liveness_refresher() -> tokio::task::JoinHandle<()> {
    system::periodic::spawn(
        system::periodic::PeriodicSpec {
            name: "pod.liveness.refresh",
            initial_delay: std::time::Duration::ZERO,
            interval: std::time::Duration::from_secs(10),
        },
        system::periodic::boxed(refresh_liveness_once),
    )
}

/// One liveness-refresh pass: probe every remote peer with bounded concurrency
/// and a tight per-dial timeout, writing results into the liveness cache.
async fn refresh_liveness_once() -> Result<()> {
    /// Cap on simultaneous mesh dials so a large pod can't fan out unbounded.
    const MAX_CONCURRENCY: usize = 8;
    /// Per-peer probe deadline — well under the roster budget and far below the
    /// 5s `pod/ping` default, so one slow peer can't stall the pass.
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);
    /// Deadline for the heavier `system.detail` + `system.update` cache warm.
    /// Larger than the ping budget (these carry the host snapshot) but still
    /// bounded so a slow peer can't stall the pass.
    const WARM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    let rows = list_raw().await?;
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENCY));
    let mut tasks = Vec::new();
    for p in rows.into_iter().filter(|p| !p.local) {
        let sem = sem.clone();
        let peer_id = p.peer_id.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = match sem.acquire().await {
                Ok(permit) => permit,
                Err(_) => return,
            };
            let live = match tokio::time::timeout(PROBE_TIMEOUT, ping(&peer_id)).await {
                Ok(pong) => crate::peer_info::PeerLiveness {
                    reachable: pong.ok,
                    version: pong.version,
                    probe_error: pong.error,
                },
                Err(_) => crate::peer_info::PeerLiveness {
                    reachable: false,
                    version: None,
                    probe_error: Some("probe timeout".to_string()),
                },
            };
            let reachable = live.reachable;
            crate::peer_info::put_liveness(&peer_id, live);
            // Keep the write-through detail + update caches warm so the enriched
            // roster + topology reads (pod.snapshot / pod.instances /
            // network.topology_view) hit cache instead of dialing on the read
            // path. Only for reachable peers; best-effort, route-aware by peer_id.
            if reachable {
                let warm = async {
                    let (_detail, _update) = tokio::join!(
                        crate::peer_info::peer_detail(&peer_id, false),
                        crate::peer_info::peer_update(&peer_id, false),
                    );
                };
                // Best-effort: a warm that times out leaves the last-good cache
                // entry in place for the next pass.
                if tokio::time::timeout(WARM_TIMEOUT, warm).await.is_err() {
                    tracing::debug!("pod.liveness detail/update warm timed out for {peer_id}");
                }
            }
        }));
    }
    for t in tasks {
        if let Err(e) = t.await {
            tracing::debug!("pod.liveness refresh probe join error: {e:#}");
        }
    }
    Ok(())
}

/// Assemble the enriched pod member set. `pod_peers` supplies identity +
/// addressing; every cross-host observed field (version, channel, update state,
/// OS snapshot, reachability) is fetched ON DEMAND from each active remote peer
/// via `peer_info` (short-TTL in-memory cache), fanned out in parallel. Nothing
/// is read from a mirror table — telemetry is not synced under the
/// data-classification law. The LOCAL row is built from local sources
/// (`local_peer_row`), never a self-RPC.
///
/// A per-peer fetch failure degrades that one peer (reachable=false, cross-host
/// fields left as-is/None) and never aborts the list.
async fn list_enriched_impl() -> Result<Vec<PodPeerDto>> {
    let own = local_peer_id();
    let own_for_blocking = own.clone();
    let (active, inactive): (Vec<PodPeerDto>, Vec<PodPeerDto>) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<PodPeerDto>, Vec<PodPeerDto>)> {
            let conn = db::open_default()?;
            let peers = db::pod::list_peer_summaries(&conn)?;
            Ok(peers
                .into_iter()
                .map(|p| {
                    let mut dto: PodPeerDto = p.into();
                    if dto.peer_id == own_for_blocking {
                        dto.local = true;
                    }
                    dto
                })
                .partition(|p| p.status == "active"))
        })
        .await??;

    let saw_self = active.iter().any(|p| p.local) || inactive.iter().any(|p| p.local);

    // Enrich active peers, preserving order. The local row is built locally; each
    // remote row fans out its on-demand fetches concurrently.
    let mut slots: Vec<Option<PodPeerDto>> = (0..active.len()).map(|_| None).collect();
    let mut tasks = Vec::new();
    for (i, p) in active.into_iter().enumerate() {
        if p.local {
            slots[i] = Some(local_peer_row().await);
        } else {
            tasks.push(tokio::spawn(async move { (i, enrich_remote(p).await) }));
        }
    }
    for t in tasks {
        match t.await {
            Ok((i, dto)) => slots[i] = Some(dto),
            Err(e) => tracing::debug!("pod.list enrich task join error: {e:#}"),
        }
    }

    let mut out: Vec<PodPeerDto> = slots.into_iter().flatten().collect();
    out.extend(inactive);

    // First-boot fallback only — once mDNS / pairing populates pod_peers the DB
    // row carries the canonical identity and this branch never fires again.
    if !saw_self {
        out.insert(0, local_peer_row().await);
    }

    // Derive `parent_peer_id` edges from TopologyClaim ↔ interface MAC matches
    // across the assembled peer set. Read-time only — no DB writes.
    crate::topology_infer::infer(&mut out);

    Ok(out)
}

/// Fetch a remote peer's observed state on demand and fold it into its DTO.
/// `system.detail` (runtime fields + OS snapshot + reachability) and
/// `system.update` (version/channel/pin/update-availability) run concurrently.
/// A `system.detail` failure marks the peer unreachable; an update failure just
/// leaves the update fields unset — neither aborts the caller.
async fn enrich_remote(mut p: PodPeerDto) -> PodPeerDto {
    let peer_id = p.peer_id.clone();
    // Telemetry comes SOLELY from the write-through detail/update caches
    // (`peer_detail`/`peer_update`), route-aware by peer_id and kept warm by the
    // background liveness refresher — so this read hits cache instead of dialing.
    // On a genuine miss the write-through fetches once and caches; on failure the
    // field stays absent, never a stale mirror.
    p.version = None;
    p.target = None;
    p.frontend = None;
    p.mode = None;
    p.channel = None;
    p.pinned_to = None;
    p.system = None;
    p.update_latest = None;
    p.update_available = None;
    p.update_checked_secs = None;
    let (detail, update) = tokio::join!(
        crate::peer_info::peer_detail(&peer_id, false),
        crate::peer_info::peer_update(&peer_id, false),
    );
    match detail {
        Ok(rep) => {
            p.version = Some(rep.version);
            p.target = Some(rep.target);
            p.frontend = Some(rep.frontend);
            p.mode = rep.mode;
            p.channel = rep.channel;
            p.pinned_to = rep.pinned_to;
            p.system = Some(rep.topology);
            // A successful live fetch IS the reachability signal; the snapshot is
            // fresh (age 0) because we just fetched it.
            p.reachable = Some(true);
            p.latency_ms = Some(0);
        }
        Err(e) => {
            p.reachable = Some(false);
            p.probe_error = Some(format!("{e:#}"));
        }
    }
    if let Ok(u) = update {
        if u.version.is_some() {
            p.version.clone_from(&u.version);
        }
        if u.channel.is_some() {
            p.channel.clone_from(&u.channel);
        }
        p.pinned_to.clone_from(&u.pinned_to);
        p.update_latest.clone_from(&u.latest);
        p.update_available = Some(u.update_available);
        let now = utils::time::now().unix_seconds();
        p.update_checked_secs = Some((now - u.checked_at).max(0) as u64);
    }
    p
}

/// Resolve a user-supplied peer selector (peer_id, hostname, or addr) to a
/// concrete dial address. Match is case-insensitive across all three fields;
/// departed peers are skipped. Ambiguity (e.g. two paired peers with the same
/// hostname) is rejected with a message listing the colliding peer_ids so the
/// caller can re-issue with the unambiguous form.
/// Legacy single-address projection over [`resolve_peer_row`]. Prod dispatch
/// now dials the full multi-channel target list, so this remains only as the
/// projection the resolution-behaviour unit tests assert against.
#[cfg(test)]
fn resolve_peer_addr(peers: &[pdb::PeerRow], input: &str) -> Result<String> {
    resolve_peer_row(peers, input).map(|p| p.peer_addr.clone())
}

/// Resolve a peer selector (peer_id / hostname / addr) to the single best
/// `PeerRow`. Callers that need the peer's full addressing (to build a
/// multi-channel dial-target list) use this; `resolve_peer_addr` is the
/// legacy single-address projection over it.
fn resolve_peer_row<'a>(peers: &'a [pdb::PeerRow], input: &str) -> Result<&'a pdb::PeerRow> {
    let want = input.to_ascii_lowercase();
    let matches: Vec<&pdb::PeerRow> = peers
        .iter()
        .filter(|p| {
            p.departed_at.is_none()
                && (p.peer_id.to_ascii_lowercase() == want
                    || p.peer_hostname.to_ascii_lowercase() == want
                    || p.peer_addr.to_ascii_lowercase() == want)
        })
        .collect();
    match matches.as_slice() {
        [] => anyhow::bail!("no active paired peer matches '{input}'"),
        [one] => Ok(*one),
        many => {
            // Multiple rows routinely describe the SAME physical peer: a
            // legacy `peer.`-prefixed id alongside the bare `machine_id_short`,
            // or a stale re-keyed identity next to the current one. All such
            // rows carry the same dial address. Collapse them — if every match
            // points at one address, pick the best row (secure first, then most
            // recently seen) and dial it. The mTLS handshake verifies identity
            // by the peer's live cert, so the redundant rows don't matter. Only
            // a genuine multi-host collision (one selector, distinct addresses)
            // stays ambiguous.
            let mut addrs: Vec<String> = many
                .iter()
                .map(|p| p.peer_addr.to_ascii_lowercase())
                .collect();
            addrs.sort();
            addrs.dedup();
            if addrs.len() == 1 {
                let best = many
                    .iter()
                    .max_by_key(|p| (p.peer_secure, p.last_seen_at))
                    .expect("matches is non-empty in this arm");
                return Ok(*best);
            }
            let ids: Vec<&str> = many.iter().map(|p| p.peer_id.as_str()).collect();
            anyhow::bail!(
                "ambiguous peer selector '{input}' matches {} peers across distinct addresses: {}; re-run with the peer_id form",
                many.len(),
                ids.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str, hostname: &str, addr: &str, departed: bool) -> pdb::PeerRow {
        pdb::PeerRow {
            peer_id: id.into(),
            peer_hostname: hostname.into(),
            peer_addr: addr.into(),
            peer_port: 12002,
            pubkey_fp: None,
            first_seen_at: 0,
            last_seen_at: 0,
            departed_at: if departed { Some(1) } else { None },
            local_secure: false,
            peer_secure: false,
        }
    }

    #[test]
    fn resolves_by_peer_id_case_insensitive() {
        let peers = vec![peer("abc", "host-e", "10.0.0.1", false)];
        assert_eq!(resolve_peer_addr(&peers, "ABC").unwrap(), "10.0.0.1");
    }

    #[test]
    fn resolves_by_hostname() {
        let peers = vec![peer("abc", "host-e", "10.0.0.1", false)];
        assert_eq!(resolve_peer_addr(&peers, "host-e").unwrap(), "10.0.0.1");
    }

    #[test]
    fn resolves_by_addr() {
        let peers = vec![peer("abc", "host-e", "10.0.0.1", false)];
        assert_eq!(resolve_peer_addr(&peers, "10.0.0.1").unwrap(), "10.0.0.1");
    }

    #[test]
    fn departed_peers_are_skipped() {
        let peers = vec![peer("abc", "host-e", "10.0.0.1", true)];
        let err = resolve_peer_addr(&peers, "host-e").unwrap_err();
        assert!(err.to_string().contains("no active paired peer"));
    }

    #[test]
    fn no_match_errors_with_selector() {
        let peers = vec![peer("abc", "host-e", "10.0.0.1", false)];
        let err = resolve_peer_addr(&peers, "host-i").unwrap_err();
        assert!(err.to_string().contains("'host-i'"));
    }

    #[test]
    fn ambiguous_hostname_lists_peer_ids() {
        let peers = vec![
            peer("abc", "host-e", "10.0.0.1", false),
            peer("def", "host-e", "10.0.0.2", false),
        ];
        let err = resolve_peer_addr(&peers, "host-e").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ambiguous"), "got: {msg}");
        assert!(msg.contains("abc"), "got: {msg}");
        assert!(msg.contains("def"), "got: {msg}");
    }

    #[test]
    fn one_active_one_departed_with_same_hostname_is_not_ambiguous() {
        let peers = vec![
            peer("abc", "host-e", "10.0.0.1", true),
            peer("def", "host-e", "10.0.0.2", false),
        ];
        assert_eq!(resolve_peer_addr(&peers, "host-e").unwrap(), "10.0.0.2");
    }

    fn peer_secure(id: &str, hostname: &str, addr: &str, secure: bool, seen: i64) -> pdb::PeerRow {
        let mut p = peer(id, hostname, addr, false);
        p.peer_secure = secure;
        p.last_seen_at = seen;
        p
    }

    #[test]
    fn same_host_prefixed_and_bare_id_one_address_is_not_ambiguous() {
        // Real freyr shape: legacy `peer.`-prefixed secure row + bare insecure
        // row, same hostname, same dial address → collapse, don't bail.
        let peers = vec![
            peer_secure("019e7105-991", "freyr", "192.0.2.15", true, 100),
            peer_secure("019e7105-991", "freyr", "192.0.2.15", false, 90),
        ];
        assert_eq!(resolve_peer_addr(&peers, "freyr").unwrap(), "192.0.2.15");
    }

    #[test]
    fn multiple_stale_identities_one_address_collapse() {
        // Real maple shape: three rows (two secure re-keyed ids + one bare
        // insecure), all one address → resolves.
        let peers = vec![
            peer_secure("019e7105-683", "maple", "192.0.2.11", true, 100),
            peer_secure("dd7a73cda622", "maple", "192.0.2.11", true, 100),
            peer_secure("dd7a73cda622", "maple", "192.0.2.11", false, 100),
        ];
        assert_eq!(resolve_peer_addr(&peers, "maple").unwrap(), "192.0.2.11");
    }

    #[test]
    fn same_hostname_distinct_addresses_still_ambiguous() {
        // Two genuinely different hosts sharing a hostname must still be
        // rejected — collapse only applies when the address is unambiguous.
        let peers = vec![
            peer_secure("abc", "host-e", "10.0.0.1", true, 100),
            peer_secure("def", "host-e", "10.0.0.2", true, 100),
        ];
        let err = resolve_peer_addr(&peers, "host-e").unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "got: {err}");
    }

    // ── resolve_peer_row best-row selection ──────────────────────────────────

    #[test]
    fn resolve_peer_row_addr_match_is_case_insensitive() {
        // Selector matching on peer_addr should be case-folded like the id and
        // hostname paths (IPv6 literals carry hex that can vary in case).
        let peers = vec![peer("abc", "host-e", "fe80::AB", false)];
        let row = resolve_peer_row(&peers, "FE80::ab").unwrap();
        assert_eq!(row.peer_id, "abc");
    }

    #[test]
    fn resolve_peer_row_collapse_prefers_secure_then_recent() {
        // Same identity, same address, three rows: the winner is secure-first,
        // then most-recently-seen. resolve_peer_row exposes the chosen row so we
        // can assert on the tie-break, not just the (identical) address.
        let peers = vec![
            peer_secure("019e7105-991", "freyr", "192.0.2.15", false, 200),
            peer_secure("019e7105-991", "freyr", "192.0.2.15", true, 90),
            peer_secure("019e7105-991", "freyr", "192.0.2.15", true, 150),
        ];
        let row = resolve_peer_row(&peers, "freyr").unwrap();
        // The two secure rows outrank the newer insecure one; among the secure
        // pair the last_seen_at=150 row wins.
        assert!(row.peer_secure);
        assert_eq!(row.last_seen_at, 150);
    }

    // ── serde shapes (assert on serialized strings, never Value) ──────────────

    #[test]
    fn ping_output_omits_none_fields_on_failure() {
        let out = PodPingOutput {
            ok: false,
            latency_ms: 0,
            error: Some("no such peer: nope".into()),
            peer_id: None,
            hostname: None,
            version: None,
        };
        let s = serde_json::to_string(&out).unwrap();
        assert_eq!(
            s,
            r#"{"ok":false,"latency_ms":0,"error":"no such peer: nope"}"#
        );
    }

    #[test]
    fn ping_output_success_carries_identity_fields() {
        let out = PodPingOutput {
            ok: true,
            latency_ms: 7,
            error: None,
            peer_id: Some("abc".into()),
            hostname: Some("host-e".into()),
            version: Some("0.20.0".into()),
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("\"error\""), "error omitted when None: {s}");
        assert!(s.contains(r#""ok":true"#), "{s}");
        assert!(s.contains(r#""latency_ms":7"#), "{s}");
        assert!(s.contains(r#""peer_id":"abc""#), "{s}");
        assert!(s.contains(r#""hostname":"host-e""#), "{s}");
        assert!(s.contains(r#""version":"0.20.0""#), "{s}");
    }

    #[test]
    fn trust_output_reports_mutual_flag() {
        let out = PodTrustOutput {
            peer_id: "abc".into(),
            local_secure: true,
            peer_secure: true,
            mutual: true,
            notify_result: "ok".into(),
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains(r#""mutual":true"#), "{s}");
        assert!(s.contains(r#""notify_result":"ok""#), "{s}");
    }

    #[test]
    fn discovery_row_dto_uses_discovery_state_key_and_omits_none_peer_id() {
        let dto = PodDiscoveryRowDto {
            pubkey_fp: "fp1".into(),
            peer_id: None,
            hostname: "host-e".into(),
            addr: "10.0.0.1".into(),
            port: 12002,
            discovery_state: "unclaimed".into(),
            can_invite: true,
            first_seen_at: 1,
            last_seen_at: 2,
        };
        let s = serde_json::to_string(&dto).unwrap();
        // The field is serialized as `discovery_state`, NOT `state`, to avoid
        // colliding with PodMember's `#[serde(tag = "state")]` discriminant.
        assert!(s.contains(r#""discovery_state":"unclaimed""#), "{s}");
        assert!(
            !s.contains(r#""state":"#),
            "must not emit bare `state`: {s}"
        );
        assert!(!s.contains("peer_id"), "None peer_id omitted: {s}");
    }

    #[test]
    fn pending_offer_dto_omits_optional_none_fields() {
        let dto = PodPendingOfferDto {
            offer_id: "o1".into(),
            direction: "in".into(),
            peer_pubkey_fp: "fp1".into(),
            peer_hostname: "host-e".into(),
            peer_addr: "10.0.0.1".into(),
            peer_port: 12002,
            inviter_peer_id: None,
            pod_id: None,
            expires_at: 100,
            ttl_secs: 42,
            created_at: 10,
        };
        let s = serde_json::to_string(&dto).unwrap();
        assert!(s.contains(r#""ttl_secs":42"#), "{s}");
        assert!(!s.contains("inviter_peer_id"), "None omitted: {s}");
        assert!(!s.contains("pod_id"), "None omitted: {s}");
    }

    #[test]
    fn offer_output_round_trips() {
        let out = PodOfferOutput {
            code: "ABCD-1234".into(),
            joiner_hostname: "host-e".into(),
            joiner_addr: "10.0.0.1".into(),
            joiner_port: 12002,
            joiner_pubkey_fp: "fp1".into(),
            offer_id: "o1".into(),
            expires_at: 999,
        };
        let s = serde_json::to_string(&out).unwrap();
        let back: PodOfferOutput = serde_json::from_str(&s).unwrap();
        assert_eq!(back.code, "ABCD-1234");
        assert_eq!(back.joiner_port, 12002);
        assert_eq!(back.expires_at, 999);
    }

    #[test]
    fn recover_output_reports_cleared_flag() {
        let out = crate::PodRecoverOutput {
            peer_id: "abc".into(),
            cleared: false,
        };
        let s = serde_json::to_string(&out).unwrap();
        assert_eq!(s, r#"{"peer_id":"abc","cleared":false}"#);
    }

    // ── db-backed read/guard paths (ephemeral, migrated SQLite) ───────────────

    fn tmp_db() -> tempfile::NamedTempFile {
        tempfile::NamedTempFile::new().unwrap()
    }

    #[tokio::test]
    async fn discover_maps_discovery_rows_to_dtos() {
        let tmp = tmp_db();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let conn = db::open_default().unwrap();
            pdb::upsert_discovery(
                &conn,
                "fp-1",
                None,
                "host-e",
                "10.0.0.7",
                12002,
                "unclaimed",
                true,
            )
            .unwrap();
            drop(conn);
            let rows = discover().unwrap();
            assert_eq!(rows.len(), 1);
            let r = &rows[0];
            assert_eq!(r.pubkey_fp, "fp-1");
            assert_eq!(r.hostname, "host-e");
            assert_eq!(r.addr, "10.0.0.7");
            assert_eq!(r.port, 12002);
            assert_eq!(r.discovery_state, "unclaimed");
            assert!(r.can_invite);
            assert!(r.peer_id.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn discover_empty_on_fresh_db() {
        let tmp = tmp_db();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            assert!(discover().unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn pending_lists_only_inbound_with_positive_ttl() {
        let tmp = tmp_db();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let conn = db::open_default().unwrap();
            pdb::insert_pending_offer(
                &conn,
                "offer-in",
                "in",
                "fp-in",
                "host-in",
                "10.0.0.8",
                12002,
                "hash-in",
                None,
                Some("inviter-1"),
                Some("pod-1"),
                3600,
                None,
                &[],
            )
            .unwrap();
            // Outbound offer must be excluded by the "in" filter.
            pdb::insert_pending_offer(
                &conn,
                "offer-out",
                "out",
                "fp-out",
                "host-out",
                "10.0.0.9",
                12002,
                "hash-out",
                None,
                None,
                None,
                3600,
                None,
                &[],
            )
            .unwrap();
            drop(conn);
            let rows = pending().unwrap();
            assert_eq!(rows.len(), 1, "only inbound offers surface");
            let r = &rows[0];
            assert_eq!(r.offer_id, "offer-in");
            assert_eq!(r.direction, "in");
            assert_eq!(r.inviter_peer_id.as_deref(), Some("inviter-1"));
            assert_eq!(r.pod_id.as_deref(), Some("pod-1"));
            assert!(r.ttl_secs > 0 && r.ttl_secs <= 3600, "ttl: {}", r.ttl_secs);
        })
        .await;
    }

    #[tokio::test]
    async fn cancel_offer_removes_outbound_rows_idempotently() {
        let tmp = tmp_db();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let conn = db::open_default().unwrap();
            pdb::insert_pending_offer(
                &conn,
                "offer-out",
                "out",
                "fp-out",
                "host-out",
                "10.0.0.9",
                12002,
                "hash-out",
                None,
                None,
                None,
                3600,
                None,
                &[],
            )
            .unwrap();
            drop(conn);
            // First cancel removes the row; the second is a no-op.
            assert_eq!(cancel_offer("10.0.0.9").unwrap(), 1);
            assert_eq!(cancel_offer("10.0.0.9").unwrap(), 0);
        })
        .await;
    }

    #[tokio::test]
    async fn offer_errors_when_addr_not_in_discovery() {
        let tmp = tmp_db();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            // Empty discovery table → the pre-dial lookup fails before any push.
            let err = match offer("203.0.113.9", None).await {
                Ok(_) => panic!("expected offer to fail on empty discovery table"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("not found in pod_discovery"),
                "got: {err:#}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn recover_clears_departed_then_is_noop() {
        let tmp = tmp_db();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let pid = utils::id::new();
            let conn = db::open_default().unwrap();
            pdb::upsert_peer(&conn, &pid, "host-e", "10.0.0.1", 12002, Some("fp"), "ca").unwrap();
            pdb::mark_peer_departed(&conn, &pid).unwrap();
            drop(conn);
            // First recover clears the flag; the second finds nothing to clear.
            let first = recover(&pid).unwrap();
            assert_eq!(first.peer_id, pid);
            assert!(first.cleared);
            assert!(!recover(&pid).unwrap().cleared);
        })
        .await;
    }

    #[tokio::test]
    async fn recover_unknown_peer_reports_not_cleared() {
        let tmp = tmp_db();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let out = recover(&utils::id::new()).unwrap();
            assert!(!out.cleared, "no row → nothing cleared");
        })
        .await;
    }

    #[tokio::test]
    async fn self_secure_round_trips_through_db() {
        let tmp = tmp_db();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            // Default is false on a fresh, migrated db.
            assert!(!get_self_secure().unwrap());
            assert!(set_self_secure(true).await.unwrap());
            assert!(get_self_secure().unwrap());
            assert!(!set_self_secure(false).await.unwrap());
            assert!(!get_self_secure().unwrap());
        })
        .await;
    }

    #[tokio::test]
    async fn ping_unknown_peer_returns_no_such_peer_without_dialing() {
        let tmp = tmp_db();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            // No matching peer row → the guard returns before any network dial.
            let out = ping("nonexistent").await;
            assert!(!out.ok);
            assert_eq!(out.latency_ms, 0);
            assert!(out.peer_id.is_none());
            assert!(
                out.error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("no such peer"),
                "got: {:?}",
                out.error
            );
        })
        .await;
    }
}
