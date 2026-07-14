//! Pod / mesh tools surfaced to every tool surface (CLI + REST + MCP).
//!
//! `pod.list` mirrors the CLI's `orca pod list` so the web overview can
//! render paired peers without a bespoke REST endpoint. The mesh ops need
//! mTLS dials, PKI material, and bootstrap signing — the mesh networking lives
//! in this crate's modules (`dialer`, `listener`, `bootstrap`, …) alongside
//! `crate::server_pod`.
//!
//! Tools call `crate::server_pod::*` free fns directly — no service trait
//! (dissolved in slice 4 per [[feedback_no_indirection]]). The daemon only
//! registers a `PodRemoteExec` transport so orca-dispatch can route
//! `remote_ok` tools to peers.

pub mod cli;
pub mod host_status_sweep;
pub mod host_status_writer;
pub mod server_pod;
pub mod status;
pub mod topology_infer;

pub use db::replicate_engine::PeerSyncReport;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

// ── Args / Output types (shared by every surface) ────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPeerAddressDto {
    pub kind: String,
    /// Human-readable label for `kind` (e.g. `"LAN IPv4"` for `"lan_v4"`).
    /// Server-owned so every surface renders identical text without
    /// re-implementing the switch per client. `#[serde(default)]` so a
    /// rc.≤25 peer that omits the field still deserializes; receivers
    /// can recompute via `system::system_info::labels::addr_kind_label`.
    #[serde(default)]
    pub kind_label: String,
    pub value: String,
    pub source: String,
    pub last_seen_at: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPeerDto {
    pub peer_id: String,
    pub hostname: String,
    pub addr: String,
    pub port: u16,
    pub last_seen_at: i64,
    pub local_secure: bool,
    pub peer_secure: bool,
    /// "active" | "departed".
    pub status: String,
    /// Multi-channel addresses (LAN v4/v6, Tailscale, FQDN, …). May be empty
    /// for peers paired before slice 4 of the host-addressing plan landed.
    #[serde(default)]
    pub addresses: Vec<PodPeerAddressDto>,
    /// True for the synthetic local-host row prepended to `pod.list`. Remote
    /// peers are always false. Lets UIs flag "this is me" without string
    /// matching the hostname.
    #[serde(default)]
    pub local: bool,
    /// `pod/ping` succeeded inside the fanout budget. `None` when probing was
    /// skipped (e.g. departed peers); `Some(false)` when the dial errored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reachable: Option<bool>,
    /// Round-trip latency of the `pod/ping` probe, milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// Error string from the probe path (ping / runtime-spec / update-check).
    /// First failure wins so the UI has one line to surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
    /// Peer-reported `system.runtime-spec.version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Peer-reported build target triple.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Peer-reported "embedded" / "disabled" UI flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontend: Option<String>,
    /// Peer-reported daemon mode: "daemon" | "parked" | "dev".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Peer-reported release channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Peer-reported version pin if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_to: Option<String>,
    /// Latest release tag visible to the peer on its channel. Pulled from
    /// `system.update-check`; `None` when the probe failed or timed out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_latest: Option<String>,
    /// True when an update is available for the peer (and not blocked by
    /// `pinned_to`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_available: Option<bool>,
    /// Age in seconds of the last successful `system.update {}` probe against
    /// this peer. `None` until the periodic probe has succeeded at least
    /// once (or for the synthetic local-host row).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_checked_secs: Option<u64>,
    /// Cross-platform OS / hardware / process / network snapshot reported
    /// by the peer's `system.runtime-spec`. `None` when the probe failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<system::system_info_types::SystemInfoReport>,
    /// Bootstrap-pubkey fingerprint of this peer, as known to the responder.
    /// Propagated through roster sync so peers learned via intermediary can
    /// transitively pin the fp instead of arriving with `None` — without
    /// this, pod/exec from a roster-synced peer is refused with "no pinned
    /// bootstrap key" forever after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pubkey_fp: Option<String>,
}

