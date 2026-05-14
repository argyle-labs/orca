//! mDNS responder + discoverer for the pod mesh.
//!
//! Service type: `_orca._tcp.local.`
//!
//! TXT properties advertised by every orca:
//!   peer_id       — our pod CN (`peer.<hostname>`) or `unclaimed.<hostname>` pre-pod
//!   state         — `unclaimed` | `pod:<pod_id>`
//!   can_invite    — `1` iff we have mesh CA key AND self_secure=true
//!   pubkey_fp     — first-16-byte SHA-256 hex of our bootstrap ed25519 pubkey
//!   port          — TCP port for the pod surface (default 12002)
//!
//! Discovery loop upserts pod_discovery rows; the auto-offer scheduler
//! (pod::scheduler) reads from that table — it does NOT consume mDNS events
//! directly. This decoupling keeps the auto-offer logic testable against a
//! seeded DB instead of a live LAN.

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use orca_sdk::pki;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, warn};

const SERVICE_TYPE: &str = "_orca._tcp.local.";

#[derive(Debug, Clone)]
pub struct Advertisement {
    pub peer_id: String,
    pub state: String, // "unclaimed" | "pod:<pod_id>"
    pub can_invite: bool,
    pub pubkey_fp: String,
    pub hostname: String, // bare hostname, no .local
    pub port: u16,
}

impl Advertisement {
    /// Build from the current host state. `pod_id` is None when this orca
    /// hasn't joined any pod yet.
    pub fn from_local(
        hostname: &str,
        pubkey_fp: &str,
        pod_id: Option<&str>,
        can_invite: bool,
        port: u16,
    ) -> Self {
        Self {
            peer_id: match pod_id {
                Some(_) => format!("peer.{hostname}"),
                None => format!("unclaimed.{hostname}"),
            },
            state: match pod_id {
                Some(id) => format!("pod:{id}"),
                None => "unclaimed".to_string(),
            },
            can_invite,
            pubkey_fp: pubkey_fp.to_string(),
            hostname: hostname.to_string(),
            port,
        }
    }

    fn into_service_info(self) -> Result<ServiceInfo> {
        let host_fqdn = format!("{}.local.", self.hostname);
        let mut props: HashMap<String, String> = HashMap::new();
        props.insert("peer_id".into(), self.peer_id);
        props.insert("state".into(), self.state);
        props.insert(
            "can_invite".into(),
            if self.can_invite { "1" } else { "0" }.into(),
        );
        props.insert("pubkey_fp".into(), self.pubkey_fp.clone());
        props.insert("port".into(), self.port.to_string());

        // We deliberately bind addresses via "" — mdns-sd auto-detects local
        // interfaces. The instance name is the pubkey fp so two orcas on the
        // same hostname don't collide (e.g. mid-rename, dual-boot).
        let instance = format!("orca-{}", &self.pubkey_fp[..8]);
        ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &host_fqdn,
            "", // auto-detect IPs
            self.port,
            Some(props),
        )
        .context("build mDNS ServiceInfo")
        .map(|info| info.enable_addr_auto())
    }
}

/// Per-host mDNS state. Drop to stop advertising and browsing.
pub struct Mdns {
    daemon: ServiceDaemon,
    instance_fullname: String,
    _browse_task: tokio::task::JoinHandle<()>,
}

impl Mdns {
    /// Start the mDNS daemon, register our advertisement, and spawn the
    /// browser task that upserts pod_discovery rows for every peer seen.
    pub fn start(ad: Advertisement) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("create mDNS daemon")?;
        let info = ad.into_service_info()?;
        let instance_fullname = info.get_fullname().to_string();
        daemon.register(info).context("register mDNS service")?;

        let browse_rx = daemon.browse(SERVICE_TYPE).context("start mDNS browse")?;
        let our_instance = instance_fullname.clone();

        let browse_task = tokio::spawn(async move {
            loop {
                match browse_rx.recv_async().await {
                    Ok(event) => handle_event(event, &our_instance),
                    Err(e) => {
                        debug!("[mdns] browse channel closed: {e}");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            daemon,
            instance_fullname,
            _browse_task: browse_task,
        })
    }

    /// Refresh the advertisement (e.g. after `pod accept` flips state from
    /// unclaimed to pod-member, or after `pod self-secure on` flips
    /// `can_invite`).
    pub fn republish(&self, ad: Advertisement) -> Result<()> {
        let _ = self.daemon.unregister(&self.instance_fullname);
        let info = ad.into_service_info()?;
        self.daemon
            .register(info)
            .context("re-register mDNS service")?;
        Ok(())
    }

    /// Stop advertising and browsing.
    pub fn shutdown(&self) {
        let _ = self.daemon.unregister(&self.instance_fullname);
        // 1s grace for the unregister broadcast to flush, then shutdown.
        let d = self.daemon.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            let _ = d.shutdown();
        });
    }
}

