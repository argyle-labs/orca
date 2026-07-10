use crate::{
    CertInfo, PodAcceptOutput, PodCertStatusOutput, PodDiscoveryRowDto, PodExecDispatch,
    PodJoinRequestOutput, PodLeaveOutput, PodOfferOutput, PodPeerAddressDto, PodPeerDto,
    PodPendingOfferDto, PodPingOutput, PodTrustOutput,
};
use anyhow::{Context, Result};
use db::ports::mesh_port;
use std::time::Instant;
use system::update_state::{read_channel_marker, read_version_pin};

use crate::cli::dial_bootstrap_pub;
use crate::pki_dir;
use crate::scheduler::{OFFER_TTL_SECS, mint_pairing_code, push_offer};
use db::pod as pdb;

#[derive(serde::Deserialize)]
struct DetailProbePayload {
    system: Option<system::system_info_types::SystemInfoReport>,
}

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

    let peer_cn = system::host_identity::machine_id_short().to_string();
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

    let resp_value = dial_bootstrap_pub(
        &offer.peer_addr,
        offer.peer_port,
        &offer.peer_pubkey_fp,
        "pod/join-confirm",
        serde_json::to_value(&env)?,
    )
    .await
    .context("pod/join-confirm over bootstrap channel failed")?;

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
        &offer.peer_addr,
        offer.peer_port,
        Some(&offer.peer_pubkey_fp),
        &r.ca_cert_pem,
    )?;
    pdb::delete_pending_offer(&conn, &offer.offer_id)?;

    Ok(PodAcceptOutput {
        pod_id: r.pod_id,
        inviter_peer_id: r.inviter_peer_id,
        inviter_hostname: offer.peer_hostname,
        inviter_addr: offer.peer_addr,
        inviter_port: offer.peer_port,
        self_secure: false,
    })
}

