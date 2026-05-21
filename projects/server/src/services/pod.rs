use anyhow::{Context, Result};
use async_trait::async_trait;
use orca_sdk::pki;
use orca_tools_def::pod::{
    CertInfo, PodAcceptOutput, PodCertStatusOutput, PodDevDisableOutput, PodDevDisablePeerResult,
    PodDevEnableOutput, PodDevEnablePeerResult, PodDevSyncOutput, PodDevSyncPeerResult,
    PodDiscoveryRowDto, PodExecDispatch, PodJoinOutput, PodLeaveOutput, PodOfferOutput,
    PodPeerAddressDto, PodPeerDto, PodPendingOfferDto, PodPingOutput, PodService, PodTrustOutput,
};
use std::time::Instant;

use crate::pod::{db as pdb, pki_dir};

pub struct ServerPod;

#[async_trait]
impl PodService for ServerPod {
    async fn list_enriched(&self) -> Result<Vec<PodPeerDto>> {
        list_enriched_impl().await
    }

    async fn accept(&self, code: &str) -> Result<PodAcceptOutput> {
        let conn = db::open_default()?;
        let offer = pdb::find_pending_offer_by_code(&conn, code)?
            .context("no pending offer matches that code (mistyped, expired, or already used?)")?;
        drop(conn);

        let pki_d = pki_dir();
        std::fs::create_dir_all(pki::mesh_dir(&pki_d))?;
        let ca_pem = offer
            .mesh_ca_cert_pem
            .as_deref()
            .context("offer has no mesh CA cert")?;
        std::fs::write(pki::mesh_ca_cert_path(&pki_d), ca_pem.as_bytes())?;

        let peer_cn = crate::host_identity::machine_id_short().to_string();
        let display_name = crate::host_identity::display_hostname().to_string();
        let (csr_client_pem, client_key_pem) =
            pki::build_peer_csr(&peer_cn, pki::PeerRole::Client)?;
        let (csr_server_pem, server_key_pem) =
            pki::build_peer_csr(&peer_cn, pki::PeerRole::Server)?;

        let signing = pki::load_or_init_bootstrap_key(&pki_d)?;
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
        let env = pki::sign_envelope(&signing, &body)?;

        use crate::commands::pod::dial_bootstrap_pub;
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

        let server_dir = pki::mesh_dir(&pki_d).join("server");
        let client_dir = pki::mesh_dir(&pki_d).join("client");
        std::fs::create_dir_all(&server_dir)?;
        std::fs::create_dir_all(&client_dir)?;
        std::fs::write(pki::mesh_server_cert_path(&pki_d), &r.server_cert_pem)?;
        std::fs::write(pki::mesh_server_key_path(&pki_d), &server_key_pem)?;
        std::fs::write(pki::mesh_client_cert_path(&pki_d), &r.client_cert_pem)?;
        std::fs::write(pki::mesh_client_key_path(&pki_d), &client_key_pem)?;

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

    async fn trust(&self, peer_id: &str, on: bool) -> Result<PodTrustOutput> {
        let conn = db::open_default()?;
        let peer = pdb::list_peers(&conn)?
            .into_iter()
            .find(|p| p.peer_id == peer_id)
            .with_context(|| format!("no such peer: {peer_id}"))?;
        let new = pdb::set_trust(&conn, peer_id, Some(on), None)?;
        drop(conn);

        let notify_result = match crate::commands::pod::call_pod_method_pub(
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
            && let Err(e) = crate::commands::pod::replicate_ca_key_if_needed_pub(&peer).await
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

    async fn ping(&self, peer_id: &str) -> PodPingOutput {
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

        let targets = crate::pod::dialer::dial_targets_for_peer(&conn, peer_id, &peer.peer_addr)
            .unwrap_or_else(|_| vec![peer.peer_addr.clone()]);
        let start = Instant::now();
        match crate::pod::dialer::try_targets(
            &targets,
            |t| async move { crate::pod::ping(&t).await },
        )
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

    fn discover(&self) -> Result<Vec<PodDiscoveryRowDto>> {
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
                state: r.state,
                can_invite: r.can_invite,
                first_seen_at: r.first_seen_at,
                last_seen_at: r.last_seen_at,
            })
            .collect())
    }

    fn pending(&self) -> Result<Vec<PodPendingOfferDto>> {
        let conn = db::open_default()?;
        let rows = pdb::list_pending_offers(&conn, "in")?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
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

    async fn offer(&self, addr: &str, port: Option<u16>) -> Result<PodOfferOutput> {
        use crate::pod::scheduler::{OFFER_TTL_SECS, mint_pairing_code, push_offer};
        use orca_utils::config::APP_PLUGIN_PORT;

        let port = port.unwrap_or(APP_PLUGIN_PORT);

        // Look up the joiner in the discovery table by addr.
        let conn = db::open_default()?;
        let discovery = pdb::list_discovery(&conn)?;
        let d = discovery
            .into_iter()
            .find(|r| r.addr == addr || format!("{}:{}", r.addr, r.port) == addr)
            .with_context(|| {
                format!("{addr} not found in pod_discovery — is the joiner visible via mDNS?")
            })?;

        if pdb::has_open_outbound_offer(&conn, &d.pubkey_fp)? {
            anyhow::bail!(
                "an open outbound offer to {addr} already exists — wait for it to expire or for the joiner to accept"
            );
        }

        let pod_id = pdb::get_pod_id(&conn)?.unwrap_or_else(|| "default".to_string());
        let code = mint_pairing_code();
        let code_hash = pdb::hash_code(&code);
        let offer_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
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

    async fn join(&self, inviter_addr: &str, port: Option<u16>) -> Result<PodJoinOutput> {
        use orca_utils::config::APP_PLUGIN_PORT;
        let port = port.unwrap_or(APP_PLUGIN_PORT);
        Ok(PodJoinOutput {
            code: String::new(),
            inviter_addr: inviter_addr.to_string(),
            inviter_port: port,
        })
    }

    async fn leave_peer(&self, peer_id: &str) -> Result<PodLeaveOutput> {
        let conn = db::open_default()?;
        let peer = pdb::list_peers(&conn)?
            .into_iter()
            .find(|p| p.peer_id == peer_id)
            .with_context(|| format!("no such peer: {peer_id}"))?;
        drop(conn);

        let notify_result = match crate::commands::pod::call_pod_method_pub(
            &peer.peer_addr,
            peer.peer_port,
            "pod/peer-leaving",
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

    async fn dev_sync(&self) -> Result<PodDevSyncOutput> {
        use crate::commands::update::cmd_dev_sync;
        use orca_utils::state::DaemonMode;

        let conn = db::open_default()?;
        let peers = pdb::list_peers(&conn)?;
        drop(conn);

        // Peer dev-sync now rides the existing pod mTLS channel (`:12002`,
        // SNI=pod.orca.local). Identity is proven by the mesh-CA-signed client
        // cert — no bearer tokens, no plaintext HTTP, no cert-distribution
        // problem. Peers not in dev mode reply with status="skipped".
        let mut results: Vec<PodDevSyncPeerResult> = Vec::new();

        let handles: Vec<_> = peers
            .into_iter()
            .filter(|p| p.departed_at.is_none())
            .map(|peer| {
                let addr = peer.peer_addr.clone();
                let peer_id = peer.peer_id.clone();
                let hostname = peer.peer_hostname.clone();
                tokio::spawn(async move {
                    match crate::pod::dev_sync(&addr).await {
                        Ok(r) => PodDevSyncPeerResult {
                            peer_id,
                            hostname,
                            status: r.status,
                            detail: r.detail,
                        },
                        Err(e) => PodDevSyncPeerResult {
                            peer_id,
                            hostname,
                            status: "error".into(),
                            detail: Some(e.to_string()),
                        },
                    }
                })
            })
            .collect();

        for handle in handles {
            if let Ok(result) = handle.await {
                results.push(result);
            }
        }

        // Also sync this host if it's in dev mode
        if matches!(
            orca_utils::state::read().ok().flatten().map(|s| s.mode),
            Some(DaemonMode::Dev) | Some(DaemonMode::Parked)
        ) {
            match tokio::task::spawn_blocking(cmd_dev_sync).await? {
                Ok(r) => results.push(PodDevSyncPeerResult {
                    peer_id: "local".into(),
                    hostname: "localhost".into(),
                    status: if r.already_up_to_date {
                        "skipped".into()
                    } else {
                        "synced".into()
                    },
                    detail: if r.already_up_to_date {
                        None
                    } else {
                        Some(r.detail)
                    },
                }),
                Err(e) => results.push(PodDevSyncPeerResult {
                    peer_id: "local".into(),
                    hostname: "localhost".into(),
                    status: "error".into(),
                    detail: Some(e.to_string()),
                }),
            }
        }

        Ok(PodDevSyncOutput { results })
    }

    async fn dev_enable_fanout(&self, peers: &[String]) -> Result<PodDevEnableOutput> {
        use crate::commands::update::cmd_dev_enable;

        let all_targets = select_peer_targets(peers)?;
        let include_local = peers_includes_local(peers);
        let exclude = load_dev_exclude_set()?;
        let (targets, excluded) = partition_dev_targets(all_targets, &exclude);

        let handles: Vec<_> = targets
            .into_iter()
            .map(|(peer_id, hostname, addr)| {
                tokio::spawn(async move {
                    match crate::pod::dev_enable(&addr).await {
                        Ok(r) => PodDevEnablePeerResult {
                            peer_id,
                            hostname,
                            status: r.status,
                            detail: r.detail,
                        },
                        Err(e) => PodDevEnablePeerResult {
                            peer_id,
                            hostname,
                            status: "error".into(),
                            detail: Some(e.to_string()),
                        },
                    }
                })
            })
            .collect();

        let mut results: Vec<PodDevEnablePeerResult> = Vec::new();
        for (peer_id, hostname, _addr) in excluded {
            results.push(PodDevEnablePeerResult {
                peer_id,
                hostname,
                status: "skipped".into(),
                detail: Some("excluded by pod.dev_exclude_peers".into()),
            });
        }
        for h in handles {
            if let Ok(r) = h.await {
                results.push(r);
            }
        }

        if include_local {
            match tokio::task::spawn_blocking(cmd_dev_enable).await? {
                Ok(r) => results.push(PodDevEnablePeerResult {
                    peer_id: "local".into(),
                    hostname: "localhost".into(),
                    status: "enabled".into(),
                    detail: Some(format!(
                        "repo={} cloned={} parked={}",
                        r.repo_path, r.cloned, r.daemon_parked
                    )),
                }),
                Err(e) => results.push(PodDevEnablePeerResult {
                    peer_id: "local".into(),
                    hostname: "localhost".into(),
                    status: "error".into(),
                    detail: Some(e.to_string()),
                }),
            }
        }

        Ok(PodDevEnableOutput { results })
    }

    async fn dev_disable_fanout(&self, peers: &[String]) -> Result<PodDevDisableOutput> {
        use crate::commands::update::cmd_dev_disable;

        let targets = select_peer_targets(peers)?;
        let include_local = peers_includes_local(peers);

        let handles: Vec<_> = targets
            .into_iter()
            .map(|(peer_id, hostname, addr)| {
                tokio::spawn(async move {
                    match crate::pod::dev_disable(&addr).await {
                        Ok(r) => PodDevDisablePeerResult {
                            peer_id,
                            hostname,
                            status: r.status,
                            detail: r.detail,
                        },
                        Err(e) => PodDevDisablePeerResult {
                            peer_id,
                            hostname,
                            status: "error".into(),
                            detail: Some(e.to_string()),
                        },
                    }
                })
            })
            .collect();

        let mut results: Vec<PodDevDisablePeerResult> = Vec::new();
        for h in handles {
            if let Ok(r) = h.await {
                results.push(r);
            }
        }

        if include_local {
            match tokio::task::spawn_blocking(cmd_dev_disable).await? {
                Ok(r) => results.push(PodDevDisablePeerResult {
                    peer_id: "local".into(),
                    hostname: "localhost".into(),
                    status: "disabled".into(),
                    detail: Some(format!(
                        "dev_stopped={} reclaimed={}",
                        r.dev_process_stopped, r.daemon_reclaimed
                    )),
                }),
                Err(e) => results.push(PodDevDisablePeerResult {
                    peer_id: "local".into(),
                    hostname: "localhost".into(),
                    status: "error".into(),
                    detail: Some(e.to_string()),
                }),
            }
        }

        Ok(PodDevDisableOutput { results })
    }

    #[allow(clippy::disallowed_types)] // mirrors PodService::exec — peer-mesh wire payload
    async fn exec(
        &self,
        peer: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<PodExecDispatch> {
        // "local" / "localhost" → loopback round-trip via the same /api/tools
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

        let r = crate::pod::exec(&addr, tool, args).await?;
        Ok(PodExecDispatch {
            peer: peer.to_string(),
            tool: r.tool,
            result: r.result,
        })
    }

    fn cert_status(&self) -> Result<PodCertStatusOutput> {
        let pki_d = pki_dir();
        let founder = pki::has_mesh_ca_key(&pki_d);
        let member = pki::mesh_ca_cert_path(&pki_d).exists();

        let parse = |path: std::path::PathBuf| -> Option<CertInfo> {
            let pem = std::fs::read_to_string(&path).ok()?;
            let days = pki::cert_days_remaining(&pem).ok()?;
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
            mesh_ca: parse(pki::mesh_ca_cert_path(&pki_d)),
            leaf_server: parse(pki::mesh_server_cert_path(&pki_d)),
            leaf_client: parse(pki::mesh_client_cert_path(&pki_d)),
            ca_previous: parse(pki::mesh_ca_previous_cert_path(&pki_d)),
            bootstrap: parse(pki::bootstrap_cert_path(&pki_d)),
        })
    }
}

/// Resolve the peer fan-out target set. `filter` empty = every non-departed
/// paired peer. Otherwise, only peers whose `peer_id`, `peer_hostname`, or
/// `peer_addr` matches an entry in `filter` (case-insensitive). The special
/// value `"local"` (and `"localhost"`) is consumed by `peers_includes_local`
/// and ignored here.
fn select_peer_targets(filter: &[String]) -> Result<Vec<(String, String, String)>> {
    let conn = db::open_default()?;
    let peers = pdb::list_peers(&conn)?;
    drop(conn);

    let filter_lc: Vec<String> = filter
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| s != "local" && s != "localhost")
        .collect();

    let want_all = filter.is_empty()
        || filter
            .iter()
            .all(|s| matches!(s.to_ascii_lowercase().as_str(), "local" | "localhost"));

    Ok(peers
        .into_iter()
        .filter(|p| p.departed_at.is_none())
        .filter(|p| {
            if want_all {
                return true;
            }
            let id_lc = p.peer_id.to_ascii_lowercase();
            let host_lc = p.peer_hostname.to_ascii_lowercase();
            let addr_lc = p.peer_addr.to_ascii_lowercase();
            filter_lc
                .iter()
                .any(|f| f == &id_lc || f == &host_lc || f == &addr_lc)
        })
        .map(|p| (p.peer_id, p.peer_hostname, p.peer_addr))
        .collect())
}

/// Build the local-host row for `pod.list`. Uses the in-process lifecycle
/// service so the synthetic local entry stays in lock-step with what every
/// remote peer would self-report via `system.runtime-spec`.
async fn local_peer_row() -> PodPeerDto {
    let frontend = if cfg!(feature = "ui") {
        "embedded"
    } else {
        "disabled"
    };
    let mode = orca_utils::state::read()
        .ok()
        .flatten()
        .map(|s| match s.mode {
            orca_utils::state::DaemonMode::Daemon => "daemon".to_string(),
            orca_utils::state::DaemonMode::Parked => "parked".to_string(),
            orca_utils::state::DaemonMode::Dev => "dev".to_string(),
        });
    let channel = crate::commands::update::read_channel_marker().map(|c| c.as_marker().to_string());
    let pinned_to = crate::commands::update::read_version_pin();
    // update-check is intentionally skipped for the local row: it requires
    // the secrets service to mint a GitHub token, and we don't want pod.list
    // to fail (or hang on GitHub) when called before the daemon is fully
    // wired. Remote peers go through their own service registration so it's
    // available for them via the fanout path.
    PodPeerDto {
        peer_id: "local".into(),
        hostname: crate::host_identity::display_hostname().to_string(),
        addr: "127.0.0.1".into(),
        port: orca_utils::config::APP_PLUGIN_PORT,
        last_seen_at: chrono::Utc::now().timestamp(),
        local_secure: true,
        peer_secure: true,
        status: "active".into(),
        addresses: Vec::<PodPeerAddressDto>::new(),
        local: true,
        reachable: Some(true),
        latency_ms: Some(0),
        probe_error: None,
        version: Some(env!("ORCA_VERSION").into()),
        target: Some(env!("ORCA_BUILD_TARGET").into()),
        frontend: Some(frontend.into()),
        mode,
        channel,
        pinned_to,
        update_latest: None,
        update_available: None,
        system: Some((*crate::system_info::current_or_collect()).clone()),
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
    base.system = serde_json::from_str::<orca_tools_def::orca_lifecycle::SystemInfoReport>(
        &latest.payload_json,
    )
    .ok();
    let now = chrono::Utc::now().timestamp();
    base.reachable = Some(now - latest.snapshot_at_unix <= REACHABLE_FRESHNESS_SECS);
    // Re-purpose latency_ms to mean "age of latest snapshot in seconds" when
    // we have no live ping. Clamp at u32::MAX to avoid overflow on very old
    // rows; the dashboard treats anything > REACHABLE_FRESHNESS_SECS as
    // stale anyway.
    let age = (now - latest.snapshot_at_unix).max(0);
    base.latency_ms = Some(u32::try_from(age).unwrap_or(u32::MAX));
}

/// Read pod_peers + local host_status; merge into enriched DTOs.
/// No RPC fanout — every cross-host field comes from the locally-mirrored
/// status table, which the sync puller keeps fresh in the background.
async fn list_enriched_impl() -> Result<Vec<PodPeerDto>> {
    let (active, inactive, status_by_peer) =
        tokio::task::spawn_blocking(|| -> Result<(_, _, _)> {
            let conn = db::open_default()?;
            let peers = db::pod::list_peers(&conn)?;
            let status_rows = db::host_status::latest_per_peer(&conn)?;
            let mut map: std::collections::HashMap<String, db::host_status::HostStatusRow> =
                std::collections::HashMap::new();
            for r in status_rows {
                map.insert(r.peer_id.clone(), r);
            }
            let (active, inactive): (Vec<PodPeerDto>, Vec<PodPeerDto>) = peers
                .into_iter()
                .map(PodPeerDto::from)
                .partition(|p| p.status == "active");
            Ok((active, inactive, map))
        })
        .await??;

    let mut out: Vec<PodPeerDto> = Vec::with_capacity(active.len() + inactive.len() + 1);
    out.push(local_peer_row().await);

    for mut p in active {
        if let Some(latest) = status_by_peer.get(&p.peer_id) {
            enrich_from_local_db(&mut p, latest);
        }
        out.push(p);
    }
    out.extend(inactive);
    Ok(out)
}

/// Whether the fan-out should also flip the local host. Empty filter = yes;
/// otherwise only when `"local"` or `"localhost"` appears explicitly.
fn peers_includes_local(filter: &[String]) -> bool {
    if filter.is_empty() {
        return true;
    }
    filter
        .iter()
        .any(|s| matches!(s.to_ascii_lowercase().as_str(), "local" | "localhost"))
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

/// Settings key holding the comma-separated peer_id / hostname exclusion list
/// for `pod dev_enable`. Operator-controlled — never auto-populated. Set via
/// `orca config set pod.dev_exclude_peers "<csv>"`. Hosts named here are
/// reported as `status="skipped"` with detail `"excluded by …"` rather than
/// being dialled, so a disk-constrained box (loki: 54 GB root, can't hold the
/// ~20 GB cargo + target tree) doesn't get pulled into dev mode on a fanout.
const DEV_EXCLUDE_KEY: &str = "pod.dev_exclude_peers";

/// Read the dev-mode exclusion set from settings. Tokens are
/// comma-separated, case-insensitive; whitespace + empty entries are
/// ignored. Returns an empty set when the key is unset.
fn load_dev_exclude_set() -> Result<std::collections::HashSet<String>> {
    let conn = db::open_default()?;
    let raw = db::settings::get(&conn, DEV_EXCLUDE_KEY)?;
    drop(conn);
    Ok(parse_exclude_set(raw.as_deref()))
}

fn parse_exclude_set(raw: Option<&str>) -> std::collections::HashSet<String> {
    raw.unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Split fanout targets into (kept, excluded). A target is excluded when its
/// peer_id, hostname, or addr (case-insensitive) appears in the exclude set.
type DevTarget = (String, String, String);
fn partition_dev_targets(
    targets: Vec<DevTarget>,
    exclude: &std::collections::HashSet<String>,
) -> (Vec<DevTarget>, Vec<DevTarget>) {
    if exclude.is_empty() {
        return (targets, Vec::new());
    }
    let mut kept = Vec::with_capacity(targets.len());
    let mut skipped = Vec::new();
    for t in targets {
        let (id_lc, host_lc, addr_lc) = (
            t.0.to_ascii_lowercase(),
            t.1.to_ascii_lowercase(),
            t.2.to_ascii_lowercase(),
        );
        if exclude.contains(&id_lc) || exclude.contains(&host_lc) || exclude.contains(&addr_lc) {
            skipped.push(t);
        } else {
            kept.push(t);
        }
    }
    (kept, skipped)
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
        let peers = vec![peer("peer.abc", "willow", "10.0.0.1", false)];
        assert_eq!(resolve_peer_addr(&peers, "PEER.abc").unwrap(), "10.0.0.1");
    }

    #[test]
    fn resolves_by_hostname() {
        let peers = vec![peer("peer.abc", "willow", "10.0.0.1", false)];
        assert_eq!(resolve_peer_addr(&peers, "willow").unwrap(), "10.0.0.1");
    }

    #[test]
    fn resolves_by_addr() {
        let peers = vec![peer("peer.abc", "willow", "10.0.0.1", false)];
        assert_eq!(resolve_peer_addr(&peers, "10.0.0.1").unwrap(), "10.0.0.1");
    }

    #[test]
    fn departed_peers_are_skipped() {
        let peers = vec![peer("peer.abc", "willow", "10.0.0.1", true)];
        let err = resolve_peer_addr(&peers, "willow").unwrap_err();
        assert!(err.to_string().contains("no active paired peer"));
    }

    #[test]
    fn no_match_errors_with_selector() {
        let peers = vec![peer("peer.abc", "willow", "10.0.0.1", false)];
        let err = resolve_peer_addr(&peers, "mint").unwrap_err();
        assert!(err.to_string().contains("'mint'"));
    }

    #[test]
    fn ambiguous_hostname_lists_peer_ids() {
        let peers = vec![
            peer("peer.abc", "willow", "10.0.0.1", false),
            peer("peer.def", "willow", "10.0.0.2", false),
        ];
        let err = resolve_peer_addr(&peers, "willow").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ambiguous"), "got: {msg}");
        assert!(msg.contains("peer.abc"), "got: {msg}");
        assert!(msg.contains("peer.def"), "got: {msg}");
    }

    #[test]
    fn parse_exclude_set_handles_unset_empty_whitespace_and_case() {
        assert!(parse_exclude_set(None).is_empty());
        assert!(parse_exclude_set(Some("")).is_empty());
        assert!(parse_exclude_set(Some(" , ,")).is_empty());
        let s = parse_exclude_set(Some("Loki, peer.ABC ,, willow"));
        assert!(s.contains("loki"));
        assert!(s.contains("peer.abc"));
        assert!(s.contains("willow"));
        assert_eq!(s.len(), 3);
    }

    fn t(id: &str, h: &str, a: &str) -> DevTarget {
        (id.into(), h.into(), a.into())
    }

    #[test]
    fn partition_with_empty_exclude_keeps_everything() {
        let targets = vec![t("peer.a", "willow", "10.0.0.1")];
        let (kept, skipped) = partition_dev_targets(targets.clone(), &Default::default());
        assert_eq!(kept, targets);
        assert!(skipped.is_empty());
    }

    #[test]
    fn partition_excludes_by_hostname() {
        let targets = vec![
            t("peer.a", "willow", "10.0.0.1"),
            t("peer.b", "loki", "10.0.0.2"),
        ];
        let exclude = parse_exclude_set(Some("loki"));
        let (kept, skipped) = partition_dev_targets(targets, &exclude);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].1, "willow");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].1, "loki");
    }

    #[test]
    fn partition_excludes_by_peer_id_case_insensitive() {
        let targets = vec![t("peer.1a068644-54a", "loki", "10.0.0.2")];
        let exclude = parse_exclude_set(Some("PEER.1a068644-54a"));
        let (kept, skipped) = partition_dev_targets(targets, &exclude);
        assert!(kept.is_empty());
        assert_eq!(skipped.len(), 1);
    }

    #[test]
    fn partition_excludes_by_addr() {
        let targets = vec![t("peer.a", "willow", "10.0.0.99")];
        let exclude = parse_exclude_set(Some("10.0.0.99"));
        let (kept, skipped) = partition_dev_targets(targets, &exclude);
        assert!(kept.is_empty());
        assert_eq!(skipped.len(), 1);
    }

    #[test]
    fn one_active_one_departed_with_same_hostname_is_not_ambiguous() {
        let peers = vec![
            peer("peer.abc", "willow", "10.0.0.1", true),
            peer("peer.def", "willow", "10.0.0.2", false),
        ];
        assert_eq!(resolve_peer_addr(&peers, "willow").unwrap(), "10.0.0.2");
    }
}