fn handle_event(event: ServiceEvent, our_instance: &str) {
    let ServiceEvent::ServiceResolved(info) = event else {
        return; // SearchStarted, ServiceFound (unresolved), ServiceRemoved — ignored
    };
    if info.get_fullname() == our_instance {
        return; // don't self-discover
    }

    let props = info.get_properties();
    let pubkey_fp = match props.get_property_val_str("pubkey_fp") {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => {
            warn!(
                "[mdns] peer {} has no pubkey_fp, skipping",
                info.get_fullname()
            );
            return;
        }
    };
    let peer_id = props.get_property_val_str("peer_id").map(str::to_string);
    let state = props
        .get_property_val_str("state")
        .unwrap_or("unclaimed")
        .to_string();
    let can_invite = props.get_property_val_str("can_invite") == Some("1");
    let port = props
        .get_property_val_str("port")
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or_else(|| info.get_port());

    let hostname = info
        .get_hostname()
        .trim_end_matches(".local.")
        .trim_end_matches('.')
        .to_string();
    let addr = pick_best_addr(info.get_addresses().iter().map(|s| s.to_ip_addr()))
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| hostname.clone());

    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => {
            warn!("[mdns] could not open orca.db to record peer: {e}");
            return;
        }
    };
    if let Err(e) = crate::pod::db::upsert_discovery(
        &conn,
        &pubkey_fp,
        peer_id.as_deref(),
        &hostname,
        &addr,
        port,
        &state,
        can_invite,
    ) {
        warn!("[mdns] upsert_discovery failed for {hostname}: {e}");
    } else {
        debug!(
            "[mdns] discovered {hostname} ({addr}:{port}) state={state} can_invite={can_invite}"
        );
    }
}

/// Best-effort: read advertisement parameters from the local pki dir + DB.
/// Used at daemon startup. Returns None if the bootstrap key can't be loaded
/// (which would indicate a deeper problem; caller logs + skips mDNS in that case).
pub fn build_advertisement(pki_dir: PathBuf, port: u16) -> Result<Advertisement> {
    let signing = pki::load_or_init_bootstrap_key(&pki_dir)?;
    let pubkey_fp = pki::bootstrap_pubkey_fingerprint(&signing.verifying_key());

    let hostname = hostname_or_unknown();
    let can_invite = pki::has_mesh_ca_key(&pki_dir);
    // pod_id + self_secure from DB; failures non-fatal (we just advertise unclaimed).
    let (pod_id, self_secure) = match db::open_default() {
        Ok(conn) => (
            crate::pod::db::get_pod_id(&conn).unwrap_or(None),
            crate::pod::db::get_self_secure(&conn).unwrap_or(false),
        ),
        Err(_) => (None, false),
    };
    let can_invite = can_invite && self_secure;
    Ok(Advertisement::from_local(
        &hostname,
        &pubkey_fp,
        pod_id.as_deref(),
        can_invite,
        port,
    ))
}

/// Rank addresses so the auto-offer scheduler dials a routable one.
/// Lower score = better. Prefer routable IPv4, then routable IPv6, then
/// link-local IPv4, then link-local IPv6 (last resort — won't route between
/// hosts without scope id).
fn addr_score(ip: &std::net::IpAddr) -> u8 {
    match ip {
        std::net::IpAddr::V4(v4) => {
            if v4.is_link_local() || v4.is_loopback() {
                2
            } else {
                0
            }
        }
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            let link_local = (seg[0] & 0xffc0) == 0xfe80;
            if link_local || v6.is_loopback() { 3 } else { 1 }
        }
    }
}

fn pick_best_addr<I: IntoIterator<Item = std::net::IpAddr>>(addrs: I) -> Option<std::net::IpAddr> {
    addrs.into_iter().min_by_key(|ip| addr_score)
}

fn hostname_or_unknown() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