/// Unified pod-membership view. Every row carries a `state` discriminant so
/// callers see joined members, in-flight handshakes, and mDNS-discovered
/// candidates in one shape. Replaces the previous trio of `system.peer.list`,
/// `system.peer.discovery.list`, and `system.peer.handshake.list` (2026-05-28
/// consolidation — see project_pod_peer_system_consolidation.md).
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum PodMember {
    /// Paired pod member — full mTLS peer with addressing, runtime info, and
    /// (when probed) ping latency + system snapshot. Boxed because the joined
    /// row carries an optional `SystemInfoReport` that's ~1 KB larger than
    /// the other variants; without the indirection the whole enum pays that
    /// size on every row.
    Joined(Box<PodPeerDto>),
    /// Pending inbound or outbound offer — pairing handshake in progress.
    Handshaking(PodPendingOfferDto),
    /// mDNS-discovered orca that is not yet paired.
    Discovered(PodDiscoveryRowDto),
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodListOutput {
    pub members: Vec<PodMember>,
}

// ── pod.snapshot — pre-classified one-shot rollup for the systems UI ─────────
//
// `pod.list` returns raw mesh state; the frontend then re-implements peer/
// candidate/stale/inbound-offer classification + cluster grouping in JS. That
// logic moves here so every surface gets the same shaped view and the systems
// page collapses from ~2000 lines to a thin renderer. See
// `projects/frontend/src/routes/+page.svelte` `refreshPodPeers` +
// `refreshProxmoxClusters` for the original JS source-of-truth.

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodCandidate {
    pub pubkey_fp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    pub hostname: String,
    pub addr: String,
    pub port: u16,
    pub can_invite: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodStaleRow {
    pub peer_id: String,
    pub hostname: String,
    pub addr: String,
    pub port: u16,
    /// "departed" | "orphan" | "stale self identity".
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<i64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodInboundOffer {
    pub offer_id: String,
    pub peer_hostname: String,
    pub peer_addr: String,
    pub peer_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inviter_peer_id: Option<String>,
    pub expires_at: i64,
    pub ttl_secs: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodSnapshotOutput {
    /// Same shape as `pod.list.members` — the UI reuses the existing type.
    pub members: Vec<PodMember>,
    /// mDNS-discovered, unclaimed, not a self-echo, not already paired.
    pub candidates: Vec<PodCandidate>,
    /// Departed joined peers + discovered orphans + stale self-identities.
    pub stale: Vec<PodStaleRow>,
    /// Handshaking offers whose `expires_at` is still in the future.
    pub inbound_offers: Vec<PodInboundOffer>,
    /// Plugin-neutral cluster roster (proxmox today; others later).
    pub clusters: Vec<contract::ClusterEntry>,
    /// `peer_id` → cluster name for every joined peer matched to a cluster
    /// via IP-first then hostname. Only matches included.
    pub cluster_membership: std::collections::BTreeMap<String, String>,
}

/// Pure classification helper — split out so unit tests can exercise the
/// rules without a `ToolCtx` or a live mesh.
fn classify_snapshot(
    members: Vec<PodMember>,
    now_secs: i64,
) -> (
    Vec<PodMember>,
    Vec<PodCandidate>,
    Vec<PodStaleRow>,
    Vec<PodInboundOffer>,
) {
    // Identify "self" hostname so we can drop this host's own mDNS echoes.
    let own_hostname = members
        .iter()
        .find_map(|m| match m {
            PodMember::Joined(p) if p.local => Some(p.hostname.to_lowercase()),
            _ => None,
        })
        .unwrap_or_default();

    // Active joined peer_ids — discovered rows that match these are paired
    // echoes, not candidates.
    let active_peer_ids: std::collections::HashSet<String> = members
        .iter()
        .filter_map(|m| match m {
            PodMember::Joined(p) if p.status == "active" => Some(p.peer_id.clone()),
            _ => None,
        })
        .collect();

    let mut candidates: Vec<PodCandidate> = Vec::new();
    let mut stale: Vec<PodStaleRow> = Vec::new();
    let mut inbound_offers: Vec<PodInboundOffer> = Vec::new();

    for m in &members {
        match m {
            PodMember::Joined(p) => {
                if !p.local && p.status != "active" {
                    stale.push(PodStaleRow {
                        peer_id: p.peer_id.clone(),
                        hostname: if p.hostname.is_empty() {
                            p.peer_id.clone()
                        } else {
                            p.hostname.clone()
                        },
                        addr: p.addr.clone(),
                        port: p.port,
                        reason: "departed".into(),
                        last_seen_at: None,
                    });
                }
            }
            PodMember::Handshaking(o) => {
                if o.expires_at > now_secs {
                    inbound_offers.push(PodInboundOffer {
                        offer_id: o.offer_id.clone(),
                        peer_hostname: o.peer_hostname.clone(),
                        peer_addr: o.peer_addr.clone(),
                        peer_port: o.peer_port,
                        inviter_peer_id: o.inviter_peer_id.clone(),
                        expires_at: o.expires_at,
                        ttl_secs: o.ttl_secs,
                    });
                }
            }
            PodMember::Discovered(d) => {
                // Drop live echoes of peers we're already paired with.
                if let Some(pid) = d.peer_id.as_deref()
                    && active_peer_ids.contains(pid)
                {
                    continue;
                }
                let is_self_echo =
                    !own_hostname.is_empty() && d.hostname.to_lowercase() == own_hostname;
                let unclaimed = d.discovery_state == "unclaimed";
                if unclaimed && !is_self_echo {
                    candidates.push(PodCandidate {
                        pubkey_fp: d.pubkey_fp.clone(),
                        peer_id: d.peer_id.clone(),
                        hostname: d.hostname.clone(),
                        addr: d.addr.clone(),
                        port: d.port,
                        can_invite: d.can_invite,
                    });
                } else if let Some(pid) = &d.peer_id {
                    stale.push(PodStaleRow {
                        peer_id: pid.clone(),
                        hostname: d.hostname.clone(),
                        addr: d.addr.clone(),
                        port: d.port,
                        reason: if is_self_echo {
                            "stale self identity".into()
                        } else {
                            "orphan".into()
                        },
                        last_seen_at: Some(d.last_seen_at),
                    });
                }
            }
        }
    }

    (members, candidates, stale, inbound_offers)
}

/// Match every joined peer to a cluster: IP-first across all addresses, then
/// `system.primary_ipv4`, then lowercased hostname against `ClusterNode.name`.
/// Match `PodInstance` rows to cluster names using the same IP-first /
/// hostname-fallback rules as [`match_clusters`]. Sibling crates building
/// inventory views from the post-projection `PodInstance` shape (e.g.
/// `inventory.tree`) call this instead of duplicating the resolver.
pub fn match_clusters_instances(
    instances: &[PodInstance],
    clusters: &[contract::ClusterEntry],
) -> std::collections::BTreeMap<String, String> {
    let mut by_ip: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut by_host: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for entry in clusters {
        let Some(cname) = entry.name.as_deref() else {
            continue;
        };
        for n in &entry.nodes {
            if let Some(ip) = n.ip.as_deref() {
                by_ip
                    .entry(ip.to_string())
                    .or_insert_with(|| cname.to_string());
            }
            if !n.name.is_empty() {
                by_host
                    .entry(n.name.to_lowercase())
                    .or_insert_with(|| cname.to_string());
            }
        }
    }

    let mut out: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for inst in instances {
        let mut matched: Option<&String> = None;
        for a in &inst.addresses {
            if let Some(hit) = by_ip.get(&a.value) {
                matched = Some(hit);
                break;
            }
        }
        if matched.is_none()
            && let Some(sys) = inst.system.as_ref()
            && let Some(ip) = sys.primary_ipv4.as_deref()
        {
            matched = by_ip.get(ip);
        }
        if matched.is_none() {
            let host = inst
                .system
                .as_ref()
                .and_then(|s| s.hostname.as_deref())
                .unwrap_or(inst.label.as_str())
                .to_lowercase();
            if !host.is_empty() {
                matched = by_host.get(&host);
            }
        }
        if let Some(cname) = matched {
            out.insert(inst.peer_id.clone(), cname.clone());
        }
    }
    out
}

fn match_clusters(
    members: &[PodMember],
    clusters: &[contract::ClusterEntry],
) -> std::collections::BTreeMap<String, String> {
    let mut by_ip: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut by_host: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for entry in clusters {
        let Some(cname) = entry.name.as_deref() else {
            continue;
        };
        for n in &entry.nodes {
            if let Some(ip) = n.ip.as_deref() {
                by_ip
                    .entry(ip.to_string())
                    .or_insert_with(|| cname.to_string());
            }
            if !n.name.is_empty() {
                by_host
                    .entry(n.name.to_lowercase())
                    .or_insert_with(|| cname.to_string());
            }
        }
    }

    let mut out: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for m in members {
        let PodMember::Joined(p) = m else { continue };
        let mut matched: Option<&String> = None;
        for a in &p.addresses {
            if let Some(hit) = by_ip.get(&a.value) {
                matched = Some(hit);
                break;
            }
        }
        if matched.is_none()
            && let Some(sys) = p.system.as_ref()
            && let Some(ip) = sys.primary_ipv4.as_deref()
        {
            matched = by_ip.get(ip);
        }
        if matched.is_none() {
            let host = p
                .system
                .as_ref()
                .and_then(|s| s.hostname.as_deref())
                .unwrap_or(p.hostname.as_str())
                .to_lowercase();
            if !host.is_empty() {
                matched = by_host.get(&host);
            }
        }
        if let Some(cname) = matched {
            out.insert(p.peer_id.clone(), cname.clone());
        }
    }
    out
}

// ── pod.instances — fully-shaped DTO for the systems UI ─────────────────────
//
// Returns a flat list of `PodInstance` rows the frontend renders directly:
// local row + every active joined peer, plus the same candidate / stale /
// inbound-offer classification as `pod.snapshot`. Replaces the client-side
// `seedInstancesFromLoad` / `seedInboundOffersFromLoad` / `reachableAddrs`
// utilities and the ~60-line bucketing block in `peers.svelte.ts`.

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PodInstanceAddress {
    pub kind: String,
    pub kind_label: String,
    pub value: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PodInstanceSecure {
    pub local: bool,
    pub peer: bool,
}

/// Fully-shaped instance row the frontend systems UI renders directly. Mirrors
/// the legacy TS `Instance` shape but every field is snake_case so the typed
/// SDK from regen flows through unchanged.
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PodInstance {
    pub id: String,
    pub peer_id: String,
    pub label: String,
    /// For the synthetic local row this is emitted as `""` — the frontend
    /// overwrites it with `window.location.origin` after fetch since the
    /// daemon doesn't know how the browser reached it. Remote rows carry the
    /// `addr:port` of the peer.
    pub origin: String,
    pub port: u16,
    /// `"local"` for the synthetic self row, `"system"` for every paired peer.
    pub role: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_to: Option<String>,

    pub update_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_latest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_checked_secs: Option<u64>,

    /// `"up"` | `"down"` | `"unknown"`.
    pub health: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Wall-clock millis when this row was assembled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_checked: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub secure: Option<PodInstanceSecure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,

    pub addresses: Vec<PodInstanceAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<system::system_info_types::SystemInfoReport>,

    /// LAN addresses reachable by the browser. Computed server-side from
    /// `addresses` + `system` to replace the JS `reachableAddrs()` helper.
    pub reachable_addrs: Vec<String>,

    /// Full version list from a `system.update {}` probe. Always empty on
    /// this endpoint — the page-level probe overlay populates it client-side.
    pub available_versions: Vec<system::update::VersionEntry>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodInstancesOutput {
    pub members: Vec<PodInstance>,
    pub candidates: Vec<PodCandidate>,
    pub stale: Vec<PodStaleRow>,
    pub inbound_offers: Vec<PodInboundOffer>,
}

/// Pure helper. Project a `PodPeerDto` (or the synthetic local row) into the
/// frontend-shaped `PodInstance`. Unit-tested.
fn build_instance(p: &PodPeerDto, is_local: bool, now_ms: i64) -> PodInstance {
    let role = if is_local { "local" } else { "system" };
    // Locality is a role, never an identity: the real machine `peer_id` is
    // carried on both local and remote rows. `id` stays distinct from
    // `peer_id` via a role prefix so the two fields never collapse to the
    // same masked value.
    let peer_id = p.peer_id.clone();
    let id = if is_local {
        format!("local:{}", p.peer_id)
    } else {
        format!("system:{}", p.peer_id)
    };
    let label = if p.hostname.is_empty() {
        p.peer_id.clone()
    } else {
        p.hostname.clone()
    };
    let origin = if is_local {
        String::new()
    } else {
        format!("{}:{}", p.addr, p.port)
    };
    let health = if is_local {
        // local health is filled in by the caller via the local probe; default
        // to "unknown" so a stale local row doesn't misreport.
        "unknown".to_string()
    } else if p.status == "active" {
        "up".to_string()
    } else {
        "down".to_string()
    };
    let addresses: Vec<PodInstanceAddress> = p
        .addresses
        .iter()
        .map(|a| PodInstanceAddress {
            kind: a.kind.clone(),
            kind_label: a.kind_label.clone(),
            value: a.value.clone(),
        })
        .collect();
    let secure = if is_local {
        None
    } else {
        Some(PodInstanceSecure {
            local: p.local_secure,
            peer: p.peer_secure,
        })
    };
    let reachable_addrs =
        reachable_addrs(&label, &addresses, p.system.as_ref(), p.port, role, &origin);

    PodInstance {
        id,
        peer_id,
        label,
        origin,
        port: p.port,
        role: role.into(),
        version: p.version.clone(),
        target: p.target.clone(),
        mode: p.mode.clone(),
        channel: p.channel.clone(),
        pinned_to: p.pinned_to.clone(),
        update_available: p.update_available.unwrap_or(false),
        update_latest: p.update_latest.clone(),
        update_checked_secs: p.update_checked_secs,
        health,
        error: None,
        last_checked: Some(now_ms),
        secure,
        status: if is_local {
            None
        } else {
            Some(p.status.clone())
        },
        addresses,
        system: p.system.clone(),
        reachable_addrs,
        available_versions: Vec::new(),
    }
}

/// Port of the JS `reachableAddrs()` helper. Returns the addresses the browser
/// should try when offering "open this host". v4-first then v6, falling back
/// to FQDN, then hostname (if `label` isn't IP-shaped and the row isn't
/// local), then origin. Pure — unit-tested.
fn reachable_addrs(
    label: &str,
    addresses: &[PodInstanceAddress],
    sys: Option<&system::system_info_types::SystemInfoReport>,
    port: u16,
    role: &str,
    origin: &str,
) -> Vec<String> {
    let v4 = addresses
        .iter()
        .find(|a| a.kind == "lan_v4")
        .map(|a| a.value.as_str())
        .or_else(|| sys.and_then(|s| s.primary_ipv4.as_deref()));
    let v6 = addresses
        .iter()
        .find(|a| a.kind == "lan_v6")
        .map(|a| a.value.as_str())
        .or_else(|| sys.and_then(|s| s.primary_ipv6.as_deref()));
    let mut out: Vec<String> = Vec::new();
    if let Some(v) = v4 {
        out.push(format!("{v}:{port}"));
    }
    if let Some(v) = v6 {
        out.push(format!("[{v}]:{port}"));
    }
    if !out.is_empty() {
        return out;
    }
    if let Some(fqdn) = sys.and_then(|s| s.fqdn.as_deref())
        && !fqdn.is_empty()
    {
        return vec![format!("{fqdn}:{port}")];
    }
    let is_ip = label.parse::<std::net::IpAddr>().is_ok();
    if !is_ip && role != "local" && !label.is_empty() {
        return vec![format!("{label}:{port}")];
    }
    vec![origin.to_string()]
}

// ── pod.join — unified pairing entry point ───────────────────────────────────
//
// `action` selects the pairing role:
//   "invite"  — inviter pushes offer to a discovered joiner  (needs `addr`)
//   "join"    — joiner pulls offer from an out-of-mDNS host  (needs `addr`)
//   "accept"  — joiner accepts a pending inbound offer        (needs `code`)

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PodJoinArgs {
    /// "invite" | "join" | "accept"
    pub action: String,
    /// Target address (host or host:port). Required for "invite" and "join".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub addr: Option<String>,
    /// Override port. Defaults to `APP_PLUGIN_PORT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub port: Option<u16>,
    /// 6-char pairing code. Required for "accept".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub code: Option<String>,
}

/// Output for `pod.join`, tagged by the pairing `action`. Each variant carries
/// exactly the fields its role produces — no cross-variant `Option` soup.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum PodJoinOutput {
    /// Inviter pushed an offer to a discovered joiner.
    Invite {
        pairing_code: String,
        joiner_hostname: String,
        joiner_addr: String,
        joiner_port: u16,
        joiner_pubkey_fp: String,
        offer_id: String,
        expires_at: i64,
    },
    /// Joiner requested an offer from an out-of-mDNS inviter.
    Join {
        pairing_code: String,
        inviter_addr: String,
        inviter_port: u16,
    },
    /// Joiner accepted a pending inbound offer; pod membership established.
    Accept {
        pod_id: String,
        inviter_peer_id: String,
        inviter_hostname: String,
        inviter_addr: String,
        inviter_port: u16,
        self_secure: bool,
    },
}

// kept for internal use by accept path
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodAcceptOutput {
    pub pod_id: String,
    pub inviter_peer_id: String,
    pub inviter_hostname: String,
    pub inviter_addr: String,
    pub inviter_port: u16,
    pub self_secure: bool,
}

// ── pod.trust ────────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PodTrustArgs {
    pub peer_id: String,
    // Bare `bool` derives as a flag (`--on`) under clap, which leaves no way
    // to express the positional `[ON]` shown in --help. Force value parsing
    // so `orca system peer update <peer> true|false` works.
    #[clap(action = clap::ArgAction::Set)]
    pub on: bool,
    /// When `true`, execute the trust update on the remote peer so THEY trust
    /// US rather than updating our local trust of them. Requires the peer to
    /// be reachable via mTLS and the caller to hold admin role.
    #[serde(default)]
    #[clap(long)]
    pub push: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodTrustOutput {
    pub peer_id: String,
    pub local_secure: bool,
    pub peer_secure: bool,
    /// True when both sides trust each other. Secure peers can sync
    /// credentials; non-mutual peers only retain their own credentials.
    pub mutual: bool,
    pub notify_result: String,
}

// ── pod.ping ─────────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PodPingArgs {
    /// Paired peer ID (`peer.<machine_id_short>`) — looked up in `pod_peers`
    /// for the dial target.
    pub peer_id: String,
}

#[derive(clap::Args, Default, Serialize, Deserialize, JsonSchema)]
pub struct PodSyncArgs {
    /// Optional source-peer filter (hostname / peer_id / addr). Omit to pull
    /// from every paired peer.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub peer: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodSyncOutput {
    pub peers: Vec<PeerSyncReport>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPingOutput {
    pub ok: bool,
    pub latency_ms: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

// ── pod.discover ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodDiscoveryRowDto {
    pub pubkey_fp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    pub hostname: String,
    pub addr: String,
    pub port: u16,
    /// mDNS-advertised membership: `"unclaimed"` or `"pod:<pod_id>"`. Named
    /// `discovery_state` (not `state`) so it doesn't collide with the
    /// `#[serde(tag = "state")]` discriminant on [`PodMember`], which would
    /// otherwise clobber the `"discovered"` tag and break state filtering.
    pub discovery_state: String,
    pub can_invite: bool,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PodDiscoveryListOutput(pub Vec<PodDiscoveryRowDto>);

// ── pod.pending ──────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPendingOfferDto {
    pub offer_id: String,
    pub direction: String,
    pub peer_pubkey_fp: String,
    pub peer_hostname: String,
    pub peer_addr: String,
    pub peer_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inviter_peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pod_id: Option<String>,
    pub expires_at: i64,
    pub ttl_secs: i64,
    pub created_at: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PodPendingListOutput(pub Vec<PodPendingOfferDto>);

// ── pod.offer ────────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PodOfferArgs {
    /// Joiner's bootstrap address (host or host:port). Joiner must already
    /// be in `pod_discovery` (mDNS-seen) so we know its pinned pubkey fp.
    pub addr: String,
    /// Optional override for the joiner's bootstrap port. Defaults to
    /// `APP_PLUGIN_PORT` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodOfferOutput {
    /// Pairing code minted for this offer; show to the operator so they can
    /// run `pod.accept` on the joiner side.
    pub code: String,
    pub joiner_hostname: String,
    pub joiner_addr: String,
    pub joiner_port: u16,
    pub joiner_pubkey_fp: String,
    pub offer_id: String,
    pub expires_at: i64,
}

// ── pod.join "join" sub-action — internal types ──────────────────────────────
//
// Used by the `pod.join` tool when `action="join"`: the joiner pulls an offer
// from an inviter not yet in mDNS. Renamed from PodJoinArgs/Output (2026-05-28)
// because the user-facing umbrella tool now owns the `PodJoin*` names.

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PodJoinRequestArgs {
    /// Inviter's address (host or host:port).
    pub inviter_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodJoinRequestOutput {
    pub code: String,
    pub inviter_addr: String,
    pub inviter_port: u16,
}

// ── pod.leave ────────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PodLeaveArgs {
    /// Peer to notify + remove. The full `pod leave` wipe path stays on the
    /// CLI (it touches secrets + PKI material and takes flags this tool
    /// purposely doesn't expose).
    pub peer_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodLeaveOutput {
    pub peer_id: String,
    pub notify_result: String,
    pub rows_removed: u32,
}

// ── pod.leave (voluntary self exit) ──────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodLeaveSelfResult {
    pub peer_id: String,
    pub notify_result: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodLeaveSelfOutput {
    /// Number of peer rows removed from `pod_peers` (one per paired peer).
    pub rows_removed: u32,
    pub peers: Vec<PodLeaveSelfResult>,
}

// ── pod.recover ──────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PodRecoverArgs {
    /// Peer whose stale `departed_at` flag should be cleared on THIS host.
    pub peer_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodRecoverOutput {
    pub peer_id: String,
    /// `true` if a `departed_at` flag was actually cleared. `false` means the
    /// peer either wasn't departed or doesn't exist locally.
    pub cleared: bool,
}

// ── pod.forget ───────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PodForgetArgs {
    /// Stale/orphan peer_id to purge mesh-wide (e.g. an old identity left over
    /// from a machine_id change, or a decommissioned host).
    pub peer_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodForgetNotice {
    /// A live member we asked to forget the target.
    pub peer_id: String,
    /// `"notified"` or `"warn: <err>"`.
    pub result: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodForgetOutput {
    pub peer_id: String,
    /// Rows deleted on THIS host across pod_peers/pod_trust/pod_discovery/offers.
    pub rows_removed: u32,
    /// Per-member fan-out result.
    pub notified: Vec<PodForgetNotice>,
}

// ── pod.cancel_offer ─────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PodCancelOfferArgs {
    /// Joiner address whose outbound offer(s) should be cleared
    /// (e.g. `192.0.2.28` or the value shown in the discovery row).
    #[arg(long)]
    pub addr: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PodCancelOfferOutput {
    pub addr: String,
    /// Rows removed from `pod_pending_offers`.
    pub rows_removed: u32,
}

// ── pod.cert-status ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CertInfo {
    pub cn: String,
    pub fingerprint: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub days_remaining: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodCertStatusOutput {
    pub founder: bool,
    pub member: bool,
    /// Tier-2 secrets-storage permission. When `true`, this host is authorized
    /// to hold encrypted secrets replicated from other pod members. Independent
    /// of cert trust — a fully paired host can still refuse to be a secrets
    /// sink. UI surfaces this as a Secrets-storage toggle distinct from Trust.
    #[serde(default)]
    pub self_secure: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mesh_ca: Option<CertInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leaf_server: Option<CertInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leaf_client: Option<CertInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_previous: Option<CertInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<CertInfo>,
}

// ── system.pod.update — singleton pod-settings update ───────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PodUpdateArgs {
    /// Toggle Tier-2 secrets-storage permission (`self_secure`). `None` leaves
    /// the current value unchanged so the tool can grow new fields without
    /// every caller having to opt out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub self_secure: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodUpdateOutput {
    pub self_secure: bool,
}

// ── DTO conversions + wire-dispatch types ───────────────────────────────────

mod dto_conversions {
    use super::*;

    impl From<db::host_addressing::PodPeerAddress> for PodPeerAddressDto {
        fn from(a: db::host_addressing::PodPeerAddress) -> Self {
            let kind_label = system::system_info::labels::addr_kind_label(&a.kind);
            Self {
                kind: a.kind,
                kind_label,
                value: a.value,
                source: a.source,
                last_seen_at: a.last_seen_at,
            }
        }
    }

    impl From<db::pod::PeerSummary> for PodPeerDto {
        fn from(p: db::pod::PeerSummary) -> Self {
            Self {
                peer_id: p.peer_id,
                hostname: p.hostname,
                addr: p.addr,
                port: p.port,
                last_seen_at: p.last_seen_at,
                local_secure: p.local_secure,
                peer_secure: p.peer_secure,
                status: p.status,
                addresses: p.addresses.into_iter().map(Into::into).collect(),
                local: false,
                reachable: None,
                latency_ms: None,
                probe_error: None,
                version: None,
                target: None,
                frontend: None,
                mode: None,
                channel: None,
                pinned_to: None,
                update_latest: None,
                update_available: None,
                update_checked_secs: None,
                system: None,
                pubkey_fp: p.pubkey_fp,
            }
        }
    }
}

/// Internal-only envelope for [`server_pod::exec`]. JSON `Value` here is the
/// JSON-RPC wire payload — type-erased only because the peer-side registry
/// dispatches by name. Callers go through [`crate::cli::exec_remote`], which
/// deserializes into the typed `OrcaToolDef::Output` immediately on receipt,
/// so no opaque value ever reaches a user-facing type.
#[allow(clippy::disallowed_types)]
pub struct PodExecDispatch {
    pub peer: String,
    pub tool: String,
    pub result: serde_json::Value,
}

/// Transport that lets the generic `contract::RemoteExec` trait dispatch
/// through `server_pod::exec`. Registered in the daemon's `build_tool_ctx` so
/// `cli::exec_remote::<T>(...)` (in orca-dispatch, which knows nothing about
/// pod) finds a peer transport. Unit struct — no service indirection.
pub struct PodRemoteExec;

#[async_trait::async_trait]
impl contract::RemoteExec for PodRemoteExec {
    #[allow(clippy::disallowed_types)]
    async fn exec(
        &self,
        peer: &str,
        tool: &str,
        args: serde_json::Value,
        caller: Option<contract::CallerIdentity>,
        correlation_id: Option<String>,
    ) -> anyhow::Result<serde_json::Value> {
        Ok(server_pod::exec(peer, tool, args, caller, correlation_id)
            .await?
            .result)
    }

    async fn refresh_peer_runtime(&self, peer: &str) -> anyhow::Result<()> {
        crate::host_status_writer::refresh_runtime_for_peer(peer).await
    }
}

// ── Tools ───────────────────────────────────────────────────────────────────

/// Collapse `peer.<mid>` / `unclaimed.<mid>` id forms to the shared `<mid>`
/// machine key. The single definition used by every pod read surface.
pub(crate) fn machine_key(peer_id: &str) -> &str {
    peer_id.split_once('.').map_or(peer_id, |(_, mid)| mid)
}

/// Canonical assembly of the pod member set — the ONE place that answers
/// "who is in the pod". Every read surface (`pod.list`, `pod.snapshot`,
/// `pod.instances`) builds on this so their member views can never diverge.
///
/// Joins the three source layers (joined membership + in-flight handshakes +
/// mDNS-discovered candidates) and applies the two identity-dedup rules
/// exactly once (cf. canonical-identity: one row per real host):
///  - drop the mDNS discovery phantom for any host already joined, and our own
///    self-sighting, by collapsing to the `<mid>` machine key;
///  - drop any non-local joined row that is really THIS host registered as a
///    peer of itself — the local row already represents it. Locality is a
///    flag, never a masked id, so this compares real machine keys.
///
/// Returns pre-classification members; callers layer their own projection
/// (thin list / classified snapshot / UI instances) on top.
async fn assemble_members() -> anyhow::Result<Vec<PodMember>> {
    let joined = server_pod::list_enriched().await?;
    let handshaking = server_pod::pending().unwrap_or_default();
    let discovered = server_pod::discover().unwrap_or_default();

    let own_key = system::host_identity::machine_id_short();
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    claimed.insert(own_key.to_string());
    for p in &joined {
        claimed.insert(machine_key(&p.peer_id).to_string());
    }
    let discovered: Vec<_> = discovered
        .into_iter()
        .filter(|d| {
            d.peer_id
                .as_deref()
                .is_none_or(|pid| !claimed.contains(machine_key(pid)))
        })
        .collect();

    let mut members = Vec::with_capacity(joined.len() + handshaking.len() + discovered.len());
    members.extend(
        joined
            .into_iter()
            .filter(|p| p.local || machine_key(&p.peer_id) != own_key)
            .map(|p| PodMember::Joined(Box::new(p))),
    );
    members.extend(handshaking.into_iter().map(PodMember::Handshaking));
    members.extend(discovered.into_iter().map(PodMember::Discovered));
    Ok(members)
}

/// Unified pod-membership view: joined members + in-flight handshakes +
/// mDNS-discovered candidates, each row tagged by `state`. Replaces the trio
/// of `system.peer.list`, `system.peer.discovery.list`, and
/// `system.peer.handshake.list` (2026-05-28 consolidation).
#[orca_tool(domain = "pod", verb = "list")]
async fn pod_list(_args: EmptyArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<PodListOutput> {
    let members = assemble_members()
        .await?
        .into_iter()
        .map(|m| match m {
            // `pod.list` is a thin membership overview. The full
            // `SystemInfoReport` is ~85 KB per host (history rings + top-process
            // tables) — embedding it on every row ballooned the list past 1.5 MB
            // across the fleet. The fat per-host snapshot belongs on the detail
            // surface (`system.detail`), not the list. Drop it here.
            PodMember::Joined(mut p) => {
                p.system = None;
                PodMember::Joined(p)
            }
            other => other,
        })
        .collect();
    Ok(PodListOutput { members })
}

/// Pre-classified rollup of pod state for the systems UI. Same `members`
/// payload as `pod.list`, plus candidate / stale / inbound-offer
/// classification and cluster-membership matching computed server-side
/// so every surface gets one shaped response instead of re-implementing
/// the rules per client.
#[orca_tool(domain = "pod", verb = "snapshot")]
async fn pod_snapshot(
    _args: EmptyArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<PodSnapshotOutput> {
    // Canonical member set shared with `pod.list` / `pod.instances` so no
    // surface can get a diverging view.
    let members = assemble_members().await?;

    let now_secs = utils::time::now().unix_seconds();
    let (members, candidates, stale, inbound_offers) = classify_snapshot(members, now_secs);

    let clusters = match ctx.service::<std::sync::Arc<dyn contract::ClusterRoster>>() {
        Ok(svc) => svc.list_clusters().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let cluster_membership = match_clusters(&members, &clusters);

    Ok(PodSnapshotOutput {
        members,
        candidates,
        stale,
        inbound_offers,
        clusters,
        cluster_membership,
    })
}

/// Fully-shaped instance roster for the systems UI. One round-trip returns
/// the local synthetic row + every active joined peer projected into
/// `PodInstance` (snake_case fields, server-derived `reachable_addrs`),
/// alongside the same candidate / stale / inbound-offer classification
/// `pod.snapshot` produces. Replaces the client-side seed + bucket logic in
/// `peers.svelte.ts` (slice S3).
#[orca_tool(domain = "pod", verb = "instances")]
async fn pod_instances(
    _args: EmptyArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PodInstancesOutput> {
    collect_pod_instances().await
}

/// Public re-entry point so sibling crates (e.g. `inventory`) can assemble
/// the same `PodInstance` projection without duplicating the active-peer +
/// synthetic-local logic. The `pod.instances` tool is a thin wrapper over
/// this fn.
pub async fn collect_pod_instances() -> anyhow::Result<PodInstancesOutput> {
    // Canonical member set shared with `pod.list` / `pod.snapshot`.
    let members_raw = assemble_members().await?;

    let now_secs = utils::time::now().unix_seconds();
    let now_ms = now_secs * 1000;
    let (members_classified, candidates, stale, inbound_offers) =
        classify_snapshot(members_raw, now_secs);

    // Project joined rows into PodInstance. Local row first, then active
    // remote peers in stable order. Departed / non-active rows go to `stale`.
    let mut instances: Vec<PodInstance> = Vec::new();
    let mut local_seen = false;
    for m in &members_classified {
        if let PodMember::Joined(p) = m
            && p.local
        {
            instances.push(build_instance(p, true, now_ms));
            local_seen = true;
            break;
        }
    }
    if !local_seen {
        // Synthesize a minimal local row so the UI always has one. Carry this
        // host's real identity — locality is signalled by `local: true`, not by
        // masking the id (see build_instance).
        let synthetic = PodPeerDto {
            peer_id: system::host_identity::machine_id_short().to_string(),
            hostname: system::host_identity::hostname().to_string(),
            addr: String::new(),
            port: 12000,
            last_seen_at: 0,
            local_secure: false,
            peer_secure: false,
            status: "active".into(),
            addresses: vec![],
            local: true,
            reachable: None,
            latency_ms: None,
            probe_error: None,
            version: None,
            target: None,
            frontend: None,
            mode: None,
            channel: None,
            pinned_to: None,
            update_latest: None,
            update_available: None,
            update_checked_secs: None,
            system: None,
            pubkey_fp: None,
        };
        instances.push(build_instance(&synthetic, true, now_ms));
    }
    for m in &members_classified {
        if let PodMember::Joined(p) = m
            && !p.local
            && p.status == "active"
        {
            instances.push(build_instance(p, false, now_ms));
        }
    }

    Ok(PodInstancesOutput {
        members: instances,
        candidates,
        stale,
        inbound_offers,
    })
}

/// Initiate or complete a pod-membership pairing.
///
/// `action`:
/// - `"invite"` — inviter pushes an offer to a discovered joiner. Requires
///   `addr` (joiner's host or host:port from mDNS discovery). Returns a
///   pairing code to show the operator; the joiner auto-accepts if its daemon
///   received the code in-band.
/// - `"join"` — joiner requests an offer from an inviter not yet in mDNS.
///   Requires `addr` (inviter's host or host:port). Returns the code the
///   inviter will display.
/// - `"accept"` — joiner accepts a pending inbound offer by its 6-char code.
///   Requires `code`. Returns the inviter identity after join.
#[orca_tool(domain = "pod", verb = "join")]
async fn pod_join(args: PodJoinArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<PodJoinOutput> {
    match args.action.as_str() {
        "invite" => {
            let addr = args
                .addr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("invite requires addr"))?;
            let out = server_pod::offer(addr, args.port).await?;
            Ok(PodJoinOutput::Invite {
                pairing_code: out.code,
                joiner_hostname: out.joiner_hostname,
                joiner_addr: out.joiner_addr,
                joiner_port: out.joiner_port,
                joiner_pubkey_fp: out.joiner_pubkey_fp,
                offer_id: out.offer_id,
                expires_at: out.expires_at,
            })
        }
        "join" => {
            let addr = args
                .addr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("join requires addr"))?;
            let out = server_pod::join(addr, args.port).await?;
            Ok(PodJoinOutput::Join {
                pairing_code: out.code,
                inviter_addr: out.inviter_addr,
                inviter_port: out.inviter_port,
            })
        }
        "accept" => {
            let code = args
                .code
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("accept requires code"))?;
            let out = server_pod::accept(code).await?;
            Ok(PodJoinOutput::Accept {
                pod_id: out.pod_id,
                inviter_peer_id: out.inviter_peer_id,
                inviter_hostname: out.inviter_hostname,
                inviter_addr: out.inviter_addr,
                inviter_port: out.inviter_port,
                self_secure: out.self_secure,
            })
        }
        other => anyhow::bail!("unknown action '{other}' (expected invite|join|accept)"),
    }
}

/// Set trust for a paired peer. Without `push`, mutates OUR local trust
/// (`local_secure`). With `push: true`, executes on the remote peer over
/// mTLS so THEY trust US (`peer_secure` from our perspective).
#[orca_tool(domain = "pod", verb = "trust")]
async fn pod_trust(args: PodTrustArgs, ctx: &contract::ToolCtx) -> anyhow::Result<PodTrustOutput> {
    if args.push {
        return server_pod::push_trust(&args.peer_id, args.on, ctx.caller()).await;
    }
    server_pod::trust(&args.peer_id, args.on).await
}

/// mTLS ping a paired peer; returns latency + their self-reported identity.
/// Kept distinct from `system.detail --peer <id>` because ping latency is a
/// *relationship* measurement between this host and the peer, not a property
/// of the peer itself.
#[orca_tool(domain = "pod", verb = "ping")]
async fn pod_ping(args: PodPingArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<PodPingOutput> {
    Ok(server_pod::ping(&args.peer_id).await)
}

/// Evict a paired peer: best-effort notify, then drop `pod_peers` + `pod_trust`
/// rows for it. Mirrors today's `system.peer.delete` semantics.
#[orca_tool(domain = "pod", verb = "kick", role = "admin")]
async fn pod_kick(args: PodLeaveArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<PodLeaveOutput> {
    server_pod::leave_peer(&args.peer_id).await
}

/// Voluntary pod exit: notify every paired peer we're leaving (best-effort),
/// then drop all `pod_peers` + `pod_trust` rows on this host. PKI material is
/// left in place — call `system bootstrap` to fully reset.
#[orca_tool(domain = "pod", verb = "leave", role = "admin", local_only = true)]
async fn pod_leave(
    _args: EmptyArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PodLeaveSelfOutput> {
    server_pod::leave_self().await
}

/// Clear a stale `departed_at` flag for a peer on THIS host. Recovery tool
/// for the 2026-05-28 kick/peer-leaving bug (and any future false-depart).
/// No network call — purely local row repair.
#[orca_tool(domain = "pod", verb = "recover", role = "admin", local_only = true)]
async fn pod_recover(
    args: PodRecoverArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PodRecoverOutput> {
    server_pod::recover(&args.peer_id)
}

/// Forget a stale/orphan peer_id mesh-wide: hard-delete it here AND fan a
/// one-way notice to every live member so they drop it too. Use for orphans
/// left by machine_id churn or decommissioned hosts — NOT for evicting a live
/// peer (that's `pod kick`).
#[orca_tool(domain = "pod", verb = "forget", role = "admin")]
async fn pod_forget(
    args: PodForgetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PodForgetOutput> {
    server_pod::forget(&args.peer_id).await
}

/// [MUTATES STATE] Clear stuck outbound pairing offer(s) for an address.
/// Use when a previous +Add never got accepted/expired and is blocking new
/// invites. Idempotent — returns rows removed (0 if nothing matched).
#[orca_tool(domain = "pod", verb = "cancel_offer", role = "admin")]
async fn pod_cancel_offer(
    args: PodCancelOfferArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PodCancelOfferOutput> {
    let rows_removed = server_pod::cancel_offer(&args.addr)?;
    Ok(PodCancelOfferOutput {
        addr: args.addr,
        rows_removed,
    })
}

/// Force a one-shot replication tick on this host (or — with `peer_id` set —
/// on the named remote peer via the universal peer-dispatch path) and return
/// a per-source-peer report. Replaces "wait 60s for the background tick to
/// fire and hope it worked." `peer` arg optionally filters which source peer
/// we pull from (hostname / peer_id / addr) — omit to pull from every paired
/// peer. Admin: this is operator-facing and can surface mesh errors.
#[orca_tool(domain = "pod", verb = "sync", role = "admin")]
async fn pod_sync(args: PodSyncArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<PodSyncOutput> {
    let reports = db::replicate_engine::sync_now(args.peer.as_deref()).await?;
    Ok(PodSyncOutput { peers: reports })
}

/// Days-remaining + rotation state for every mesh cert on this host, plus
/// the current `self_secure` (Tier-2 secrets-storage) setting.
#[orca_tool(domain = "pod", verb = "detail")]
async fn pod_detail(
    _args: EmptyArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PodCertStatusOutput> {
    server_pod::status()
}

/// Update pod-level settings on this host or — when a peer target is set
/// (`--peer <h>` / `X-Orca-Peer` / MCP `peer` arg) — on the named remote peer
/// over the pod mesh via the universal peer-dispatch stanza. Currently exposes
/// `self_secure` (Tier-2 secrets-storage permission). Admin-only because
/// flipping it can authorize secrets replication into this host.
#[orca_tool(domain = "pod", verb = "update", role = "admin")]
async fn pod_update(
    args: PodUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PodUpdateOutput> {
    let self_secure = match args.self_secure {
        Some(v) => server_pod::set_self_secure(v).await?,
        None => server_pod::get_self_secure()?,
    };
    Ok(PodUpdateOutput { self_secure })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_peer_address_from_db_row() {
        let row = db::host_addressing::PodPeerAddress {
            peer_id: "x".into(),
            kind: "lan_v4".into(),
            value: "10.0.0.5".into(),
            source: "mdns".into(),
            last_seen_at: 42,
        };
        let dto: PodPeerAddressDto = row.into();
        assert_eq!(dto.kind, "lan_v4");
        assert_eq!(dto.value, "10.0.0.5");
        assert_eq!(dto.source, "mdns");
        assert_eq!(dto.last_seen_at, 42);
    }

    #[test]
    fn pod_peer_from_db_summary_defaults_optional_fields_to_none() {
        let row = db::pod::PeerSummary {
            peer_id: "x".into(),
            hostname: "h".into(),
            addr: "1.2.3.4".into(),
            port: 12002,
            last_seen_at: 1,
            local_secure: true,
            peer_secure: false,
            status: "active".into(),
            addresses: vec![],
            pubkey_fp: None,
        };
        let dto: PodPeerDto = row.into();
        assert_eq!(dto.peer_id, "x");
        assert!(!dto.local);
        assert!(dto.reachable.is_none());
        assert!(dto.version.is_none());
        assert!(dto.system.is_none());
    }
}

// ── mesh networking: mTLS dials, PKI, bootstrap signing, pod-wire methods ──
mod bootstrap;
pub mod caller_token;
pub mod cert_rotation;
pub mod dialer;
pub mod dispatcher;
pub mod host_status_replica;
mod listener;
pub mod mdns;
pub mod mesh_listener;
pub mod roster_sync;
pub mod runtime_cache;
pub mod scheduler;
pub mod subscribe;
pub mod subscribe_client;
pub mod subscribe_demand;
pub mod subscribe_wire;
pub mod system_detail_probe;
pub mod transport;
pub mod update_state_probe;

pub use bootstrap::handle_pod_bootstrap_connection;
pub use listener::handle_pod_connection;

use ::db::ports::mesh_port;
use anyhow::{Context, Result};
use contract::config::{APP_PKI_DIR, APP_STATE_DIR};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use utils::framing::{read_frame, write_frame};
use utils::jsonrpc::{Message, Request, Response};

pub const POD_PING_METHOD: &str = "pod/ping";
pub const POD_DEV_SYNC_METHOD: &str = "pod/dev-sync";
pub const POD_DEV_ENABLE_METHOD: &str = "pod/dev-enable";
pub const POD_DEV_DISABLE_METHOD: &str = "pod/dev-disable";
pub const POD_EXEC_METHOD: &str = "pod/exec";
pub const POD_REPLICATE_EXPORT_METHOD: &str = "pod/replicate-export";
pub const POD_REPLICATE_PUSH_METHOD: &str = "pod/replicate-push";
pub const POD_REPLICATE_ROOTS_METHOD: &str = "pod/replicate-roots";

/// Body of `pod/replicate-export`: this host's full view of every shared-state
/// entity registered via `#[derive(Replicated)]` — `{ entity_name -> rows }`.
/// Signed with the host's bootstrap key so the puller can verify the payload
/// against the source peer's pinned `pod_peers.pubkey_fp` before merging.
/// Shared entities have no per-row owner (any paired host may publish), so the
/// signature is authenticated transport, not ownership. ONE bundle covers
/// users + (later) configs + settings. See project_unified_mesh_state.md.
mod replicate_wire {
    // Heterogeneous registry: each entity has its own typed row, so the common
    // bundle map is free-form JSON here (typed inside each entity's merge).
    #![allow(clippy::disallowed_types)]
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ReplicateBundle {
        pub peer_id: String,
        pub issued_at: i64,
        pub entities: std::collections::BTreeMap<String, serde_json::Value>,
    }
}

pub use replicate_wire::ReplicateBundle;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodPingResult {
    pub peer_id: String,
    pub version: String,
    pub hostname: String,
    /// Addressing snapshot of the responding peer (rc.25+). Optional +
    /// `#[serde(default)]` so rc.≤24 daemons that omit the field still
    /// deserialize cleanly. Callers use this to refresh
    /// `pod_peer_addresses` without requiring a re-pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addressing: Option<HostAddressingSnapshot>,
}

/// Peer-to-peer addressing snapshot carried on `pod/ping`. `display_name` is
/// the human label; `channels` is the per-channel address list (`lan_v4`,
/// `lan_v6`, `tailscale_v4`, `tailscale_v6`, `fqdn`). Source + detected_at
/// stay local to the responding peer and are not propagated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostAddressingSnapshot {
    pub display_name: String,
    pub channels: Vec<AddressChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressChannel {
    pub kind: String,
    /// Human-readable label for `kind`. Server-owned (see
    /// [`PodPeerAddressDto::kind_label`]). `#[serde(default)]` for
    /// wire-tolerance against rc.≤25 peers.
    #[serde(default)]
    pub kind_label: String,
    pub value: String,
}

/// Result of `pod/dev-sync`. `status` is one of:
/// - `"synced"` — `git pull` completed; cargo-watch will rebuild.
/// - `"skipped"` — peer is not in dev mode (intentional no-op).
/// - `"error"`  — pull failed; `detail` carries the message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodDevSyncResult {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits_pulled: Option<u32>,
}

/// Resolve the PKI dir for this host using the same logic as the rest of
/// the daemon (HOME + APP_STATE_DIR + APP_PKI_DIR).
pub fn pki_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(APP_STATE_DIR).join(APP_PKI_DIR)
}

/// Detect a mesh client cert whose CN doesn't match the current naming
/// convention (`<machine_id_short>`). Stale certs come from hosts joined
/// under the old `peer.<hostname>` convention; mixing the two produces
/// duplicate `pod_peers` rows because the TLS-extracted CN keys
/// `ensure_peer_stub` differ from the CNs minted by `pod/join-confirm`.
///
/// When a stale CN is detected we delete the mesh client/server cert+key
/// pairs and wipe `pod_peers/pod_trust/pod_pending_offers/pod_discovery`
/// so the daemon comes up unpaired and the operator can re-pair into a
/// clean mesh. The mesh CA + bootstrap key are preserved (host identity
/// + founder ability survive).
///
/// Returns `Ok(true)` if a reset happened. Best-effort: any error is
/// logged at warn and returns `Ok(false)` so daemon startup proceeds.
pub fn reset_if_stale_mesh_identity(pki_dir: &std::path::Path) -> Result<bool> {
    let cert_path = utils::pki::mesh_client_cert_path(pki_dir);
    let expected = system::host_identity::machine_id_short().to_string();

    // Classify current state into one of:
    //   "ok"     – cert present, CN matches expected. No-op.
    //   "stale"  – cert present, CN drifted. Wipe + (founder) reissue.
    //   "missing"– cert absent, founder must reissue from its CA. Wipe
    //              pod tables in case a prior partial reset left them.
    //   "none"   – cert absent, no CA. Pre-pod. No-op.
    let state = if cert_path.exists() {
        match std::fs::read_to_string(&cert_path)
            .ok()
            .and_then(|pem| {
                rustls_pemfile::certs(&mut pem.as_bytes())
                    .next()
                    .and_then(Result::ok)
            })
            .and_then(|der| utils::pki::peer_common_name(&der).ok())
        {
            Some(cn) if cn == expected => "ok",
            Some(cn) => {
                tracing::warn!(
                    "[pod] mesh client cert CN {cn:?} does not match expected {expected:?} — \
                     resetting pod identity (cert was issued under an older naming convention)."
                );
                "stale"
            }
            None => {
                tracing::warn!(
                    "[pod] mesh client cert at {} is unreadable — treating as stale",
                    cert_path.display()
                );
                "stale"
            }
        }
    } else if utils::pki::has_mesh_ca_key(pki_dir) {
        tracing::warn!(
            "[pod] mesh client cert is missing but this host holds the CA key — \
             founder will self-reissue client+server certs."
        );
        "missing"
    } else {
        return Ok(false);
    };

    if state == "ok" {
        return Ok(false);
    }

    let mesh = utils::pki::mesh_dir(pki_dir);
    for sub in ["client", "server"] {
        let d = mesh.join(sub);
        if d.exists() {
            _ = std::fs::remove_dir_all(&d);
        }
    }
    let conn = ::db::open_default()?;
    db::pod::wipe_pod_membership(&conn)?;
    drop(conn);

    // If this host holds the mesh CA key (founder), self-issue fresh
    // client/server certs under the new CN immediately so the daemon can
    // keep operating without an external re-pair. Joiner-only hosts have
    // to wait for an inviter; log the path so the operator knows.
    if utils::pki::has_mesh_ca_key(pki_dir) {
        let host = system::host_identity::machine_id_short().to_string();
        utils::pki::reissue_mesh_server_cert(pki_dir).context("self-reissue mesh server cert")?;
        utils::pki::reissue_mesh_client_cert(pki_dir, &host)
            .context("self-reissue mesh client cert")?;
        tracing::warn!(
            "[pod] founder reissued mesh client+server certs under CN {host}; \
             pod-membership wiped — re-pair joiners as needed"
        );
        let conn = ::db::open_default()?;
        db::pod::set_self_secure(&conn, true)?;
    } else {
        tracing::warn!(
            "[pod] mesh cert+pod-membership state wiped; daemon will come up unpaired — \
             re-pair this host with `orca pod join <inviter>` or wait for an mDNS auto-offer"
        );
    }
    Ok(true)
}

/// Dial `host` over mTLS with SNI=pod.orca.local, send a `pod/ping`, and
/// return the peer's report. `host` is a bare hostname or IP; the connector
/// always uses the canonical SNI so the server's resolver returns the
/// mesh-CA-signed cert.
pub async fn ping(host: &str) -> Result<PodPingResult> {
    call_typed(host, POD_PING_METHOD, None::<()>, Duration::from_secs(5)).await
}

/// Result of `pod/dev-enable`. `status` is `"enabled"` on success, `"error"`
/// on failure (`detail` carries the message).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodDevEnableResult {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_parked: Option<bool>,
}

/// Result of `pod/dev-disable`. `status` is `"disabled"` on success,
/// `"error"` on failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodDevDisableResult {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_process_stopped: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_reclaimed: Option<bool>,
}

/// Dial `host` over the existing pod mTLS channel and ask it to git-pull its
/// dev checkout. `host` is a bare hostname or IP; SNI is fixed to
/// `pod.orca.local`. Identity is proven by the mesh-CA-signed client cert —
/// no bearer tokens involved, so this is the canonical peer↔peer auth path.
pub async fn dev_sync(host: &str) -> Result<PodDevSyncResult> {
    // git pull + cargo-watch detect can run long on a slow LAN; allow more
    // headroom than `pod/ping`.
    call_typed(
        host,
        POD_DEV_SYNC_METHOD,
        None::<()>,
        Duration::from_secs(45),
    )
    .await
}

/// Ask `host` to flip into dev mode. cmd_dev_enable may clone the repo on
/// first run, so allow generous timeout.
pub async fn dev_enable(host: &str) -> Result<PodDevEnableResult> {
    call_typed(
        host,
        POD_DEV_ENABLE_METHOD,
        None::<()>,
        Duration::from_secs(120),
    )
    .await
}

// `pod/exec` is the wire-level JSON-RPC dispatch for cross-peer OrcaTool
// invocation. The Value fields here are strictly the JSON-RPC wire payload —
// the caller (`dispatch::cli::exec_remote`) serializes the tool's
// typed Args before this point and deserializes the typed Output immediately
// after, so opaque JSON never reaches any user-facing type.
mod exec_wire {
    #![allow(clippy::disallowed_types)]
    use serde::{Deserialize, Serialize};

    /// Parameters for `pod/exec`. `tool` is a fully-qualified
    /// `<domain>.<verb>` name; `args` is the on-wire JSON args payload.
    ///
    /// `caller_token` is an Ed25519-signed [`crate::caller_token::CallerToken`]
    /// minted by the calling peer's bootstrap key. The recipient verifies the
    /// signature, binds the signer fp to the authenticated peer, checks
    /// expiry/replay/args, and derives the effective role from its own
    /// replicated `users` table. Optional for back-compat with rc.≤11 peers.
    ///
    /// `caller_role` is the legacy unsigned role assertion, retained so newly
    /// updated peers can still drive rc.≤11 recipients that don't understand
    /// the token. New recipients prefer `caller_token` and ignore this when a
    /// valid token is present.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PodExecParams {
        pub tool: String,
        #[serde(default)]
        pub args: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub caller_role: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub caller_token: Option<utils::pki::SignedEnvelope>,
        /// End-to-end trace id stamped by the originating REST/SDK request
        /// (or synthesized by the daemon middleware). The recipient sets it
        /// on its per-request ctx + tracing span so a single browser action
        /// shows up under one trace id across every host's logs. Optional
        /// for back-compat with older peers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub correlation_id: Option<String>,
    }

    /// Wire result of `pod/exec` — `result` is the tool's serialized output.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PodExecResult {
        pub tool: String,
        pub result: serde_json::Value,
    }
}

pub use exec_wire::{PodExecParams, PodExecResult};

/// Dial `host` and dispatch an allowlisted OrcaTool on the peer over mTLS.
/// Identity is the mesh client cert; the peer additionally checks the tool's
/// `REMOTE_OK` flag and 401s anything not in its allowlist.
#[allow(clippy::disallowed_types)]
pub async fn exec(host: &str, tool: &str, args: serde_json::Value) -> Result<PodExecResult> {
    exec_as(host, tool, args, None, None).await
}

/// Same as [`exec`] but on behalf of a local operator. Mints an Ed25519-signed
/// [`caller_token`] from `caller` so the recipient can verify origin + derive
/// the role from its own replicated `users` table. `caller_role` is also set
/// (advisory) for back-compat with rc.≤11 recipients that predate the token.
#[allow(clippy::disallowed_types)]
pub async fn exec_as(
    host: &str,
    tool: &str,
    args: serde_json::Value,
    caller: Option<contract::CallerIdentity>,
    correlation_id: Option<String>,
) -> Result<PodExecResult> {
    let (caller_role, caller_token) = match caller {
        Some(id) => {
            let token =
                caller_token::mint(&pki_dir(), &id, tool, &args, caller_token::DEFAULT_TTL_SECS)?;
            (Some(id.role), Some(token))
        }
        None => (None, None),
    };
    call_typed(
        host,
        POD_EXEC_METHOD,
        Some(PodExecParams {
            tool: tool.to_string(),
            args,
            caller_role,
            caller_token,
            correlation_id,
        }),
        Duration::from_secs(120),
    )
    .await
}

/// Pull a peer's signed bundle of all shared-state entities. The returned
/// envelope is verified + merged by [`replication_sync`]; this fn just dials.
pub async fn fetch_replicate_bundle(host: &str) -> Result<utils::pki::SignedEnvelope> {
    call_typed(
        host,
        POD_REPLICATE_EXPORT_METHOD,
        None::<()>,
        Duration::from_secs(30),
    )
    .await
}

/// Push our signed bundle to `host`. Recipient verifies sig + pinned bootstrap
/// fp before merging. Returns the count of rows merged on the recipient.
pub async fn push_replicate_bundle(
    host: &str,
    envelope: &utils::pki::SignedEnvelope,
) -> Result<usize> {
    let result: ReplicatePushResult = call_typed(
        host,
        POD_REPLICATE_PUSH_METHOD,
        Some(envelope),
        Duration::from_secs(30),
    )
    .await?;
    Ok(result.merged)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicatePushResult {
    pub merged: usize,
}

/// Cheap divergence-check response: per-entity content roots from the peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicateRootsResult {
    pub roots: std::collections::BTreeMap<String, String>,
}

/// Fetch a peer's per-entity content roots. Cheap (32 bytes/entity); the
/// engine uses this to skip the full bundle fetch when nothing diverged.
pub async fn fetch_replicate_roots(host: &str) -> Result<ReplicateRootsResult> {
    call_typed(
        host,
        POD_REPLICATE_ROOTS_METHOD,
        None::<()>,
        Duration::from_secs(15),
    )
    .await
}

/// Ask `host` to drop dev mode and let the production daemon reclaim.
pub async fn dev_disable(host: &str) -> Result<PodDevDisableResult> {
    call_typed(
        host,
        POD_DEV_DISABLE_METHOD,
        None::<()>,
        Duration::from_secs(30),
    )
    .await
}

/// Open a fresh mTLS client connection to a peer's pod channel. Used by
/// both one-shot `call_typed` and long-lived streaming dials
/// (`subscribe_client`). The caller owns the returned stream.
pub(crate) async fn connect_pod_tls(
    host: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let pki = pki_dir();
    let bundle = utils::pki::load_mesh_client(&pki)
        .context("load mesh client bundle (run `orca pod init`)")?;
    let (chain, key) = utils::pki::parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem)?;
    let roots = Arc::new(utils::pki::ca_root_store(&bundle.ca_cert_pem)?);

    let client_config = ClientConfig::builder()
        .with_root_certificates((*roots).clone())
        .with_client_auth_cert(chain, key)
        .context("build client TLS config")?;

    let connector = TlsConnector::from(Arc::new(client_config));
    let addr = format!("{host}:{}", mesh_port());
    let tcp = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let sni = ServerName::try_from(utils::pki::POD_SERVER_SAN)
        .context("build SNI ServerName")?
        .to_owned();
    connector
        .connect(sni, tcp)
        .await
        .context("TLS handshake (is the peer's mesh CA the same as ours?)")
}

/// Generic mTLS JSON-RPC roundtrip to a peer over the pod channel. One-shot:
/// connect → write one request → read one response → return. No pooling yet;
/// adopters call this directly per peer. Keeping the connection short-lived
/// matches how `pod/ping` worked previously and avoids leaking sockets.
async fn call_typed<P, R>(
    host: &str,
    method: &str,
    params: Option<P>,
    timeout: Duration,
) -> Result<R>
where
    P: Serialize,
    R: for<'de> Deserialize<'de>,
{
    let mut tls = connect_pod_tls(host).await?;

    let params_value = match params {
        Some(p) => Some(serde_json::to_value(p).context("serialize request params")?),
        None => None,
    };
    let req = Request::new(1, method, params_value);
    let envelope = serde_json::to_vec(&req).context("serialize request")?;
    write_frame(&mut tls, &envelope)
        .await
        .context("write request frame")?;

    let raw = tokio::time::timeout(timeout, read_frame(&mut tls))
        .await
        .with_context(|| format!("{method} read timed out"))?
        .context("read response")?;
    let msg: Message =
        serde_json::from_slice(&raw).context("parse response as JSON-RPC Message")?;
    let resp: Response = match msg {
        Message::Response(r) => r,
        Message::Request(_) | Message::Notification(_) => {
            anyhow::bail!("unexpected message type in response to {method}")
        }
    };
    if let Some(err) = resp.error {
        anyhow::bail!("peer returned error: {}", err.message);
    }
    let result = resp.result.context("peer response had no result")?;
    serde_json::from_value(result).with_context(|| format!("parse {method} result"))
}

#[cfg(test)]
mod mesh_tests {
    use super::*;

    #[test]
    fn ping_result_deserializes_rc24_without_addressing() {
        let json = serde_json::json!({
            "peer_id": "abc",
            "version": "0.0.3",
            "hostname": "abc123",
        });
        let r: PodPingResult = serde_json::from_value(json).unwrap();
        assert_eq!(r.peer_id, "abc");
        assert!(r.addressing.is_none());
    }

    #[test]
    fn ping_result_roundtrip_rc25_with_addressing() {
        let json = serde_json::json!({
            "peer_id": "abc",
            "version": "0.0.4",
            "hostname": "abc123",
            "addressing": {
                "display_name": "host-g",
                "channels": [
                    { "kind": "lan_v4", "value": "10.0.0.8" },
                    { "kind": "tailscale_v4", "value": "100.96.1.2" },
                ],
            },
        });
        let r: PodPingResult = serde_json::from_value(json).unwrap();
        let a = r.addressing.expect("addressing populated");
        assert_eq!(a.display_name, "host-g");
        assert_eq!(a.channels.len(), 2);
        assert_eq!(a.channels[0].kind, "lan_v4");
        assert_eq!(a.channels[0].value, "10.0.0.8");
        assert_eq!(a.channels[1].kind, "tailscale_v4");
    }

    #[test]
    fn ping_result_serialize_omits_none_addressing() {
        let r = PodPingResult {
            peer_id: "abc".into(),
            version: "0.0.4".into(),
            hostname: "abc123".into(),
            addressing: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert!(
            v.get("addressing").is_none(),
            "None must be skipped on wire"
        );
    }
}

#[cfg(test)]
mod pod_snapshot_tests {
    use super::*;

    fn joined(peer_id: &str, hostname: &str, status: &str, local: bool) -> PodMember {
        PodMember::Joined(Box::new(PodPeerDto {
            peer_id: peer_id.into(),
            hostname: hostname.into(),
            addr: "10.0.0.1".into(),
            port: 7777,
            last_seen_at: 0,
            local_secure: false,
            peer_secure: false,
            status: status.into(),
            addresses: vec![],
            local,
            reachable: None,
            latency_ms: None,
            probe_error: None,
            version: None,
            target: None,
            frontend: None,
            mode: None,
            channel: None,
            pinned_to: None,
            update_latest: None,
            update_available: None,
            update_checked_secs: None,
            system: None,
            pubkey_fp: None,
        }))
    }

    fn discovered(
        pubkey_fp: &str,
        peer_id: Option<&str>,
        hostname: &str,
        discovery_state: &str,
    ) -> PodMember {
        PodMember::Discovered(PodDiscoveryRowDto {
            pubkey_fp: pubkey_fp.into(),
            peer_id: peer_id.map(|s| s.into()),
            hostname: hostname.into(),
            addr: "10.0.0.2".into(),
            port: 7777,
            discovery_state: discovery_state.into(),
            can_invite: true,
            first_seen_at: 0,
            last_seen_at: 42,
        })
    }

    fn handshaking(offer_id: &str, expires_at: i64) -> PodMember {
        PodMember::Handshaking(PodPendingOfferDto {
            offer_id: offer_id.into(),
            direction: "inbound".into(),
            peer_pubkey_fp: "fp".into(),
            peer_hostname: "h".into(),
            peer_addr: "10.0.0.3".into(),
            peer_port: 7777,
            inviter_peer_id: None,
            pod_id: None,
            expires_at,
            ttl_secs: 60,
            created_at: 0,
        })
    }

    #[test]
    fn inbound_offers_keep_non_expired_drop_expired() {
        let members = vec![handshaking("fresh", 1000), handshaking("stale", 50)];
        let (_m, _c, _s, offers) = classify_snapshot(members, 500);
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].offer_id, "fresh");
    }

    #[test]
    fn candidates_drop_self_echo() {
        let members = vec![
            joined("peer.self", "myhost", "active", true),
            discovered("fp1", Some("peer.x"), "MyHost", "unclaimed"),
            discovered("fp2", Some("peer.y"), "other", "unclaimed"),
        ];
        let (_m, candidates, stale, _o) = classify_snapshot(members, 0);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].hostname, "other");
        // self-echo lands in stale with the dedicated reason.
        assert!(stale.iter().any(|s| s.reason == "stale self identity"));
    }

    #[test]
    fn candidates_drop_already_joined() {
        let members = vec![
            joined("peer.a", "ha", "active", false),
            discovered("fp", Some("peer.a"), "ha", "unclaimed"),
        ];
        let (_m, candidates, _s, _o) = classify_snapshot(members, 0);
        assert!(candidates.is_empty());
    }

    #[test]
    fn stale_includes_inactive_joined_as_departed() {
        let members = vec![joined("peer.gone", "gone", "departed", false)];
        let (_m, _c, stale, _o) = classify_snapshot(members, 0);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].reason, "departed");
        assert_eq!(stale[0].peer_id, "peer.gone");
    }

    #[test]
    fn stale_includes_orphan_discovered_with_peer_id() {
        // Non-unclaimed discovery row with a peer_id but no matching joined.
        let members = vec![discovered("fp", Some("peer.orph"), "host", "pod:other")];
        let (_m, candidates, stale, _o) = classify_snapshot(members, 0);
        assert!(candidates.is_empty());
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].reason, "orphan");
    }

    #[test]
    fn match_clusters_ip_first_then_hostname() {
        let mut p_ip = match joined("peer.byip", "ignored", "active", false) {
            PodMember::Joined(b) => *b,
            _ => unreachable!(),
        };
        p_ip.addresses.push(PodPeerAddressDto {
            kind: "lan_v4".into(),
            kind_label: "LAN IPv4".into(),
            value: "10.0.0.99".into(),
            source: "test".into(),
            last_seen_at: 0,
        });
        let p_host = match joined("peer.byname", "node-b", "active", false) {
            PodMember::Joined(b) => *b,
            _ => unreachable!(),
        };
        let members = vec![
            PodMember::Joined(Box::new(p_ip)),
            PodMember::Joined(Box::new(p_host)),
        ];

        let clusters = vec![contract::ClusterEntry {
            endpoint: "ep".into(),
            name: Some("alpha".into()),
            quorate: Some(true),
            nodes: vec![
                contract::ClusterNode {
                    name: "node-a".into(),
                    ip: Some("10.0.0.99".into()),
                    online: Some(true),
                },
                contract::ClusterNode {
                    name: "node-b".into(),
                    ip: None,
                    online: Some(true),
                },
            ],
        }];

        let m = match_clusters(&members, &clusters);
        assert_eq!(m.get("peer.byip").map(String::as_str), Some("alpha"));
        assert_eq!(m.get("peer.byname").map(String::as_str), Some("alpha"));
    }

    // ── pod.instances helpers ────────────────────────────────────────────────

    fn make_peer(peer_id: &str, hostname: &str, status: &str, local: bool) -> PodPeerDto {
        match joined(peer_id, hostname, status, local) {
            PodMember::Joined(b) => *b,
            _ => unreachable!(),
        }
    }

    fn addr(kind: &str, value: &str) -> PodInstanceAddress {
        PodInstanceAddress {
            kind: kind.into(),
            kind_label: format!("k:{kind}"),
            value: value.into(),
        }
    }

    #[test]
    fn reachable_addrs_v4_only() {
        let a = vec![addr("lan_v4", "10.0.0.5")];
        let r = reachable_addrs("host", &a, None, 12000, "system", "10.0.0.5:12000");
        assert_eq!(r, vec!["10.0.0.5:12000"]);
    }

    #[test]
    fn reachable_addrs_v6_only() {
        let a = vec![addr("lan_v6", "fe80::1")];
        let r = reachable_addrs("host", &a, None, 12000, "system", "[fe80::1]:12000");
        assert_eq!(r, vec!["[fe80::1]:12000"]);
    }

    #[test]
    fn reachable_addrs_both_v4_and_v6() {
        let a = vec![addr("lan_v4", "10.0.0.5"), addr("lan_v6", "fe80::1")];
        let r = reachable_addrs("host", &a, None, 12000, "system", "10.0.0.5:12000");
        assert_eq!(r, vec!["10.0.0.5:12000", "[fe80::1]:12000"]);
    }

    #[test]
    fn reachable_addrs_fqdn_fallback() {
        let sys = system::system_info_types::SystemInfoReport {
            fqdn: Some("host.lan".into()),
            ..Default::default()
        };
        let r = reachable_addrs("host", &[], Some(&sys), 12000, "system", "");
        assert_eq!(r, vec!["host.lan:12000"]);
    }

    #[test]
    fn reachable_addrs_hostname_fallback_when_label_not_ip() {
        let r = reachable_addrs("myhost", &[], None, 12000, "system", "");
        assert_eq!(r, vec!["myhost:12000"]);
    }

    #[test]
    fn reachable_addrs_origin_fallback_when_label_is_ip() {
        let r = reachable_addrs("10.0.0.5", &[], None, 12000, "system", "10.0.0.5:12000");
        assert_eq!(r, vec!["10.0.0.5:12000"]);
    }

    #[test]
    fn reachable_addrs_origin_fallback_for_local_role() {
        let r = reachable_addrs("hostname", &[], None, 12000, "local", "http://x");
        assert_eq!(r, vec!["http://x"]);
    }

    #[test]
    fn build_instance_local_role_and_origin_empty() {
        let p = make_peer("peer.self", "myhost", "active", true);
        let inst = build_instance(&p, true, 1000);
        assert_eq!(inst.role, "local");
        // Locality lives in `role`, not the id: the real machine id is carried,
        // and `id`/`peer_id` never collapse to a masked "local".
        assert_eq!(inst.id, "local:peer.self");
        assert_eq!(inst.peer_id, "peer.self");
        assert_ne!(inst.id, inst.peer_id);
        assert_eq!(inst.origin, "");
        assert!(inst.secure.is_none());
        // Local health defaults to "unknown" — frontend overlays the real
        // value from /api/health.
        assert_eq!(inst.health, "unknown");
    }

    #[test]
    fn build_instance_system_role_health_from_status() {
        let p = make_peer("peer.a", "ha", "active", false);
        let inst = build_instance(&p, false, 1000);
        assert_eq!(inst.role, "system");
        assert_eq!(inst.id, "system:peer.a");
        assert_eq!(inst.peer_id, "peer.a");
        assert_eq!(inst.health, "up");
        assert_eq!(inst.origin, "10.0.0.1:7777");
        assert!(inst.secure.is_some());
        assert_eq!(inst.status.as_deref(), Some("active"));
    }

    #[test]
    fn build_instance_addresses_projected_with_kind_label() {
        let mut p = make_peer("peer.a", "ha", "active", false);
        p.addresses.push(PodPeerAddressDto {
            kind: "lan_v4".into(),
            kind_label: "LAN IPv4".into(),
            value: "10.0.0.7".into(),
            source: "mdns".into(),
            last_seen_at: 0,
        });
        let inst = build_instance(&p, false, 1000);
        assert_eq!(inst.addresses.len(), 1);
        assert_eq!(inst.addresses[0].kind, "lan_v4");
        assert_eq!(inst.addresses[0].kind_label, "LAN IPv4");
        assert_eq!(inst.addresses[0].value, "10.0.0.7");
    }
}
