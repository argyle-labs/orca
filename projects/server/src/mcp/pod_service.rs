use anyhow::{Context, Result};
use async_trait::async_trait;
use orca_sdk::pki;
use orca_tools_def::pod::{
    CertInfo, PodAcceptOutput, PodCertStatusOutput, PodDiscoveryRowDto, PodJoinOutput,
    PodLeaveOutput, PodOfferOutput, PodPendingOfferDto, PodPingOutput, PodService, PodTrustOutput,
};
use std::time::Instant;

use crate::pod::{db as pdb, pki_dir};

pub struct ServerPod;

#[async_trait]
impl PodService for ServerPod {
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

        let start = Instant::now();
        match crate::pod::ping(&peer.peer_addr).await {
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

    async fn offer(&self, _addr: &str, _port: Option<u16>) -> Result<PodOfferOutput> {
        anyhow::bail!(
            "manual pod offer is not yet implemented — mDNS auto-offer handles LAN pairing"
        )
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