pub async fn trust(peer_id: &str, on: bool) -> Result<PodTrustOutput> {
    let conn = db::open_default()?;
    let peer = pdb::list_peers(&conn)?
        .into_iter()
        .find(|p| p.peer_id == peer_id)
        .with_context(|| format!("no such peer: {peer_id}"))?;
    let new = pdb::set_trust(&conn, peer_id, Some(on), None)?;
    drop(conn);

    let notify_result = match crate::cli::call_pod_method_pub(
        &peer.peer_addr,
        peer.peer_port,
        "pod/notify-trust",
        serde_json::json!({ "trust": on }),
    )
    .await
    {
        Ok(_) => "ok".to_string(),
        Err(e) => format!("warn: {e}"),
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
    let own_id = system::host_identity::machine_id_short().to_string();
    // Execute pod.trust on the remote host, making THEM set their
    // local_secure for us. `push: false` prevents recursion. The caller is the
    // local admin who invoked `pod.trust` — the recipient authorizes the
    // mutating handshake against that admin's replicated users row (zero-trust:
    // no synthetic/asserted identity).
    #[allow(clippy::disallowed_types)] // exec is the wire-level dispatch boundary
    let dispatch = exec(
        peer_id,
        "pod.trust",
        serde_json::json!({ "peer_id": own_id, "on": on, "push": false }),
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
    match crate::dialer::try_targets(&targets, |t| async move { crate::ping(&t).await }).await {
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

pub async fn join(inviter_addr: &str, port: Option<u16>) -> Result<PodJoinRequestOutput> {
    let port = port.unwrap_or_else(mesh_port);
    Ok(PodJoinRequestOutput {
        code: String::new(),
        inviter_addr: inviter_addr.to_string(),
        inviter_port: port,
    })
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
    drop(conn);

    let notify_result = match crate::cli::call_pod_method_pub(
        &peer.peer_addr,
        peer.peer_port,
        "pod/peer-removed",
        serde_json::json!({}),
    )
    .await
    {
        Ok(_) => "notified".to_string(),
        Err(e) => format!("warn: {e}"),
    };

    let conn = db::open_default()?;
    conn.execute("DELETE FROM pod_peers WHERE peer_id = ?", [peer_id])?;
    conn.execute("DELETE FROM pod_trust WHERE peer_id = ?", [peer_id])?;

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

    let addr = if is_local {
        "127.0.0.1".to_string()
    } else {
        let conn = db::open_default()?;
        let peers = pdb::list_peers(&conn)?;
        drop(conn);
        resolve_peer_addr(&peers, peer)?
    };

    let r = crate::exec_as(&addr, tool, args, caller, correlation_id).await?;
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
    drop(conn);

    let mut notified = Vec::new();
    for m in &members {
        // Skip the target itself and any already-departed members.
        if m.peer_id == peer_id || m.departed_at.is_some() {
            continue;
        }
        let result = match crate::cli::call_pod_method_pub(
            &m.peer_addr,
            m.peer_port,
            "pod/peer-forget",
            serde_json::json!({ "peer_id": peer_id }),
        )
        .await
        {
            Ok(_) => "notified".to_string(),
            Err(e) => format!("warn: {e}"),
        };
        notified.push(crate::PodForgetNotice {
            peer_id: m.peer_id.clone(),
            result,
        });
    }

    let conn = db::open_default()?;
    let rows_removed = pdb::forget_peer(&conn, peer_id)?;
    crate::runtime_cache::remove(peer_id);

    Ok(crate::PodForgetOutput {
        peer_id: peer_id.to_string(),
        rows_removed,
        notified,
    })
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
    let pinned_to = read_version_pin();
    // update-check is intentionally skipped for the local row: it requires
    // the secrets service to mint a GitHub token, and we don't want pod.list
    // to fail (or hang on GitHub) when called before the daemon is fully
    // wired. Remote peers go through their own service registration so it's
    // available for them via the fanout path.
    PodPeerDto {
        peer_id: "local".into(),
        hostname: system::host_identity::display_hostname().to_string(),
        addr: "127.0.0.1".into(),
        port: db::ports::mesh_port(),
        last_seen_at: utils::time::now().unix_seconds(),
        local_secure: true,
        peer_secure: true,
        status: "active".into(),
        addresses: Vec::<PodPeerAddressDto>::new(),
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
        system: Some((*system::system_info::current_or_collect()).clone()),
        // The local row publishes our own bootstrap-pubkey fp so peers that
        // learn about us via roster-sync can pin it transitively (otherwise
        // every cross-host pod/exec lands on "no pinned bootstrap key").
        pubkey_fp: utils::pki::load_or_init_bootstrap_key(&pki_dir())
            .ok()
            .map(|k| utils::pki::bootstrap_pubkey_fingerprint(&k.verifying_key())),
    }
}

/// "Reachable" threshold derived from snapshot freshness. A peer whose
/// latest synced status row is within this window is considered alive; older
/// than this and the dashboard treats it as offline. Matches the sync
/// puller's 60s cadence with a multiplier so a single missed pull doesn't
/// flip the indicator.
const REACHABLE_FRESHNESS_SECS: i64 = 180;

/// Fill the per-peer enrichment fields from the local `host_status` table.
/// The peer itself wrote those rows; the sync puller mirrored them in.
/// No network IO — this is the read-only consumer side of the mesh sync.
fn enrich_from_local_db(base: &mut PodPeerDto, latest: &db::host_status::HostStatusRow) {
    base.system =
        serde_json::from_str::<system::system_info_types::SystemInfoReport>(&latest.payload_json)
            .ok();
    let now = utils::time::now().unix_seconds();
    base.reachable = Some(now - latest.snapshot_at_unix <= REACHABLE_FRESHNESS_SECS);
    // Re-purpose latency_ms to mean "age of latest snapshot in seconds" when
    // we have no live ping. Clamp at u32::MAX to avoid overflow on very old
    // rows; the dashboard treats anything > REACHABLE_FRESHNESS_SECS as
    // stale anyway.
    let age = (now - latest.snapshot_at_unix).max(0);
    base.latency_ms = Some(u32::try_from(age).unwrap_or(u32::MAX));
}

/// Canonical peer-id for the local host. Mirrors the value the listener
/// publishes in its mTLS CN and on the wire (`<machine_id_short>`), so any
/// DB row matching this id is unambiguously a self-reference (e.g. mDNS
/// discovered us at our own LAN IP and stub'd us in via `ensure_peer_stub`).
pub fn local_peer_id() -> String {
    system::host_identity::machine_id_short().to_string()
}

/// Read pod_peers + local host_status; merge into enriched DTOs.
/// No RPC fanout — every cross-host field comes from the locally-mirrored
/// status table, which the sync puller keeps fresh in the background.
///
/// S4: the host's own row lives in `pod_peers` like any other peer (mDNS
/// stubs it in via `ensure_peer_stub`). We flag it with `local=true` so
/// UIs can highlight "this is me" without a divergent synthetic entry.
/// First-boot fallback: when nothing in `pod_peers` matches the canonical
/// local peer-id, prepend a synthesized row so the dashboard isn't blank
/// before mDNS / pairing populates the table.
async fn list_enriched_impl() -> Result<Vec<PodPeerDto>> {
    let own = local_peer_id();
    let own_for_blocking = own.clone();
    let (active, inactive, status_by_peer, update_by_peer, detail_by_peer) =
        tokio::task::spawn_blocking(move || -> Result<(_, _, _, _, _)> {
            let conn = db::open_default()?;
            let peers = db::pod::list_peer_summaries(&conn)?;
            let status_rows = db::host_status::latest_per_peer(&conn)?;
            let mut map: std::collections::HashMap<String, db::host_status::HostStatusRow> =
                std::collections::HashMap::new();
            for r in status_rows {
                map.insert(r.peer_id.clone(), r);
            }
            let mut updates: std::collections::HashMap<
                String,
                db::peer_update_state::PeerUpdateState,
            > = std::collections::HashMap::new();
            for r in db::peer_update_state::list_all(&conn)? {
                updates.insert(r.peer_id.clone(), r);
            }
            let mut details: std::collections::HashMap<
                String,
                db::peer_detail_state::PeerDetailState,
            > = std::collections::HashMap::new();
            for r in db::peer_detail_state::list_all(&conn)? {
                details.insert(r.peer_id.clone(), r);
            }
            let (active, inactive): (Vec<PodPeerDto>, Vec<PodPeerDto>) = peers
                .into_iter()
                .map(|p| {
                    let mut dto: PodPeerDto = p.into();
                    if dto.peer_id == own_for_blocking {
                        dto.local = true;
                    }
                    dto
                })
                .partition(|p| p.status == "active");
            Ok((active, inactive, map, updates, details))
        })
        .await??;

    let mut out: Vec<PodPeerDto> = Vec::with_capacity(active.len() + inactive.len() + 1);
    let mut saw_self = false;

    for mut p in active {
        if p.local {
            saw_self = true;
        }
        if let Some(latest) = status_by_peer.get(&p.peer_id) {
            enrich_from_local_db(&mut p, latest);
        }
        // Each field is overridden only when the cache holds a real value —
        // a `None` from a partially-populated cache row must not wipe a
        // version that came in via `enrich_from_local_db` (host_status mirror).
        // Symptom this guards against: chips "appear then disappear after
        // mount" when mesh fetch fails and the cache entry rotates through
        // an empty state. Matches the guard pattern used for the
        // `peer_update_state` override a few lines down.
        if let Some(rt) = crate::runtime_cache::get(&p.peer_id) {
            if rt.version.is_some() {
                p.version = rt.version;
            }
            if rt.target.is_some() {
                p.target = rt.target;
            }
            if rt.frontend.is_some() {
                p.frontend = rt.frontend;
            }
            if rt.mode.is_some() {
                p.mode = rt.mode;
            }
            if rt.channel.is_some() {
                p.channel = rt.channel;
            }
            if rt.pinned_to.is_some() {
                p.pinned_to = rt.pinned_to;
            }
        }
        // Persisted `system.update {}` probe results override the in-memory
        // runtime_cache for the version/channel/pin fields when both are
        // present — the probe is authoritative per peer, the runtime_cache
        // sometimes carries `system.detail` stale across daemon restarts.
        // Always set update_available/update_latest/update_checked_secs from
        // the probe; runtime_cache doesn't track those.
        // SKIP local peer: the periodic probe in `update_state_probe` explicitly
        // filters `peer_id != own` (it's a remote-only probe), so the row for
        // self is whatever was last persisted — potentially years old after a
        // version bump. The freshly-loaded build_local_peer_dto values are the
        // truth for self; only let probe overrides win for remote peers.
        if !p.local
            && let Some(u) = update_by_peer.get(&p.peer_id)
        {
            if u.version.is_some() {
                p.version.clone_from(&u.version);
            }
            if u.channel.is_some() {
                p.channel.clone_from(&u.channel);
            }
            p.pinned_to.clone_from(&u.pinned_to);
            p.update_latest.clone_from(&u.latest);
            p.update_available = Some(u.update_available);
            if let Some(checked) = u.checked_at {
                let now = utils::time::now().unix_seconds();
                let age = (now - checked).max(0) as u64;
                p.update_checked_secs = Some(age);
            }
        }
        // Cached `system.detail {}` probe payload — overrides `p.system` (which
        // came from the host_status mirror) with the fresher report the peer
        // returns from its own `system.detail` tool. Lets the UI drawer hydrate
        // without an on-open RPC for remote peers.
        if let Some(d) = detail_by_peer.get(&p.peer_id)
            && let Ok(payload) = serde_json::from_str::<DetailProbePayload>(&d.payload)
            && let Some(sys) = payload.system
        {
            p.system = Some(sys);
        }
        out.push(p);
    }
    for p in &inactive {
        if p.local {
            saw_self = true;
        }
    }
    out.extend(inactive);

    // First-boot fallback only — once mDNS / pairing populates pod_peers
    // the DB row carries the canonical identity and this branch never
    // fires again.
    if !saw_self {
        out.insert(0, local_peer_row().await);
    }

    // Derive `parent_peer_id` edges from TopologyClaim ↔ interface MAC
    // matches across the assembled peer set. Read-time only — no DB
    // writes — so a peer with stale claims doesn't mutate another peer's
    // stored snapshot.
    crate::topology_infer::infer(&mut out);

    Ok(out)
}

/// Resolve a user-supplied peer selector (peer_id, hostname, or addr) to a
/// concrete dial address. Match is case-insensitive across all three fields;
/// departed peers are skipped. Ambiguity (e.g. two paired peers with the same
/// hostname) is rejected with a message listing the colliding peer_ids so the
/// caller can re-issue with the unambiguous form.
fn resolve_peer_addr(peers: &[pdb::PeerRow], input: &str) -> Result<String> {
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
        [one] => Ok(one.peer_addr.clone()),
        many => {
            let ids: Vec<&str> = many.iter().map(|p| p.peer_id.as_str()).collect();
            anyhow::bail!(
                "ambiguous peer selector '{input}' matches {} peers: {}; re-run with the peer_id form",
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
}
