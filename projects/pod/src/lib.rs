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

/// Crate-wide serialization lock for tests that repoint the process-global
/// `HOME` (and thus `pki_dir()` / `~/.orca`). Every module previously kept its
/// own module-private `ENV_LOCK`, which does NOT serialize across modules — a
/// `roster_sync` test could race a `cert_rotation` or `cli` test under the
/// threaded `cargo test` harness (nextest isolates per process and is immune).
/// All HOME-mutating tests in this crate must hold THIS single lock.
#[cfg(test)]
pub(crate) static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

// ── Args / Output types (shared by every surface) ────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

pub use utils::route::{Route, Routes};

/// Stamp the server-owned `kind_label` onto a mesh [`Route`] so every surface
/// renders identical channel text without re-implementing the switch per
/// client. The single label-stamping step reused by the local-row builder and
/// the peer-DTO shaping.
pub(crate) fn labeled(mut route: Route) -> Route {
    route.kind_label = Some(system::system_info::labels::addr_kind_label(&route.kind));
    route
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPeerDto {
    pub peer_id: String,
    pub hostname: String,
    /// Legacy single dial address. No longer serialized — every address now
    /// lives in `addresses` as an equal channel (there is no privileged
    /// "primary" address). Kept as a `serde(default)` inbound field so rosters
    /// from pre-collapse peers (which still send `addr`) still deserialize, and
    /// as an internal fallback until every peer advertises a full `addresses`
    /// snapshot. The shaping layer folds this value into `addresses` so nothing
    /// is lost when it stops being serialized.
    #[serde(default, skip_serializing)]
    pub addr: String,
    pub port: u16,
    pub last_seen_at: i64,
    pub local_secure: bool,
    pub peer_secure: bool,
    /// "active" | "departed".
    pub status: String,
    /// Multi-channel routes (LAN v4/v6, Tailscale, FQDN, …) as shared
    /// [`Route`]s. May be empty for peers paired before slice 4 of the
    /// host-addressing plan landed.
    #[serde(default)]
    pub routes: Routes,
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
    /// Lean topology facts (hostname/type/cluster/virt/macs/claims/primary IPs)
    /// projected from the peer's `system.detail`. Drives parent-inference,
    /// cluster grouping, and host-card rendering without the fat host snapshot
    /// (which lives on `system.info.detail`). `None` when the probe failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<system::system::TopologyFacts>,
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
    /// (when probed) ping latency + topology facts. Boxed because the joined
    /// row carries more fields than the other variants; without the
    /// indirection the whole enum pays that size on every row.
    Joined(Box<PodPeerDto>),
    /// Pending inbound or outbound offer — pairing handshake in progress.
    Handshaking(PodPendingOfferDto),
    /// mDNS-discovered orca that is not yet paired.
    Discovered(PodDiscoveryRowDto),
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PodListArgs {
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
    /// Return the pre-classified `snapshot` rollup (members + candidates +
    /// stale + inbound offers + clusters) instead of the thin paged roster.
    #[arg(long)]
    pub snapshot: bool,
    /// Return the fully-shaped `PodInstance` roster for the systems UI instead
    /// of the thin paged roster. Wins over `snapshot` when both are set.
    #[arg(long)]
    pub instances: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodListOutput {
    pub members: Vec<PodMember>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total rows across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// Shape returned by `pod.list`. Defaults to the thin paged roster
/// ([`PodListOutput`]); the `snapshot`/`instances` flags fold the former
/// `pod.snapshot` / `pod.instances` rollups into this one verb. Untagged so the
/// default roster shape stays wire-identical for existing consumers
/// (roster-sync deserializes [`PodListOutput`] directly).
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PodListResult {
    Snapshot(Box<PodSnapshotOutput>),
    Instances(Box<PodInstancesOutput>),
    List(PodListOutput),
}

// ── pod.snapshot — pre-classified one-shot rollup for the systems UI ─────────
//
// `pod.list` returns raw mesh state; the frontend then re-implements peer/
// candidate/stale/inbound-offer classification + cluster grouping in JS. That
// logic moves here so every surface gets the same shaped view and the systems
// page collapses from ~2000 lines to a thin renderer. The original JS
// source-of-truth (`refreshPodPeers` + `refreshProxmoxClusters`) lived in the
// in-repo frontend, since extracted to the peacock plugin (argyle-labs/peacock).

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
/// `pod.detail`) call this instead of duplicating the resolver.
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
        for a in &p.routes {
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
    pub system: Option<system::system::TopologyFacts>,

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
    // Hard rule: ids are bare UUIDv7, never prefixed. Locality is carried
    // on the separate `role` field, so `id` and `peer_id` are simply the
    // machine's UUIDv7 — the same value the tree/roster exposes as a
    // selector, round-trippable with no `local:`/`system:` decoration.
    let peer_id = p.peer_id.clone();
    let id = p.peer_id.clone();
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
        .routes
        .iter()
        .map(|a| PodInstanceAddress {
            kind: a.kind.clone(),
            kind_label: a.kind_label.clone().unwrap_or_default(),
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
    sys: Option<&system::system::TopologyFacts>,
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

/// Pairing path for `pod.create`.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PodCreateAction {
    /// Dial the inviter directly and auto-accept (needs `addr`).
    #[default]
    Join,
    /// Push an offer to an mDNS-discovered joiner (needs `addr`).
    Offer,
    /// Complete an out-of-band offer by its printed code (needs `code`).
    Accept,
}

/// Union of args across the three pairing paths. Required fields are validated
/// per `action` at dispatch (`join`/`offer` need `addr`; `accept` needs `code`).
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PodCreateArgs {
    /// `join` (default), `offer`, or `accept`.
    #[arg(long, default_value = "join")]
    pub action: PodCreateAction,
    /// (join/offer) Address to dial: host or `host:port`.
    #[arg(long)]
    pub addr: Option<String>,
    /// (join/offer) Override the mesh port. Defaults to `APP_PLUGIN_PORT`.
    #[arg(long)]
    pub port: Option<u16>,
    /// (accept) 6-char pairing code printed on the inviter's CLI.
    #[arg(long)]
    pub code: Option<String>,
}

/// Tagged result of `pod.create`: `join`/`accept` return the membership
/// accept payload; `offer` returns the minted pairing code.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PodCreateOutput {
    Accept(PodAcceptOutput),
    Offer(PodOfferOutput),
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

// ── pod ping transport ───────────────────────────────────────────────────────

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

// ── pod.delete (kick / leave / forget) ───────────────────────────────────────

/// Target selector for `pod.delete`.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PodDeleteAction {
    /// Evict a paired peer (needs `peer_id`).
    #[default]
    Kick,
    /// Voluntary self exit. LOCAL-ONLY.
    Leave,
    /// Hard-delete a stale/orphan peer mesh-wide (needs `peer_id`).
    Forget,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PodDeleteArgs {
    /// `kick` (default), `leave`, or `forget`.
    #[arg(long, default_value = "kick")]
    pub action: PodDeleteAction,
    /// (kick/forget) Peer to remove.
    #[arg(long)]
    pub peer_id: Option<String>,
}

/// Tagged result of `pod.delete`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PodDeleteOutput {
    Kick(PodLeaveOutput),
    Leave(PodLeaveSelfOutput),
    Forget(PodForgetOutput),
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

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodRecoverOutput {
    pub peer_id: String,
    /// `true` if a `departed_at` flag was actually cleared. `false` means the
    /// peer either wasn't departed or doesn't exist locally.
    pub cleared: bool,
}

// ── pod.forget ───────────────────────────────────────────────────────────────

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
    /// Running orca version of the host this detail describes. For a
    /// peer-dispatched (`--peer`) call this is the *remote* host's version,
    /// since the handler executes on that host — making `pod certs --peer <h>`
    /// the canonical way to read a peer's version.
    #[serde(default)]
    pub version: String,
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

// ── pod.update (settings / trust / sync / recover / cancel_offer) ────────────

/// Operation selector for `pod.update`.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PodUpdateAction {
    /// Toggle `self_secure` (Tier-2 secrets-storage). Default.
    #[default]
    Settings,
    /// Set trust for a paired peer (needs `peer_id` + `on`).
    Trust,
    /// Force a one-shot replication tick (optional `peer` filter).
    Sync,
    /// Clear a stale `departed_at` flag on THIS host (needs `peer_id`).
    /// LOCAL-ONLY.
    Recover,
    /// Clear stuck outbound pairing offer(s) for `addr`.
    CancelOffer,
}

/// Union of args across the pod-update operations; required fields are
/// validated per `action` at dispatch.
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PodUpdateArgs {
    /// `settings` (default), `trust`, `sync`, `recover`, or `cancel_offer`.
    #[arg(long, default_value = "settings")]
    pub action: PodUpdateAction,
    /// (settings) Toggle Tier-2 secrets-storage permission. `None` leaves the
    /// current value unchanged.
    #[arg(long)]
    pub self_secure: Option<bool>,
    /// (trust/recover) Target peer.
    #[arg(long)]
    pub peer_id: Option<String>,
    /// (trust) New trust value.
    #[arg(long, action = clap::ArgAction::Set)]
    pub on: Option<bool>,
    /// (trust) Execute on the remote peer so THEY trust US.
    #[arg(long)]
    pub push: bool,
    /// (sync) Optional source-peer filter (hostname / peer_id / addr).
    #[arg(long)]
    pub peer: Option<String>,
    /// (cancel_offer) Joiner address whose outbound offer(s) to clear.
    #[arg(long)]
    pub addr: Option<String>,
}

/// Result of `pod.update action=settings`.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodSettingsOutput {
    pub self_secure: bool,
}

/// Tagged result of `pod.update`, one variant per action.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PodUpdateOutput {
    Settings(PodSettingsOutput),
    Trust(PodTrustOutput),
    Sync(PodSyncOutput),
    Recover(PodRecoverOutput),
    CancelOffer(PodCancelOfferOutput),
}

// ── DTO conversions + wire-dispatch types ───────────────────────────────────

mod dto_conversions {
    use super::*;

    impl From<db::pod::PeerSummary> for PodPeerDto {
        fn from(p: db::pod::PeerSummary) -> Self {
            let mut routes: Routes = p.routes.into_iter().map(crate::labeled).collect();
            // Fold the legacy single addr into the channel list so it isn't
            // lost now that `addr` is no longer serialized. Skip if any channel
            // already carries the value (dedup by value across all kinds, which
            // is looser than `Routes::push`'s per-(kind, value) dedup).
            if !p.addr.is_empty() && !routes.iter().any(|a| a.value == p.addr) {
                routes.push(crate::labeled(Route::learned(
                    "legacy",
                    p.addr.clone(),
                    "peer_addr",
                    p.last_seen_at,
                )));
            }
            Self {
                peer_id: p.peer_id,
                hostname: p.hostname,
                addr: p.addr,
                port: p.port,
                last_seen_at: p.last_seen_at,
                local_secure: p.local_secure,
                peer_secure: p.peer_secure,
                status: p.status,
                routes,
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
        // Trait method (RemoteExec): force-refresh the peer's write-through
        // system.detail cache so the next pod.list reflects a just-applied
        // update without waiting out the TTL.
        crate::peer_info::peer_detail(peer, true).await?;
        Ok(())
    }
}

// ── Tools ───────────────────────────────────────────────────────────────────

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

    let own_key = system::host_identity::machine_id();
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    claimed.insert(own_key.to_string());
    for p in &joined {
        claimed.insert(p.peer_id.clone());
    }
    let discovered: Vec<_> = discovered
        .into_iter()
        .filter(|d| {
            d.peer_id
                .as_deref()
                .is_none_or(|pid| !claimed.contains(pid))
        })
        .collect();

    let mut members = Vec::with_capacity(joined.len() + handshaking.len() + discovered.len());
    // Every id is a canonical uuidv7 (the legacy `peer.`/`unclaimed.` prefix
    // forms are retired), so ids are compared and surfaced as-is — no key
    // collapse.
    members.extend(
        joined
            .into_iter()
            .filter(|p| p.local || p.peer_id != own_key)
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
async fn pod_list(args: PodListArgs, ctx: &contract::ToolCtx) -> anyhow::Result<PodListResult> {
    // The former `pod.snapshot` / `pod.instances` verbs fold into `pod.list` as
    // query flags — one roster verb, richer shapes on demand.
    if args.instances {
        return Ok(PodListResult::Instances(Box::new(
            collect_pod_instances().await?,
        )));
    }
    if args.snapshot {
        return Ok(PodListResult::Snapshot(Box::new(
            collect_pod_snapshot(ctx).await?,
        )));
    }
    // `pod.list` is THE thin systems roster: identity + addressing from the
    // cached `pod_peers` row, plus a LIVE-LITE per-host probe for
    // reachability + version/channel. The controller caches NOTHING about
    // another host's telemetry — a failed probe leaves those fields absent and
    // `reachable = false`, never a stale mirror value. The heavy
    // `SystemInfoReport` (~85 KB/host) is NOT fetched here; it lives on
    // `system.detail`, and the fat classified candidate/stale/inbound-offer
    // view lives on `pod.snapshot` / `pod.instances`.
    let mut members: Vec<PodMember> = server_pod::list_lite()
        .await?
        .into_iter()
        .map(|p| PodMember::Joined(Box::new(p)))
        .collect();
    // Stable, deterministic order before paginating: group by state, then by id.
    members.sort_by_key(member_sort_key);
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(members, &params);
    Ok(PodListResult::List(PodListOutput {
        members: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    }))
}

/// Deterministic sort key for a [`PodMember`]: `(state ordinal, identity)`.
fn member_sort_key(m: &PodMember) -> (u8, String) {
    match m {
        PodMember::Joined(p) => (0, p.peer_id.clone()),
        PodMember::Handshaking(o) => (1, o.offer_id.clone()),
        PodMember::Discovered(d) => (2, d.peer_id.clone().unwrap_or_else(|| d.pubkey_fp.clone())),
    }
}

/// Pre-classified rollup of pod state for the systems UI. Same `members`
/// payload as `pod.list`, plus candidate / stale / inbound-offer
/// classification and cluster-membership matching computed server-side
/// so every surface gets one shaped response instead of re-implementing
/// the rules per client.
pub async fn collect_pod_snapshot(ctx: &contract::ToolCtx) -> anyhow::Result<PodSnapshotOutput> {
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
            peer_id: system::host_identity::machine_id().to_string(),
            hostname: system::host_identity::hostname().to_string(),
            addr: String::new(),
            port: 12000,
            last_seen_at: 0,
            local_secure: false,
            peer_secure: false,
            status: "active".into(),
            routes: Routes::new(),
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

/// Establish pod membership. `action` selects the pairing path:
///   - `join`   — dial the inviter DIRECTLY over the bootstrap channel (no mDNS
///     required) and auto-accept in one call (needs `addr`, optional `port`).
///   - `offer`  — push a membership offer to a joiner discovered via mDNS
///     (needs `addr`, optional `port`); returns a pairing code to show the
///     operator.
///   - `accept` — complete an out-of-band offer by its 6-char code (needs
///     `code`).
#[orca_tool(domain = "pod", verb = "create")]
async fn pod_create(
    args: PodCreateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PodCreateOutput> {
    match args.action {
        PodCreateAction::Join => {
            let addr = args
                .addr
                .ok_or_else(|| anyhow::anyhow!("pod.create action=join requires `addr`"))?;
            Ok(PodCreateOutput::Accept(
                server_pod::join(&addr, args.port).await?,
            ))
        }
        PodCreateAction::Offer => {
            let addr = args
                .addr
                .ok_or_else(|| anyhow::anyhow!("pod.create action=offer requires `addr`"))?;
            Ok(PodCreateOutput::Offer(
                server_pod::offer(&addr, args.port).await?,
            ))
        }
        PodCreateAction::Accept => {
            let code = args
                .code
                .ok_or_else(|| anyhow::anyhow!("pod.create action=accept requires `code`"))?;
            Ok(PodCreateOutput::Accept(server_pod::accept(&code).await?))
        }
    }
}

/// Mutate pod state on this host (or a `--peer` target). `action` selects the
/// operation:
///   - `settings`     — toggle `self_secure` (Tier-2 secrets-storage). Default.
///   - `trust`        — set trust for a paired peer (needs `peer_id` + `on`;
///     `push` flips THEIR trust of us over mTLS).
///   - `sync`         — force a one-shot replication tick (optional `peer`
///     source filter).
///   - `recover`      — clear a stale `departed_at` flag on THIS host (needs
///     `peer_id`). LOCAL-ONLY: rejected for remote callers by the pod listener.
///   - `cancel_offer` — clear stuck outbound pairing offer(s) for `addr`.
#[orca_tool(domain = "pod", verb = "update", role = "admin")]
async fn pod_update(
    args: PodUpdateArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<PodUpdateOutput> {
    match args.action {
        PodUpdateAction::Settings => {
            let self_secure = match args.self_secure {
                Some(v) => server_pod::set_self_secure(v).await?,
                None => server_pod::get_self_secure()?,
            };
            Ok(PodUpdateOutput::Settings(PodSettingsOutput { self_secure }))
        }
        PodUpdateAction::Trust => {
            let peer_id = args
                .peer_id
                .ok_or_else(|| anyhow::anyhow!("pod.update action=trust requires `peer_id`"))?;
            let on = args
                .on
                .ok_or_else(|| anyhow::anyhow!("pod.update action=trust requires `on`"))?;
            let out = if args.push {
                server_pod::push_trust(&peer_id, on, ctx.caller()).await?
            } else {
                server_pod::trust(&peer_id, on).await?
            };
            Ok(PodUpdateOutput::Trust(out))
        }
        PodUpdateAction::Sync => {
            let reports = db::replicate_engine::sync_now(args.peer.as_deref()).await?;
            Ok(PodUpdateOutput::Sync(PodSyncOutput { peers: reports }))
        }
        PodUpdateAction::Recover => {
            let peer_id = args
                .peer_id
                .ok_or_else(|| anyhow::anyhow!("pod.update action=recover requires `peer_id`"))?;
            Ok(PodUpdateOutput::Recover(server_pod::recover(&peer_id)?))
        }
        PodUpdateAction::CancelOffer => {
            let addr = args
                .addr
                .ok_or_else(|| anyhow::anyhow!("pod.update action=cancel_offer requires `addr`"))?;
            let rows_removed = server_pod::cancel_offer(&addr)?;
            Ok(PodUpdateOutput::CancelOffer(PodCancelOfferOutput {
                addr,
                rows_removed,
            }))
        }
    }
}

/// Remove pod membership. `action` selects the target:
///   - `kick`   — evict a paired peer: best-effort notify, then drop its
///     `pod_peers` + `pod_trust` rows (needs `peer_id`). Default.
///   - `leave`  — voluntary self exit: notify every paired peer, then drop all
///     `pod_peers` + `pod_trust` rows on this host. LOCAL-ONLY: rejected for
///     remote callers by the pod listener.
///   - `forget` — hard-delete a stale/orphan `peer_id` here AND fan a one-way
///     forget notice to every live member (needs `peer_id`).
#[orca_tool(domain = "pod", verb = "delete", role = "admin")]
async fn pod_delete(
    args: PodDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PodDeleteOutput> {
    match args.action {
        PodDeleteAction::Kick => {
            let peer_id = args
                .peer_id
                .ok_or_else(|| anyhow::anyhow!("pod.delete action=kick requires `peer_id`"))?;
            Ok(PodDeleteOutput::Kick(
                server_pod::leave_peer(&peer_id).await?,
            ))
        }
        PodDeleteAction::Leave => Ok(PodDeleteOutput::Leave(server_pod::leave_self().await?)),
        PodDeleteAction::Forget => {
            let peer_id = args
                .peer_id
                .ok_or_else(|| anyhow::anyhow!("pod.delete action=forget requires `peer_id`"))?;
            Ok(PodDeleteOutput::Forget(server_pod::forget(&peer_id).await?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learned_route_carries_source_and_stamped_label() {
        // The DB→Route path is `Route::learned`; the DTO edge stamps the label.
        let route = labeled(Route::learned("lan_v4", "10.0.0.5", "mdns", 42));
        assert_eq!(route.kind, "lan_v4");
        assert_eq!(route.value, "10.0.0.5");
        assert_eq!(route.source.as_deref(), Some("mdns"));
        assert_eq!(route.last_seen_at, Some(42));
        assert_eq!(route.kind_label.as_deref(), Some("LAN IPv4"));
        // Schemeless mesh route → not URL-addressable.
        assert!(route.base_url().is_none());
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
            routes: Routes::new(),
            pubkey_fp: None,
        };
        let dto: PodPeerDto = row.into();
        assert_eq!(dto.peer_id, "x");
        assert!(!dto.local);
        assert!(dto.reachable.is_none());
        assert!(dto.version.is_none());
        assert!(dto.system.is_none());
    }

    // ── migrate-never-wipe: mesh leaf identity/format reconcile ──────────────
    //
    // Regression backstop for the rc.20 incident: the leaf CN moved from the
    // short 12-hex machine-id to the full 32-hex machine_id, and the daemon
    // *wiped* its leaf + pod membership on the first restart, coming up
    // unpaired. The correct behaviour — asserted here — is to MIGRATE the leaf
    // in place from the existing CA and PRESERVE membership + trust.

    const OLD_SHORT_CN: &str = "24647a14a251";
    const NEW_FULL_CN: &str = "24647a14a251e863cdf8dcee692f2915";

    fn test_db() -> rusqlite::Connection {
        // Unencrypted on-disk temp DB with schema + migrations applied. Leaked
        // tempdir keeps the file alive for the connection's lifetime.
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        ::db::open_unencrypted(&dir.path().join("orca.db")).unwrap()
    }

    fn seed_membership(conn: &rusqlite::Connection) {
        db::pod::set_self_secure(conn, true).unwrap();
        db::pod::upsert_peer(
            conn,
            "019e7105-0000-7000-8000-0000000abc01",
            "willow",
            "peer.local",
            12002,
            Some("fp-abc"),
            "-----BEGIN CERTIFICATE-----\nfake\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
    }

    fn leaf_cn(pki_dir: &std::path::Path) -> String {
        let pem = std::fs::read_to_string(utils::pki::mesh_client_cert_path(pki_dir)).unwrap();
        utils::pki::cert_summary(&pem).unwrap().cn
    }

    /// The incident scenario: an on-disk leaf carrying the OLD short-CN format
    /// on a CA-holding node. Reconciling to the NEW full-CN format must MIGRATE
    /// the leaf in place and leave pod membership untouched — never unpaired.
    #[test]
    fn old_format_leaf_is_migrated_not_wiped() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        // Founder-style init under the OLD short CN — leaf + CA both present.
        utils::pki::init_mesh_ca(pki, OLD_SHORT_CN).unwrap();
        assert_eq!(leaf_cn(pki), OLD_SHORT_CN, "fixture: leaf starts on old CN");

        let conn = test_db();
        seed_membership(&conn);
        assert_eq!(db::pod::list_peers(&conn).unwrap().len(), 1);

        // Reconcile to the NEW full-CN format (as the rc.20 upgrade would).
        let outcome = reconcile_mesh_leaf_identity(pki, NEW_FULL_CN, &conn).unwrap();

        // MIGRATE, never wipe: leaf carries the new CN...
        assert_eq!(outcome, LeafReconcileOutcome::Migrated);
        assert_eq!(
            leaf_cn(pki),
            NEW_FULL_CN,
            "leaf must be re-issued under new CN"
        );
        // ...and the CA + server leaf are intact...
        assert!(utils::pki::has_mesh_ca_key(pki));
        assert!(utils::pki::mesh_server_cert_path(pki).exists());
        // ...and — the whole point — pod membership/trust SURVIVES.
        // Under the old wipe behaviour this row would be gone and the node
        // would come up unpaired.
        assert_eq!(
            db::pod::list_peers(&conn).unwrap().len(),
            1,
            "pod membership must survive a leaf format migration"
        );
    }

    /// A leaf already on the expected format is a no-op.
    #[test]
    fn current_format_leaf_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        utils::pki::init_mesh_ca(pki, NEW_FULL_CN).unwrap();
        let conn = test_db();
        seed_membership(&conn);

        let outcome = reconcile_mesh_leaf_identity(pki, NEW_FULL_CN, &conn).unwrap();
        assert_eq!(outcome, LeafReconcileOutcome::AlreadyCurrent);
        assert_eq!(leaf_cn(pki), NEW_FULL_CN);
        assert_eq!(db::pod::list_peers(&conn).unwrap().len(), 1);
    }

    /// A host with neither leaf nor CA was never enrolled — no-op, no wipe.
    #[test]
    fn unenrolled_host_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let conn = test_db();
        let outcome = reconcile_mesh_leaf_identity(dir.path(), NEW_FULL_CN, &conn).unwrap();
        assert_eq!(outcome, LeafReconcileOutcome::NotEnrolled);
    }

    /// The mint-controller incident (2026-08): a host that WAS enrolled (pod
    /// membership + self_secure in the DB) whose entire `pki/mesh/` subtree is
    /// gone — CA and leaves both absent, e.g. a reinstall displaced it into
    /// `.orca-trash`. This must NOT be misread as `NotEnrolled` (which left the
    /// daemon "paired but identity-less", failing every handshake with
    /// `no server certificate chain resolved`). It must reset stale membership
    /// and report `ResetUnpaired` so the host comes up ready to re-pair.
    #[test]
    fn enrolled_host_with_lost_mesh_material_resets_not_noop() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        // No mesh material at all on disk...
        assert!(!utils::pki::mesh_client_cert_path(pki).exists());
        assert!(!utils::pki::has_mesh_ca_key(pki));

        // ...but the DB still records prior enrollment.
        let conn = test_db();
        seed_membership(&conn);
        assert_eq!(db::pod::list_peers(&conn).unwrap().len(), 1);

        let outcome = reconcile_mesh_leaf_identity(pki, NEW_FULL_CN, &conn).unwrap();
        assert_eq!(
            outcome,
            LeafReconcileOutcome::ResetUnpaired,
            "enrolled-but-material-lost must reset, not silently no-op"
        );
        assert_eq!(
            db::pod::list_peers(&conn).unwrap().len(),
            0,
            "stale membership is cleared so the daemon comes up ready to re-pair"
        );
    }

    /// Last resort only: a drifted leaf whose CA key is genuinely absent cannot
    /// be migrated, so it resets. This is the sole path allowed to drop
    /// membership — and even then it comes up ready to re-pair, not dead.
    #[test]
    fn drifted_leaf_without_ca_resets_as_last_resort() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        // Build an old-CN leaf, then remove the CA key so migration is
        // impossible (a joiner-only host that somehow lost its CA).
        utils::pki::init_mesh_ca(pki, OLD_SHORT_CN).unwrap();
        _ = std::fs::remove_file(utils::pki::mesh_ca_key_path(pki));
        assert!(!utils::pki::has_mesh_ca_key(pki));

        let conn = test_db();
        seed_membership(&conn);

        let outcome = reconcile_mesh_leaf_identity(pki, NEW_FULL_CN, &conn).unwrap();
        assert_eq!(outcome, LeafReconcileOutcome::ResetUnpaired);
        assert_eq!(
            db::pod::list_peers(&conn).unwrap().len(),
            0,
            "last-resort reset clears membership (re-pair follows)"
        );
    }

    /// A CA-holding host that has LOST its client leaf (file removed) but still
    /// holds the CA key must re-issue the leaf in place under the expected CN —
    /// the `LeafState::Absent && has_ca` migration branch — without touching
    /// membership. Distinct from the drifted-CN path: here there is no leaf to
    /// read at all.
    #[test]
    fn absent_leaf_with_ca_is_reissued_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        utils::pki::init_mesh_ca(pki, NEW_FULL_CN).unwrap();
        // Delete the client leaf so the on-disk state is Absent, but keep the
        // CA key (migration is possible).
        _ = std::fs::remove_file(utils::pki::mesh_client_cert_path(pki));
        assert!(!utils::pki::mesh_client_cert_path(pki).exists());
        assert!(utils::pki::has_mesh_ca_key(pki));

        let conn = test_db();
        seed_membership(&conn);

        let outcome = reconcile_mesh_leaf_identity(pki, NEW_FULL_CN, &conn).unwrap();
        assert_eq!(outcome, LeafReconcileOutcome::Migrated);
        // The leaf was re-minted under the expected CN from the local CA...
        assert_eq!(leaf_cn(pki), NEW_FULL_CN);
        // ...and membership survived — a missing leaf is never a reason to wipe.
        assert_eq!(db::pod::list_peers(&conn).unwrap().len(), 1);
    }

    /// An unreadable (corrupt) leaf on a CA-holding host is treated as drifted
    /// and re-issued in place — exercising the `None =>` (unparseable CN)
    /// classification branch, which is distinct from a readable-but-mismatched
    /// CN. Membership is preserved.
    #[test]
    fn unreadable_leaf_with_ca_is_migrated_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        utils::pki::init_mesh_ca(pki, NEW_FULL_CN).unwrap();
        // Corrupt the client leaf so its CN cannot be parsed (not valid PEM).
        std::fs::write(
            utils::pki::mesh_client_cert_path(pki),
            b"not a certificate at all",
        )
        .unwrap();

        let conn = test_db();
        seed_membership(&conn);

        let outcome = reconcile_mesh_leaf_identity(pki, NEW_FULL_CN, &conn).unwrap();
        assert_eq!(outcome, LeafReconcileOutcome::Migrated);
        assert_eq!(
            leaf_cn(pki),
            NEW_FULL_CN,
            "corrupt leaf must be re-issued under the expected CN"
        );
        assert_eq!(db::pod::list_peers(&conn).unwrap().len(), 1);
    }
}

// ── mesh networking: mTLS dials, PKI, bootstrap signing, pod-wire methods ──
mod bootstrap;
pub mod caller_token;
pub mod cert_rotation;
pub mod dialer;
pub mod dispatcher;
mod listener;
pub mod mdns;
pub mod mesh_listener;
pub mod peer_info;
pub mod roster_sync;
pub mod route_health;
pub mod scheduler;
pub mod subscribe;
pub mod subscribe_demand;
pub mod subscribe_wire;
pub mod transport;

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
/// `lan_v6`, `tailscale_v4`, `tailscale_v6`, `fqdn`). Source + last_seen_at
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
/// the daemon. Delegates to the canonical `$ORCA_HOME`-aware resolver so an
/// alternate instance (or an isolated test) that sets `$ORCA_HOME` is
/// honored. A bare `$HOME` override alone previously left mesh PKI pointing
/// at the real `~/.orca`, so a daemon spawned with an isolated `$ORCA_HOME`
/// could still rotate the live mesh certs.
pub fn pki_dir() -> PathBuf {
    if let Ok(dir) = contract::config::paths::pki_dir() {
        return dir;
    }
    // Sealed environments (neither `$ORCA_HOME` nor `$HOME` set) fall back to
    // the legacy `$HOME`-relative layout.
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(APP_STATE_DIR).join(APP_PKI_DIR)
}

/// Outcome of [`reconcile_mesh_leaf_identity`]. Distinguishes the paths so
/// callers (and tests) can assert on *how* identity was brought into line,
/// not merely that something changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafReconcileOutcome {
    /// On-disk leaf already matched the expected identity/format. No-op.
    AlreadyCurrent,
    /// No leaf and no CA — this host has never been part of a mesh. No-op.
    NotEnrolled,
    /// The leaf drifted from the expected identity/format but the host still
    /// holds its CA key, so a corrected leaf was re-issued **in place** and
    /// pod membership + peer trust were fully preserved. No pairing loss.
    Migrated,
    /// Migration was genuinely impossible (CA key absent, so no way to
    /// re-issue a trusted leaf). Cert material was reset and membership wiped;
    /// the daemon will attempt to re-pair. This is the last resort and is
    /// logged loudly — it should never happen on a CA-holding host.
    ResetUnpaired,
}

/// Reconcile the on-disk mesh leaf to the expected identity/format —
/// **migrating in place, never wiping**, whenever migration is possible.
///
/// This is the single hook for mesh identity/format changes. The motivating
/// case: rc.20 changed the leaf CN from the 12-hex short machine-id to the
/// full 32-hex `machine_id`. Older behaviour *deleted* the leaf and wiped
/// `pod_peers/pod_trust/pod_pending_offers/pod_discovery`, so the node came
/// up unpaired and unreachable over mTLS — even though the shared mesh CA and
/// the host keypair were intact the whole time, i.e. everything needed to
/// re-issue a correct leaf without losing pairing.
///
/// Policy — **migrate, never wipe**:
/// - Leaf CN matches `expected_cn` → [`LeafReconcileOutcome::AlreadyCurrent`].
/// - Leaf drifted (or is unreadable/missing) **and this host holds the CA
///   key** → re-issue client+server leaves under `expected_cn` from the
///   existing CA and **preserve pod membership + trust**
///   → [`LeafReconcileOutcome::Migrated`]. This is the path every current
///   node hits on a format bump; no pairing is ever lost.
/// - No leaf and no CA **and no pod membership** → genuinely pre-pod host
///   → [`LeafReconcileOutcome::NotEnrolled`].
/// - No leaf and no CA **but pod membership rows exist** → the host was
///   enrolled and lost its mesh material entirely (e.g. a reinstall displaced
///   `pki/mesh/`). Cannot re-mint without the CA key; reset stale membership
///   and come up ready to re-pair → [`LeafReconcileOutcome::ResetUnpaired`].
///   Logged loudly. Without this, the daemon comes up "paired but
///   identity-less" and every handshake fails `no server certificate chain
///   resolved`.
/// - Leaf drifted but the CA key is genuinely absent → migration is
///   impossible; reset cert material + membership and come up ready to
///   re-pair → [`LeafReconcileOutcome::ResetUnpaired`]. Logged loudly.
///
/// Future identity/format migrations extend this function (e.g. add cases to
/// the CN/format check) so the class stays "always migrate, never wipe".
///
/// `expected_cn` is the CN the leaf *should* carry (normally
/// `machine_id()`); `conn` is the membership DB. Split out from
/// [`reset_if_stale_mesh_identity`] so it is deterministic and unit-testable
/// against a tempdir CA + in-memory DB.
pub fn reconcile_mesh_leaf_identity(
    pki_dir: &std::path::Path,
    expected_cn: &str,
    conn: &rusqlite::Connection,
) -> Result<LeafReconcileOutcome> {
    let cert_path = utils::pki::mesh_client_cert_path(pki_dir);

    // Classify the on-disk leaf.
    //   Current  – present, CN matches expected. Nothing to do.
    //   Drifted  – present but CN mismatched OR unreadable. Needs migration.
    //   Absent   – no leaf on disk.
    enum LeafState {
        Current,
        Drifted,
        Absent,
    }
    let leaf = if cert_path.exists() {
        match std::fs::read_to_string(&cert_path)
            .ok()
            .and_then(|pem| {
                rustls_pemfile::certs(&mut pem.as_bytes())
                    .next()
                    .and_then(Result::ok)
            })
            .and_then(|der| utils::pki::peer_common_name(&der).ok())
        {
            Some(cn) if cn == expected_cn => LeafState::Current,
            Some(cn) => {
                tracing::warn!(
                    "[pod] mesh leaf CN {cn:?} does not match expected {expected_cn:?} — \
                     migrating leaf in place (re-issuing under current format; \
                     pod membership + peer trust preserved)."
                );
                LeafState::Drifted
            }
            None => {
                tracing::warn!(
                    "[pod] mesh leaf at {} is unreadable — treating as drifted and \
                     re-issuing in place",
                    cert_path.display()
                );
                LeafState::Drifted
            }
        }
    } else {
        LeafState::Absent
    };

    if matches!(leaf, LeafState::Current) {
        return Ok(LeafReconcileOutcome::AlreadyCurrent);
    }

    let has_ca = utils::pki::has_mesh_ca_key(pki_dir);

    // MIGRATE-IN-PLACE. As long as this host holds the CA key it can mint a
    // trusted leaf under the expected format, so there is never a reason to
    // destroy pairing. Re-issue both leaves and leave `pod_peers/pod_trust`
    // untouched. Atomic writes swap the leaf files under the same paths.
    if has_ca {
        if matches!(leaf, LeafState::Absent) {
            tracing::warn!(
                "[pod] mesh leaf missing but this host holds the CA key — \
                 re-issuing client+server leaves from the local CA \
                 (pod membership preserved)."
            );
        }
        utils::pki::reissue_mesh_server_cert(pki_dir)
            .context("migrate mesh leaf: re-issue server cert")?;
        utils::pki::reissue_mesh_client_cert(pki_dir, expected_cn)
            .context("migrate mesh leaf: re-issue client cert")?;
        // Founder identity: keep self marked secure. Membership rows are left
        // exactly as they were — this is the whole point of migrate-not-wipe.
        db::pod::set_self_secure(conn, true)?;
        tracing::info!(
            "[pod] mesh leaf migrated in place under CN {expected_cn:?}; \
             pod membership + peer trust preserved (no re-pair needed)."
        );
        return Ok(LeafReconcileOutcome::Migrated);
    }

    // No leaf and no CA. Two very different situations share this on-disk shape,
    // and the DB membership state is what tells them apart:
    //   (a) Truly never enrolled — no `self_secure` marker and no peer rows.
    //       Nothing to migrate, nothing to reset → `NotEnrolled`.
    //   (b) Enrolled before, but the mesh material (CA + leaves) is entirely
    //       gone while pod membership rows survive in the DB — e.g. a reinstall
    //       displaced `pki/mesh/` into `.orca-trash`. Silently returning
    //       `NotEnrolled` here leaves the daemon "paired but identity-less": it
    //       can present no mesh server cert, so every handshake fails with
    //       `no server certificate chain resolved` and the host is dead but
    //       silent (the reconcile ran and reported "nothing to do"). Take the
    //       last-resort reset so it comes up ready to re-pair instead.
    if matches!(leaf, LeafState::Absent) {
        let was_enrolled = db::pod::get_self_secure(conn).unwrap_or(false)
            || !db::pod::list_peer_summaries(conn)?.is_empty();
        if !was_enrolled {
            return Ok(LeafReconcileOutcome::NotEnrolled);
        }
        tracing::error!(
            "[pod] mesh material (CA + leaves) is ABSENT but pod membership rows \
             exist — this host was enrolled and has lost its mesh identity (e.g. \
             a reinstall displaced pki/mesh/ into .orca-trash). The CA key is \
             gone, so a trusted leaf cannot be re-minted here; resetting stale \
             membership so the daemon comes up unpaired and can re-pair via \
             `orca pod join <inviter>` or an mDNS auto-offer. This should never \
             happen while pki/mesh/ is intact."
        );
        db::pod::wipe_pod_membership(conn)?;
        return Ok(LeafReconcileOutcome::ResetUnpaired);
    }

    // LAST RESORT. The leaf drifted but the CA key is genuinely absent, so we
    // cannot mint a trusted replacement here. Reset cert material + membership
    // and come up ready to re-pair rather than dead. This must be loud: on a
    // healthy CA-holding node it should never be reached.
    tracing::error!(
        "[pod] mesh leaf drifted from expected format but the mesh CA key is \
         ABSENT on this host — cannot migrate in place. Resetting mesh cert \
         material and pod membership; daemon will come up unpaired and must \
         re-pair via `orca pod join <inviter>` or an mDNS auto-offer. This is \
         a last-resort path and indicates the CA was not present when it \
         should have been."
    );
    let mesh = utils::pki::mesh_dir(pki_dir);
    for sub in ["client", "server"] {
        let d = mesh.join(sub);
        if d.exists() {
            _ = std::fs::remove_dir_all(&d);
        }
    }
    db::pod::wipe_pod_membership(conn)?;
    Ok(LeafReconcileOutcome::ResetUnpaired)
}

/// Startup entry point: reconcile the on-disk mesh leaf to this host's current
/// identity/format, **migrating in place** wherever possible (see
/// [`reconcile_mesh_leaf_identity`] for the full policy). Wires the pure
/// reconcile logic to the live `machine_id()` and default DB.
///
/// Returns `Ok(true)` if the on-disk state was changed (migrated or, in the
/// last-resort case, reset). Best-effort: any error is surfaced to the caller,
/// which logs at warn and proceeds with startup.
pub fn reset_if_stale_mesh_identity(pki_dir: &std::path::Path) -> Result<bool> {
    let expected = system::host_identity::machine_id().to_string();
    let conn = ::db::open_default()?;
    let outcome = reconcile_mesh_leaf_identity(pki_dir, &expected, &conn)?;
    Ok(!matches!(
        outcome,
        LeafReconcileOutcome::AlreadyCurrent | LeafReconcileOutcome::NotEnrolled
    ))
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

/// Route-aware [`exec`] BY PEER IDENTITY: resolve the peer's ordered dial
/// targets (every addressing channel, route-health ranked) and try them in
/// order, exactly as [`server_pod::ping`] does — never a single hard-coded
/// address. This is the seam every by-peer RPC should use so a multi-homed peer
/// stays reachable when one channel is down. Callers pass a `peer_id`, not an
/// `addr`; identity is the address ([[machine-multi-homed-addresses-not-identity]]).
#[allow(clippy::disallowed_types)]
pub async fn exec_peer(
    peer_id: &str,
    tool: &str,
    args: serde_json::Value,
) -> Result<PodExecResult> {
    let conn = db::open_default()?;
    let peer = db::pod::list_peers(&conn)?
        .into_iter()
        .find(|p| p.peer_id == peer_id)
        .ok_or_else(|| anyhow::anyhow!("no such peer: {peer_id}"))?;
    let targets = crate::dialer::dial_targets_for_peer(&conn, peer_id, &peer.peer_addr)
        .unwrap_or_else(|_| vec![peer.peer_addr.clone()]);
    drop(conn);
    crate::dialer::try_targets_tracked(Some(peer_id), &targets, |t| {
        let tool = tool.to_string();
        let args = args.clone();
        async move { exec(&t, &tool, args).await }
    })
    .await
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
            routes: Routes::new(),
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

    #[test]
    fn joined_member_omits_top_level_addr() {
        // The collapse: `addr` is never serialized — every address is an equal
        // channel in `addresses`. Guards against a regression that reintroduces
        // a privileged top-level address on the roster row.
        let PodMember::Joined(dto) = joined("p1", "h", "active", false) else {
            unreachable!()
        };
        let v = serde_json::to_value(&*dto).unwrap();
        assert!(
            v.get("addr").is_none(),
            "roster row must not serialize a top-level addr, got: {v}"
        );
        // And it still round-trips a legacy inbound `addr` (serde default).
        let back: PodPeerDto =
            serde_json::from_value(serde_json::json!({"peer_id":"p1","hostname":"h","addr":"10.0.0.9","port":7777,"last_seen_at":0,"local_secure":false,"peer_secure":false,"status":"active"})).unwrap();
        assert_eq!(back.addr, "10.0.0.9");
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
            joined("self", "myhost", "active", true),
            discovered("fp1", Some("x"), "MyHost", "unclaimed"),
            discovered("fp2", Some("y"), "other", "unclaimed"),
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
            joined("a", "ha", "active", false),
            discovered("fp", Some("a"), "ha", "unclaimed"),
        ];
        let (_m, candidates, _s, _o) = classify_snapshot(members, 0);
        assert!(candidates.is_empty());
    }

    #[test]
    fn stale_includes_inactive_joined_as_departed() {
        let members = vec![joined("gone", "gone", "departed", false)];
        let (_m, _c, stale, _o) = classify_snapshot(members, 0);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].reason, "departed");
        assert_eq!(stale[0].peer_id, "gone");
    }

    #[test]
    fn stale_includes_orphan_discovered_with_peer_id() {
        // Non-unclaimed discovery row with a peer_id but no matching joined.
        let members = vec![discovered("fp", Some("orph"), "host", "pod:other")];
        let (_m, candidates, stale, _o) = classify_snapshot(members, 0);
        assert!(candidates.is_empty());
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].reason, "orphan");
    }

    #[test]
    fn match_clusters_ip_first_then_hostname() {
        let mut p_ip = match joined("byip", "ignored", "active", false) {
            PodMember::Joined(b) => *b,
            _ => unreachable!(),
        };
        p_ip.routes
            .push(labeled(Route::learned("lan_v4", "10.0.0.99", "test", 0)));
        let p_host = match joined("byname", "node-b", "active", false) {
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
        assert_eq!(m.get("byip").map(String::as_str), Some("alpha"));
        assert_eq!(m.get("byname").map(String::as_str), Some("alpha"));
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
        let sys = system::system::TopologyFacts {
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
        let p = make_peer(
            "019e7105-0000-7000-8000-000000000001",
            "myhost",
            "active",
            true,
        );
        let inst = build_instance(&p, true, 1000);
        assert_eq!(inst.role, "local");
        // Hard rule: the id is the bare peer_id UUIDv7, never prefixed.
        // Locality lives in `role`, so `id` == `peer_id`.
        assert_eq!(inst.id, "019e7105-0000-7000-8000-000000000001");
        assert_eq!(inst.peer_id, "019e7105-0000-7000-8000-000000000001");
        assert_eq!(inst.id, inst.peer_id);
        assert_eq!(inst.origin, "");
        assert!(inst.secure.is_none());
        // Local health defaults to "unknown" — frontend overlays the real
        // value from /api/health.
        assert_eq!(inst.health, "unknown");
    }

    #[test]
    fn build_instance_system_role_health_from_status() {
        let p = make_peer(
            "019e7105-0000-7000-8000-00000000000a",
            "ha",
            "active",
            false,
        );
        let inst = build_instance(&p, false, 1000);
        assert_eq!(inst.role, "system");
        assert_eq!(inst.id, "019e7105-0000-7000-8000-00000000000a");
        assert_eq!(inst.peer_id, "019e7105-0000-7000-8000-00000000000a");
        assert_eq!(inst.health, "up");
        assert_eq!(inst.origin, "10.0.0.1:7777");
        assert!(inst.secure.is_some());
        assert_eq!(inst.status.as_deref(), Some("active"));
    }

    #[test]
    fn build_instance_addresses_projected_with_kind_label() {
        let mut p = make_peer("a", "ha", "active", false);
        p.routes
            .push(labeled(Route::learned("lan_v4", "10.0.0.7", "mdns", 0)));
        let inst = build_instance(&p, false, 1000);
        assert_eq!(inst.addresses.len(), 1);
        assert_eq!(inst.addresses[0].kind, "lan_v4");
        assert_eq!(inst.addresses[0].kind_label, "LAN IPv4");
        assert_eq!(inst.addresses[0].value, "10.0.0.7");
    }
}

#[cfg(test)]
mod added_coverage {
    //! Extra pure/deterministic coverage: serde shapes, dispatch enums,
    //! classification edge branches, cluster matching on the `PodInstance`
    //! projection, and the DB→DTO conversion. No network / DB / subprocess.
    use super::*;

    // ── helpers (independent from the other test modules) ────────────────────

    fn peer(peer_id: &str, hostname: &str, status: &str, local: bool) -> PodPeerDto {
        PodPeerDto {
            peer_id: peer_id.into(),
            hostname: hostname.into(),
            addr: "10.0.0.1".into(),
            port: 7777,
            last_seen_at: 0,
            local_secure: true,
            peer_secure: true,
            status: status.into(),
            routes: Routes::new(),
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
        }
    }

    // ── labeled() over more kinds ────────────────────────────────────────────

    #[test]
    fn labeled_stamps_known_and_unknown_kinds() {
        let ts = labeled(Route::learned("tailscale_v4", "100.64.0.1", "test", 0));
        assert_eq!(ts.kind_label.as_deref(), Some("Tailscale IPv4"));
        let fq = labeled(Route::learned("fqdn", "host.lan", "test", 0));
        assert_eq!(fq.kind_label.as_deref(), Some("FQDN"));
        // Unknown kinds pass through untranslated but are still stamped Some.
        let wg = labeled(Route::learned("wireguard_v4", "10.9.9.9", "test", 0));
        assert_eq!(wg.kind_label.as_deref(), Some("wireguard_v4"));
    }

    // ── dto From<PeerSummary> — legacy addr fold + dedup ─────────────────────

    fn summary(addr: &str, routes: Routes) -> db::pod::PeerSummary {
        db::pod::PeerSummary {
            peer_id: "p".into(),
            hostname: "h".into(),
            addr: addr.into(),
            port: 12002,
            last_seen_at: 7,
            local_secure: false,
            peer_secure: true,
            status: "active".into(),
            routes,
            pubkey_fp: Some("fp".into()),
        }
    }

    #[test]
    fn from_summary_folds_legacy_addr_into_routes_as_channel() {
        let dto: PodPeerDto = summary("1.2.3.4", Routes::new()).into();
        // The legacy single addr becomes a "legacy" channel so it isn't lost
        // now that `addr` is no longer serialized.
        assert!(dto.routes.iter().any(|r| r.value == "1.2.3.4"));
        let legacy = dto.routes.iter().find(|r| r.value == "1.2.3.4").unwrap();
        assert_eq!(legacy.kind, "legacy");
        assert!(legacy.kind_label.is_some(), "folded route must be stamped");
        assert_eq!(dto.pubkey_fp.as_deref(), Some("fp"));
        assert!(dto.peer_secure);
    }

    #[test]
    fn from_summary_skips_legacy_fold_when_value_already_present() {
        let mut routes = Routes::new();
        routes.push(labeled(Route::learned("lan_v4", "1.2.3.4", "mdns", 0)));
        let dto: PodPeerDto = summary("1.2.3.4", routes).into();
        // No duplicate "legacy" channel for a value an existing route carries.
        assert_eq!(
            dto.routes.iter().filter(|r| r.value == "1.2.3.4").count(),
            1
        );
        assert!(!dto.routes.iter().any(|r| r.kind == "legacy"));
    }

    #[test]
    fn from_summary_empty_addr_adds_no_legacy_route() {
        let dto: PodPeerDto = summary("", Routes::new()).into();
        assert!(dto.routes.is_empty());
        assert_eq!(dto.addr, "");
    }

    // ── member_sort_key — state ordinal + identity ordering ───────────────────

    #[test]
    fn member_sort_key_orders_by_state_then_identity() {
        let j = PodMember::Joined(Box::new(peer("z", "h", "active", false)));
        let h = PodMember::Handshaking(PodPendingOfferDto {
            offer_id: "off".into(),
            direction: "inbound".into(),
            peer_pubkey_fp: "fp".into(),
            peer_hostname: "h".into(),
            peer_addr: "10.0.0.3".into(),
            peer_port: 7777,
            inviter_peer_id: None,
            pod_id: None,
            expires_at: 0,
            ttl_secs: 0,
            created_at: 0,
        });
        let d = PodMember::Discovered(PodDiscoveryRowDto {
            pubkey_fp: "fpd".into(),
            peer_id: None,
            hostname: "h".into(),
            addr: "10.0.0.4".into(),
            port: 7777,
            discovery_state: "unclaimed".into(),
            can_invite: true,
            first_seen_at: 0,
            last_seen_at: 0,
        });
        assert_eq!(member_sort_key(&j), (0, "z".to_string()));
        assert_eq!(member_sort_key(&h), (1, "off".to_string()));
        // Discovered with no peer_id falls back to the pubkey fingerprint.
        assert_eq!(member_sort_key(&d), (2, "fpd".to_string()));
    }

    // ── classify_snapshot — additional branches ──────────────────────────────

    #[test]
    fn classify_no_local_row_treats_unclaimed_as_candidate() {
        // With no local joined row, own_hostname is empty so nothing is a
        // self-echo — an unclaimed discovery becomes a candidate.
        let members = vec![PodMember::Discovered(PodDiscoveryRowDto {
            pubkey_fp: "fp".into(),
            peer_id: Some("x".into()),
            hostname: "anyhost".into(),
            addr: "10.0.0.2".into(),
            port: 7777,
            discovery_state: "unclaimed".into(),
            can_invite: false,
            first_seen_at: 0,
            last_seen_at: 9,
        })];
        let (_m, candidates, stale, _o) = classify_snapshot(members, 0);
        assert_eq!(candidates.len(), 1);
        assert!(!candidates[0].can_invite);
        assert!(stale.is_empty());
    }

    #[test]
    fn classify_non_unclaimed_discovery_without_peer_id_is_skipped() {
        // Claimed (pod:*) discovery row with NO peer_id is neither a candidate
        // nor a stale row — it silently drops.
        let members = vec![PodMember::Discovered(PodDiscoveryRowDto {
            pubkey_fp: "fp".into(),
            peer_id: None,
            hostname: "host".into(),
            addr: "10.0.0.2".into(),
            port: 7777,
            discovery_state: "pod:other".into(),
            can_invite: false,
            first_seen_at: 0,
            last_seen_at: 0,
        })];
        let (_m, candidates, stale, _o) = classify_snapshot(members, 0);
        assert!(candidates.is_empty());
        assert!(stale.is_empty());
    }

    #[test]
    fn classify_departed_uses_peer_id_when_hostname_empty() {
        let members = vec![PodMember::Joined(Box::new(peer(
            "pid", "", "departed", false,
        )))];
        let (_m, _c, stale, _o) = classify_snapshot(members, 0);
        assert_eq!(stale.len(), 1);
        // Empty hostname falls back to the peer_id for the display label.
        assert_eq!(stale[0].hostname, "pid");
    }

    #[test]
    fn classify_local_departed_row_is_not_stale() {
        // The local row is never classified as departed even when inactive.
        let members = vec![PodMember::Joined(Box::new(peer(
            "me", "myhost", "departed", true,
        )))];
        let (_m, _c, stale, _o) = classify_snapshot(members, 0);
        assert!(stale.is_empty());
    }

    // ── build_instance — down health + update flags ──────────────────────────

    #[test]
    fn build_instance_down_health_for_inactive_remote() {
        let inst = build_instance(&peer("p", "h", "departed", false), false, 5);
        assert_eq!(inst.health, "down");
        assert_eq!(inst.last_checked, Some(5));
        assert!(!inst.update_available);
    }

    #[test]
    fn build_instance_carries_update_and_meta_fields() {
        let mut p = peer("p", "h", "active", false);
        p.update_available = Some(true);
        p.update_latest = Some("0.9.0".into());
        p.version = Some("0.8.0".into());
        p.channel = Some("beta".into());
        p.update_checked_secs = Some(120);
        let inst = build_instance(&p, false, 0);
        assert!(inst.update_available);
        assert_eq!(inst.update_latest.as_deref(), Some("0.9.0"));
        assert_eq!(inst.version.as_deref(), Some("0.8.0"));
        assert_eq!(inst.channel.as_deref(), Some("beta"));
        assert_eq!(inst.update_checked_secs, Some(120));
        assert!(inst.available_versions.is_empty());
    }

    // ── reachable_addrs — system primary_ipv4 fallback ───────────────────────

    #[test]
    fn reachable_addrs_uses_system_primary_ipv4_when_no_lan_address() {
        let sys = system::system::TopologyFacts {
            primary_ipv4: Some("192.168.1.9".into()),
            ..Default::default()
        };
        let r = reachable_addrs("host", &[], Some(&sys), 12000, "system", "");
        assert_eq!(r, vec!["192.168.1.9:12000"]);
    }

    // ── match_clusters_instances — parity resolver on PodInstance ─────────────

    fn instance_with_routes(peer_id: &str, hostname: &str, ip: Option<&str>) -> PodInstance {
        let mut p = peer(peer_id, hostname, "active", false);
        if let Some(ip) = ip {
            p.routes
                .push(labeled(Route::learned("lan_v4", ip, "test", 0)));
        }
        build_instance(&p, false, 0)
    }

    fn cluster(name: &str, node: &str, ip: Option<&str>) -> contract::ClusterEntry {
        contract::ClusterEntry {
            endpoint: "ep".into(),
            name: Some(name.into()),
            quorate: Some(true),
            nodes: vec![contract::ClusterNode {
                name: node.into(),
                ip: ip.map(|s| s.into()),
                online: Some(true),
            }],
        }
    }

    #[test]
    fn match_clusters_instances_ip_first() {
        let instances = vec![instance_with_routes("byip", "ignored", Some("10.0.0.50"))];
        let clusters = vec![cluster("alpha", "node-a", Some("10.0.0.50"))];
        let m = match_clusters_instances(&instances, &clusters);
        assert_eq!(m.get("byip").map(String::as_str), Some("alpha"));
    }

    #[test]
    fn match_clusters_instances_hostname_fallback_via_label() {
        // No address hit, no system facts: fall back to lowercased label.
        let instances = vec![instance_with_routes("byname", "Node-B", None)];
        let clusters = vec![cluster("beta", "node-b", None)];
        let m = match_clusters_instances(&instances, &clusters);
        assert_eq!(m.get("byname").map(String::as_str), Some("beta"));
    }

    #[test]
    fn match_clusters_instances_no_match_is_absent() {
        let instances = vec![instance_with_routes("lonely", "nowhere", Some("10.0.0.1"))];
        let clusters = vec![cluster("gamma", "node-z", Some("10.0.0.99"))];
        let m = match_clusters_instances(&instances, &clusters);
        assert!(m.is_empty());
    }

    #[test]
    fn match_clusters_instances_skips_unnamed_cluster() {
        let instances = vec![instance_with_routes("byip", "h", Some("10.0.0.50"))];
        let clusters = vec![contract::ClusterEntry {
            endpoint: "ep".into(),
            name: None,
            quorate: None,
            nodes: vec![contract::ClusterNode {
                name: "node-a".into(),
                ip: Some("10.0.0.50".into()),
                online: None,
            }],
        }];
        let m = match_clusters_instances(&instances, &clusters);
        assert!(m.is_empty(), "unnamed clusters contribute no membership");
    }

    // ── serde: PodMember state tag ───────────────────────────────────────────

    #[test]
    fn pod_member_serializes_state_discriminant() {
        let j = serde_json::to_string(&PodMember::Joined(Box::new(peer(
            "p", "h", "active", false,
        ))))
        .unwrap();
        assert!(j.contains("\"state\":\"joined\""), "got: {j}");
        let d = serde_json::to_string(&PodMember::Discovered(PodDiscoveryRowDto {
            pubkey_fp: "fp".into(),
            peer_id: None,
            hostname: "h".into(),
            addr: "10.0.0.2".into(),
            port: 1,
            discovery_state: "unclaimed".into(),
            can_invite: true,
            first_seen_at: 0,
            last_seen_at: 0,
        }))
        .unwrap();
        assert!(d.contains("\"state\":\"discovered\""), "got: {d}");
        // `discovery_state` must NOT be renamed to the reserved `state` key.
        assert!(d.contains("\"discovery_state\":\"unclaimed\""), "got: {d}");
    }

    // ── serde: dispatch action enums (snake_case) ────────────────────────────

    #[test]
    fn action_enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&PodCreateAction::Join).unwrap(),
            "\"join\""
        );
        assert_eq!(
            serde_json::to_string(&PodCreateAction::Offer).unwrap(),
            "\"offer\""
        );
        assert_eq!(
            serde_json::to_string(&PodCreateAction::Accept).unwrap(),
            "\"accept\""
        );
        assert_eq!(
            serde_json::to_string(&PodDeleteAction::Kick).unwrap(),
            "\"kick\""
        );
        assert_eq!(
            serde_json::to_string(&PodDeleteAction::Leave).unwrap(),
            "\"leave\""
        );
        assert_eq!(
            serde_json::to_string(&PodDeleteAction::Forget).unwrap(),
            "\"forget\""
        );
        assert_eq!(
            serde_json::to_string(&PodUpdateAction::Settings).unwrap(),
            "\"settings\""
        );
        assert_eq!(
            serde_json::to_string(&PodUpdateAction::CancelOffer).unwrap(),
            "\"cancel_offer\""
        );
    }

    #[test]
    fn action_enums_default_and_roundtrip() {
        assert_eq!(PodCreateAction::default(), PodCreateAction::Join);
        assert_eq!(PodDeleteAction::default(), PodDeleteAction::Kick);
        assert_eq!(PodUpdateAction::default(), PodUpdateAction::Settings);
        let a: PodUpdateAction = serde_json::from_str("\"cancel_offer\"").unwrap();
        assert_eq!(a, PodUpdateAction::CancelOffer);
    }

    // ── serde: args defaults + camelCase ─────────────────────────────────────

    #[test]
    fn pod_list_args_default_and_camel_case() {
        let d = PodListArgs::default();
        assert!(d.limit.is_none() && d.cursor.is_none() && !d.snapshot && !d.instances);
        let a: PodListArgs =
            serde_json::from_str(r#"{"limit":10,"cursor":"c1","snapshot":true}"#).unwrap();
        assert_eq!(a.limit, Some(10));
        assert_eq!(a.cursor.as_deref(), Some("c1"));
        assert!(a.snapshot && !a.instances);
    }

    #[test]
    fn pod_update_args_deserializes_camel_case_self_secure() {
        let a: PodUpdateArgs =
            serde_json::from_str(r#"{"action":"trust","peerId":"p","on":true,"push":true}"#)
                .unwrap();
        assert_eq!(a.action, PodUpdateAction::Trust);
        assert_eq!(a.peer_id.as_deref(), Some("p"));
        assert_eq!(a.on, Some(true));
        assert!(a.push);
    }

    // ── serde: skip_serializing_if on rollup rows ────────────────────────────

    #[test]
    fn pod_candidate_omits_none_peer_id() {
        let c = PodCandidate {
            pubkey_fp: "fp".into(),
            peer_id: None,
            hostname: "h".into(),
            addr: "1.2.3.4".into(),
            port: 1,
            can_invite: true,
        };
        let s = serde_json::to_string(&c).unwrap();
        assert!(!s.contains("peer_id"), "None peer_id must be skipped: {s}");
    }

    #[test]
    fn pod_stale_row_omits_none_last_seen() {
        let s = serde_json::to_string(&PodStaleRow {
            peer_id: "p".into(),
            hostname: "h".into(),
            addr: "1.2.3.4".into(),
            port: 1,
            reason: "orphan".into(),
            last_seen_at: None,
        })
        .unwrap();
        assert!(!s.contains("last_seen_at"), "got: {s}");
    }

    #[test]
    fn pod_inbound_offer_omits_none_inviter() {
        let s = serde_json::to_string(&PodInboundOffer {
            offer_id: "o".into(),
            peer_hostname: "h".into(),
            peer_addr: "1.2.3.4".into(),
            peer_port: 1,
            inviter_peer_id: None,
            expires_at: 10,
            ttl_secs: 5,
        })
        .unwrap();
        assert!(!s.contains("inviter_peer_id"), "got: {s}");
    }

    // ── serde: transparent newtype wrappers ──────────────────────────────────

    #[test]
    fn discovery_list_output_is_transparent_array() {
        let out = PodDiscoveryListOutput(vec![PodDiscoveryRowDto {
            pubkey_fp: "fp".into(),
            peer_id: None,
            hostname: "h".into(),
            addr: "1.2.3.4".into(),
            port: 1,
            discovery_state: "unclaimed".into(),
            can_invite: true,
            first_seen_at: 0,
            last_seen_at: 0,
        }]);
        let s = serde_json::to_string(&out).unwrap();
        assert!(
            s.starts_with('['),
            "transparent wrapper serializes as array: {s}"
        );
    }

    #[test]
    fn pending_list_output_is_transparent_array() {
        let out = PodPendingListOutput(Vec::new());
        assert_eq!(serde_json::to_string(&out).unwrap(), "[]");
    }

    // ── serde: untagged result enums pick the inner shape ────────────────────

    #[test]
    fn pod_create_output_untagged_offer_and_accept() {
        let offer = PodCreateOutput::Offer(PodOfferOutput {
            code: "ABC123".into(),
            joiner_hostname: "h".into(),
            joiner_addr: "1.2.3.4".into(),
            joiner_port: 1,
            joiner_pubkey_fp: "fp".into(),
            offer_id: "o".into(),
            expires_at: 99,
        });
        let s = serde_json::to_string(&offer).unwrap();
        assert!(s.contains("\"code\":\"ABC123\""), "got: {s}");
        // Untagged: no variant discriminant leaks onto the wire.
        assert!(
            !s.contains("Offer"),
            "untagged must not emit variant name: {s}"
        );
    }

    #[test]
    fn pod_delete_output_untagged_leave() {
        let out = PodDeleteOutput::Leave(PodLeaveSelfOutput {
            rows_removed: 3,
            peers: vec![PodLeaveSelfResult {
                peer_id: "p".into(),
                notify_result: "notified".into(),
            }],
        });
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains("\"rows_removed\":3"), "got: {s}");
    }

    #[test]
    fn pod_update_output_untagged_settings() {
        let out = PodUpdateOutput::Settings(PodSettingsOutput { self_secure: true });
        assert_eq!(
            serde_json::to_string(&out).unwrap(),
            r#"{"self_secure":true}"#
        );
    }

    // ── serde: cert-status defaults + skip ───────────────────────────────────

    #[test]
    fn cert_status_defaults_version_and_self_secure() {
        let out: PodCertStatusOutput =
            serde_json::from_str(r#"{"founder":true,"member":false}"#).unwrap();
        assert!(out.founder && !out.member);
        assert_eq!(out.version, "");
        assert!(!out.self_secure);
        assert!(out.mesh_ca.is_none() && out.bootstrap.is_none());
    }

    #[test]
    fn cert_info_roundtrips() {
        let ci = CertInfo {
            cn: "host".into(),
            fingerprint: "ab:cd".into(),
            issued_at: 1,
            expires_at: 2,
            days_remaining: 30,
        };
        let s = serde_json::to_string(&ci).unwrap();
        let back: CertInfo = serde_json::from_str(&s).unwrap();
        assert_eq!(back.cn, "host");
        assert_eq!(back.days_remaining, 30);
    }

    // ── serde: mesh wire result tolerance ────────────────────────────────────

    #[test]
    fn address_channel_defaults_kind_label() {
        let c: AddressChannel =
            serde_json::from_str(r#"{"kind":"lan_v4","value":"10.0.0.1"}"#).unwrap();
        assert_eq!(c.kind, "lan_v4");
        assert_eq!(c.kind_label, "", "missing label defaults to empty");
    }

    #[test]
    fn dev_sync_result_defaults_optional_fields() {
        let r: PodDevSyncResult = serde_json::from_str(r#"{"status":"skipped"}"#).unwrap();
        assert_eq!(r.status, "skipped");
        assert!(r.detail.is_none() && r.commits_pulled.is_none());
    }

    #[test]
    fn dev_enable_and_disable_results_default_fields() {
        let e: PodDevEnableResult = serde_json::from_str(r#"{"status":"enabled"}"#).unwrap();
        assert_eq!(e.status, "enabled");
        assert!(e.repo_path.is_none() && e.cloned.is_none() && e.daemon_parked.is_none());
        let d: PodDevDisableResult = serde_json::from_str(r#"{"status":"disabled"}"#).unwrap();
        assert_eq!(d.status, "disabled");
        assert!(d.dev_process_stopped.is_none() && d.daemon_reclaimed.is_none());
    }

    #[test]
    fn exec_params_default_optional_fields() {
        let p: PodExecParams = serde_json::from_str(r#"{"tool":"pod.list"}"#).unwrap();
        assert_eq!(p.tool, "pod.list");
        assert!(p.caller_role.is_none() && p.caller_token.is_none());
        assert!(p.correlation_id.is_none());
    }

    #[test]
    fn replicate_push_and_roots_results_roundtrip() {
        let pr: ReplicatePushResult = serde_json::from_str(r#"{"merged":7}"#).unwrap();
        assert_eq!(pr.merged, 7);
        let rr: ReplicateRootsResult =
            serde_json::from_str(r#"{"roots":{"users":"deadbeef"}}"#).unwrap();
        assert_eq!(rr.roots.get("users").map(String::as_str), Some("deadbeef"));
    }

    // ── outcome enum semantics used by reset_if_stale wrapper ────────────────

    #[test]
    fn leaf_reconcile_outcome_equality() {
        assert_eq!(
            LeafReconcileOutcome::Migrated,
            LeafReconcileOutcome::Migrated
        );
        assert_ne!(
            LeafReconcileOutcome::AlreadyCurrent,
            LeafReconcileOutcome::NotEnrolled
        );
    }

    // ── pki_dir composition ──────────────────────────────────────────────────

    #[test]
    fn pki_dir_ends_with_state_and_pki_components() {
        let p = pki_dir();
        assert!(p.ends_with(std::path::Path::new(APP_STATE_DIR).join(APP_PKI_DIR)));
    }

    // ── build_instance: empty-hostname label fallback ────────────────────────

    #[test]
    fn build_instance_empty_hostname_falls_back_to_peer_id_label() {
        let p = peer("019e7105-0000-7000-8000-0000000000ff", "", "active", false);
        let inst = build_instance(&p, false, 0);
        assert_eq!(inst.label, "019e7105-0000-7000-8000-0000000000ff");
    }

    // ── match_clusters: system.primary_ipv4 fallback (no route hit) ───────────

    #[test]
    fn match_clusters_falls_back_to_system_primary_ipv4() {
        let mut p = peer("bysys", "unmatched-host", "active", false);
        p.system = Some(system::system::TopologyFacts {
            primary_ipv4: Some("10.7.7.7".into()),
            ..Default::default()
        });
        let members = vec![PodMember::Joined(Box::new(p))];
        let clusters = vec![contract::ClusterEntry {
            endpoint: "ep".into(),
            name: Some("alpha".into()),
            quorate: Some(true),
            nodes: vec![contract::ClusterNode {
                name: "node-a".into(),
                ip: Some("10.7.7.7".into()),
                online: Some(true),
            }],
        }];
        let m = match_clusters(&members, &clusters);
        assert_eq!(m.get("bysys").map(String::as_str), Some("alpha"));
    }

    // ── match_clusters: unnamed clusters contribute nothing ──────────────────

    #[test]
    fn match_clusters_skips_unnamed_cluster() {
        let mut p = peer("byip", "h", "active", false);
        p.routes
            .push(labeled(Route::learned("lan_v4", "10.0.0.50", "test", 0)));
        let members = vec![PodMember::Joined(Box::new(p))];
        let clusters = vec![contract::ClusterEntry {
            endpoint: "ep".into(),
            name: None,
            quorate: None,
            nodes: vec![contract::ClusterNode {
                name: "node-a".into(),
                ip: Some("10.0.0.50".into()),
                online: None,
            }],
        }];
        let m = match_clusters(&members, &clusters);
        assert!(m.is_empty(), "unnamed clusters yield no membership");
    }

    // ── match_clusters_instances: system.primary_ipv4 fallback ───────────────

    #[test]
    fn match_clusters_instances_falls_back_to_system_primary_ipv4() {
        let mut p = peer("bysys", "unmatched-host", "active", false);
        p.system = Some(system::system::TopologyFacts {
            primary_ipv4: Some("10.8.8.8".into()),
            ..Default::default()
        });
        let instances = vec![build_instance(&p, false, 0)];
        let clusters = vec![cluster("beta", "node-b", Some("10.8.8.8"))];
        let m = match_clusters_instances(&instances, &clusters);
        assert_eq!(m.get("bysys").map(String::as_str), Some("beta"));
    }

    // ── reachable_addrs: v6 via system.primary_ipv6 fallback ─────────────────

    #[test]
    fn reachable_addrs_uses_system_primary_ipv6_when_no_lan_address() {
        // No lan_v4/lan_v6 channels and no primary_ipv4 — the only reachable
        // channel is the system-reported primary_ipv6, bracketed per URL rules.
        let sys = system::system::TopologyFacts {
            primary_ipv6: Some("fd00::9".into()),
            ..Default::default()
        };
        let r = reachable_addrs("host", &[], Some(&sys), 12000, "system", "");
        assert_eq!(r, vec!["[fd00::9]:12000"]);
    }

    #[test]
    fn reachable_addrs_v4_and_v6_both_from_system() {
        let sys = system::system::TopologyFacts {
            primary_ipv4: Some("10.1.1.1".into()),
            primary_ipv6: Some("fd00::1".into()),
            ..Default::default()
        };
        let r = reachable_addrs("host", &[], Some(&sys), 8080, "system", "");
        assert_eq!(r, vec!["10.1.1.1:8080", "[fd00::1]:8080"]);
    }

    // ── reachable_addrs: empty fqdn is skipped, falls through to label ────────

    #[test]
    fn reachable_addrs_empty_fqdn_is_skipped_falls_to_label() {
        // fqdn present but empty must NOT produce a ":port" address — the code
        // guards `!fqdn.is_empty()`, so a non-IP label wins next.
        let sys = system::system::TopologyFacts {
            fqdn: Some(String::new()),
            ..Default::default()
        };
        let r = reachable_addrs("namedhost", &[], Some(&sys), 12000, "system", "orig");
        assert_eq!(r, vec!["namedhost:12000"]);
    }

    #[test]
    fn reachable_addrs_empty_label_falls_to_origin() {
        // Non-local role but an empty label: the `!label.is_empty()` guard fails
        // so we fall through to the origin fallback.
        let r = reachable_addrs("", &[], None, 12000, "system", "the-origin");
        assert_eq!(r, vec!["the-origin"]);
    }

    // ── match_clusters: system.hostname wins over the peer's own hostname ─────

    #[test]
    fn match_clusters_uses_system_hostname_over_peer_hostname() {
        // No route/IP hit; the system-reported hostname (not the peer row's
        // hostname) is what resolves the cluster node.
        let mut p = peer("byhost", "wrong-hostname", "active", false);
        p.system = Some(system::system::TopologyFacts {
            hostname: Some("Real-Node".into()),
            ..Default::default()
        });
        let members = vec![PodMember::Joined(Box::new(p))];
        let clusters = vec![contract::ClusterEntry {
            endpoint: "ep".into(),
            name: Some("alpha".into()),
            quorate: Some(true),
            nodes: vec![contract::ClusterNode {
                name: "real-node".into(),
                ip: None,
                online: Some(true),
            }],
        }];
        let m = match_clusters(&members, &clusters);
        assert_eq!(m.get("byhost").map(String::as_str), Some("alpha"));
    }

    #[test]
    fn match_clusters_instances_uses_system_hostname_over_label() {
        let mut p = peer("byhost", "wrong-hostname", "active", false);
        p.system = Some(system::system::TopologyFacts {
            hostname: Some("Real-Node".into()),
            ..Default::default()
        });
        let instances = vec![build_instance(&p, false, 0)];
        let clusters = vec![cluster("beta", "real-node", None)];
        let m = match_clusters_instances(&instances, &clusters);
        assert_eq!(m.get("byhost").map(String::as_str), Some("beta"));
    }

    // ── build_instance: reachable_addrs is derived from the lan_v4 route ──────

    #[test]
    fn build_instance_populates_reachable_addrs_from_route() {
        let mut p = peer("p", "h", "active", false);
        p.port = 9000;
        p.routes
            .push(labeled(Route::learned("lan_v4", "10.2.2.2", "mdns", 0)));
        let inst = build_instance(&p, false, 0);
        assert_eq!(inst.reachable_addrs, vec!["10.2.2.2:9000"]);
    }

    // ── serde: HostAddressingSnapshot / AddressChannel round-trip ─────────────

    #[test]
    fn host_addressing_snapshot_roundtrips_channels() {
        let snap = HostAddressingSnapshot {
            display_name: "greg".into(),
            channels: vec![AddressChannel {
                kind: "lan_v4".into(),
                kind_label: "LAN IPv4".into(),
                value: "10.0.0.3".into(),
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: HostAddressingSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.display_name, "greg");
        assert_eq!(back.channels.len(), 1);
        assert_eq!(back.channels[0].kind, "lan_v4");
        assert_eq!(back.channels[0].value, "10.0.0.3");
    }

    #[test]
    fn address_channel_kind_label_defaults_when_absent() {
        // rc.≤25 peers omit `kind_label`; the `#[serde(default)]` fills "".
        let ch: AddressChannel =
            serde_json::from_str(r#"{"kind":"fqdn","value":"host.lan"}"#).unwrap();
        assert_eq!(ch.kind, "fqdn");
        assert_eq!(ch.value, "host.lan");
        assert_eq!(ch.kind_label, "");
    }

    // ── serde: PodPeerDto never serializes the legacy top-level addr ──────────

    #[test]
    fn pod_peer_dto_skips_top_level_addr_on_serialize() {
        let p = peer("p", "h", "active", false);
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            !json.contains("\"addr\""),
            "legacy addr must not serialize: {json}"
        );
    }

    // ── serde: PodListResult untagged List variant is the thin roster ─────────

    #[test]
    fn pod_list_result_untagged_list_is_wire_identical_to_output() {
        let out = PodListOutput {
            members: vec![PodMember::Joined(Box::new(peer("p", "h", "active", false)))],
            next_cursor: Some("cur".into()),
            total: Some(1),
        };
        let direct = serde_json::to_value(&out).unwrap();
        let wrapped = serde_json::to_value(PodListResult::List(out)).unwrap();
        assert_eq!(direct, wrapped);
    }
}

#[cfg(test)]
mod handler_dispatch_tests {
    //! Coverage for the `pod.create` / `pod.update` / `pod.delete` dispatch
    //! bodies plus the `collect_pod_instances` / `collect_pod_snapshot` roll-up
    //! projections. The per-action missing-argument guards short-circuit before
    //! any DB or network access, so they run deterministically without a ctx
    //! that touches state. The DB-backed arms (`settings`, `recover`,
    //! `cancel_offer`, `forget`, `leave`) run against an ephemeral migrated
    //! SQLite via `with_db_path`; none of them reach real mesh PKI or a live
    //! daemon (all remote fan-out targets are unresolved/loopback and fail fast
    //! into the best-effort warn arm).
    use super::*;
    use contract::ToolCtx;
    use contract::config::{Config, Model};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn empty_ctx() -> ToolCtx {
        ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/orca-pod-handler-test.db"),
            ports: Default::default(),
        }))
    }

    fn tmp_db() -> tempfile::NamedTempFile {
        tempfile::NamedTempFile::new().unwrap()
    }

    /// Extract the error from a `Result` whose `Ok` variant does not implement
    /// `Debug` (the tagged output enums are serde-only), so guard tests can
    /// assert on the message without an `unwrap_err` Debug bound.
    fn expect_err<T>(r: anyhow::Result<T>) -> anyhow::Error {
        match r {
            Ok(_) => panic!("expected an error, got Ok"),
            Err(e) => e,
        }
    }

    // ── member_sort_key: one tuple per PodMember variant ─────────────────────

    #[test]
    fn member_sort_key_orders_variants_and_carries_identity() {
        let j = PodMember::Joined(Box::new(PodPeerDto {
            peer_id: "peer-j".into(),
            hostname: "hj".into(),
            addr: "10.0.0.1".into(),
            port: 7777,
            last_seen_at: 0,
            local_secure: false,
            peer_secure: false,
            status: "active".into(),
            routes: Routes::new(),
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
            pubkey_fp: None,
        }));
        assert_eq!(member_sort_key(&j), (0, "peer-j".to_string()));

        let h = PodMember::Handshaking(PodPendingOfferDto {
            offer_id: "off-h".into(),
            direction: "in".into(),
            peer_pubkey_fp: "fp".into(),
            peer_hostname: "hh".into(),
            peer_addr: "10.0.0.2".into(),
            peer_port: 7777,
            inviter_peer_id: None,
            pod_id: None,
            expires_at: 0,
            ttl_secs: 0,
            created_at: 0,
        });
        assert_eq!(member_sort_key(&h), (1, "off-h".to_string()));
    }

    #[test]
    fn member_sort_key_discovered_prefers_peer_id_then_falls_back_to_fp() {
        let with_id = PodMember::Discovered(PodDiscoveryRowDto {
            pubkey_fp: "fp-x".into(),
            peer_id: Some("disc-id".into()),
            hostname: "hd".into(),
            addr: "10.0.0.3".into(),
            port: 7777,
            discovery_state: "unclaimed".into(),
            can_invite: true,
            first_seen_at: 0,
            last_seen_at: 0,
        });
        assert_eq!(member_sort_key(&with_id), (2, "disc-id".to_string()));

        let no_id = PodMember::Discovered(PodDiscoveryRowDto {
            pubkey_fp: "fp-y".into(),
            peer_id: None,
            hostname: "hd".into(),
            addr: "10.0.0.4".into(),
            port: 7777,
            discovery_state: "unclaimed".into(),
            can_invite: true,
            first_seen_at: 0,
            last_seen_at: 0,
        });
        // No peer_id → the pubkey_fp is used as the stable sort discriminator.
        assert_eq!(member_sort_key(&no_id), (2, "fp-y".to_string()));
    }

    // ── pod_create: per-action required-argument guards (pre-I/O) ─────────────

    #[tokio::test]
    async fn pod_create_join_requires_addr() {
        let ctx = empty_ctx();
        let err = expect_err(
            pod_create(
                PodCreateArgs {
                    action: PodCreateAction::Join,
                    ..Default::default()
                },
                &ctx,
            )
            .await,
        );
        assert!(
            format!("{err:#}").contains("action=join requires `addr`"),
            "got: {err:#}"
        );
    }

    #[tokio::test]
    async fn pod_create_offer_requires_addr() {
        let ctx = empty_ctx();
        let err = expect_err(
            pod_create(
                PodCreateArgs {
                    action: PodCreateAction::Offer,
                    ..Default::default()
                },
                &ctx,
            )
            .await,
        );
        assert!(
            format!("{err:#}").contains("action=offer requires `addr`"),
            "got: {err:#}"
        );
    }

    #[tokio::test]
    async fn pod_create_accept_requires_code() {
        let ctx = empty_ctx();
        let err = expect_err(
            pod_create(
                PodCreateArgs {
                    action: PodCreateAction::Accept,
                    ..Default::default()
                },
                &ctx,
            )
            .await,
        );
        assert!(
            format!("{err:#}").contains("action=accept requires `code`"),
            "got: {err:#}"
        );
    }

    // ── pod_update: required-argument guards (pre-I/O) ────────────────────────

    #[tokio::test]
    async fn pod_update_trust_requires_peer_id() {
        let ctx = empty_ctx();
        let err = expect_err(
            pod_update(
                PodUpdateArgs {
                    action: PodUpdateAction::Trust,
                    ..Default::default()
                },
                &ctx,
            )
            .await,
        );
        assert!(
            format!("{err:#}").contains("action=trust requires `peer_id`"),
            "got: {err:#}"
        );
    }

    #[tokio::test]
    async fn pod_update_trust_requires_on_when_peer_id_present() {
        let ctx = empty_ctx();
        let err = expect_err(
            pod_update(
                PodUpdateArgs {
                    action: PodUpdateAction::Trust,
                    peer_id: Some("some-peer".into()),
                    ..Default::default()
                },
                &ctx,
            )
            .await,
        );
        assert!(
            format!("{err:#}").contains("action=trust requires `on`"),
            "got: {err:#}"
        );
    }

    #[tokio::test]
    async fn pod_update_recover_requires_peer_id() {
        let ctx = empty_ctx();
        let err = expect_err(
            pod_update(
                PodUpdateArgs {
                    action: PodUpdateAction::Recover,
                    ..Default::default()
                },
                &ctx,
            )
            .await,
        );
        assert!(
            format!("{err:#}").contains("action=recover requires `peer_id`"),
            "got: {err:#}"
        );
    }

    #[tokio::test]
    async fn pod_update_cancel_offer_requires_addr() {
        let ctx = empty_ctx();
        let err = expect_err(
            pod_update(
                PodUpdateArgs {
                    action: PodUpdateAction::CancelOffer,
                    ..Default::default()
                },
                &ctx,
            )
            .await,
        );
        assert!(
            format!("{err:#}").contains("action=cancel_offer requires `addr`"),
            "got: {err:#}"
        );
    }

    // ── pod_update: DB-backed arms ────────────────────────────────────────────

    #[tokio::test]
    async fn pod_update_settings_reads_then_writes_self_secure() {
        let tmp = tmp_db();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            // self_secure = None → read current (default false) without mutating.
            let out = pod_update(
                PodUpdateArgs {
                    action: PodUpdateAction::Settings,
                    self_secure: None,
                    ..Default::default()
                },
                &ctx,
            )
            .await
            .unwrap();
            match out {
                PodUpdateOutput::Settings(s) => assert!(!s.self_secure),
                _ => panic!("expected Settings variant"),
            }
            // self_secure = Some(true) → write and echo back the new value.
            let out = pod_update(
                PodUpdateArgs {
                    action: PodUpdateAction::Settings,
                    self_secure: Some(true),
                    ..Default::default()
                },
                &ctx,
            )
            .await
            .unwrap();
            match out {
                PodUpdateOutput::Settings(s) => assert!(s.self_secure),
                _ => panic!("expected Settings variant"),
            }
            assert!(db::pod::get_self_secure(&db::open_default().unwrap()).unwrap());
        })
        .await;
    }

    #[tokio::test]
    async fn pod_update_recover_clears_departed_flag() {
        let tmp = tmp_db();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let pid = utils::id::new();
            let conn = db::open_default().unwrap();
            db::pod::upsert_peer(&conn, &pid, "host-r", "10.0.0.1", 12002, Some("fp"), "ca")
                .unwrap();
            db::pod::mark_peer_departed(&conn, &pid).unwrap();
            drop(conn);
            let out = pod_update(
                PodUpdateArgs {
                    action: PodUpdateAction::Recover,
                    peer_id: Some(pid.clone()),
                    ..Default::default()
                },
                &ctx,
            )
            .await
            .unwrap();
            match out {
                PodUpdateOutput::Recover(r) => {
                    assert_eq!(r.peer_id, pid);
                    assert!(r.cleared);
                }
                _ => panic!("expected Recover variant"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn pod_update_cancel_offer_removes_outbound_rows() {
        let tmp = tmp_db();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let conn = db::open_default().unwrap();
            db::pod::insert_pending_offer(
                &conn,
                "off-1",
                "out",
                "fp",
                "host",
                "10.9.9.9",
                12002,
                "h",
                None,
                None,
                None,
                3600,
                None,
                &[],
            )
            .unwrap();
            drop(conn);
            let out = pod_update(
                PodUpdateArgs {
                    action: PodUpdateAction::CancelOffer,
                    addr: Some("10.9.9.9".into()),
                    ..Default::default()
                },
                &ctx,
            )
            .await
            .unwrap();
            match out {
                PodUpdateOutput::CancelOffer(c) => {
                    assert_eq!(c.addr, "10.9.9.9");
                    assert_eq!(c.rows_removed, 1);
                }
                _ => panic!("expected CancelOffer variant"),
            }
        })
        .await;
    }

    // ── pod_delete: guards + DB-backed arms ───────────────────────────────────

    #[tokio::test]
    async fn pod_delete_kick_requires_peer_id() {
        let ctx = empty_ctx();
        let err = expect_err(
            pod_delete(
                PodDeleteArgs {
                    action: PodDeleteAction::Kick,
                    ..Default::default()
                },
                &ctx,
            )
            .await,
        );
        assert!(
            format!("{err:#}").contains("action=kick requires `peer_id`"),
            "got: {err:#}"
        );
    }

    #[tokio::test]
    async fn pod_delete_forget_requires_peer_id() {
        let ctx = empty_ctx();
        let err = expect_err(
            pod_delete(
                PodDeleteArgs {
                    action: PodDeleteAction::Forget,
                    ..Default::default()
                },
                &ctx,
            )
            .await,
        );
        assert!(
            format!("{err:#}").contains("action=forget requires `peer_id`"),
            "got: {err:#}"
        );
    }

    #[tokio::test]
    async fn pod_delete_forget_unknown_peer_removes_zero_rows() {
        let tmp = tmp_db();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let out = pod_delete(
                PodDeleteArgs {
                    action: PodDeleteAction::Forget,
                    peer_id: Some("ghost".into()),
                },
                &ctx,
            )
            .await
            .unwrap();
            match out {
                PodDeleteOutput::Forget(f) => {
                    assert_eq!(f.peer_id, "ghost");
                    assert_eq!(f.rows_removed, 0);
                    assert!(f.notified.is_empty());
                }
                _ => panic!("expected Forget variant"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn pod_delete_leave_on_empty_db_removes_nothing() {
        let tmp = tmp_db();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let out = pod_delete(
                PodDeleteArgs {
                    action: PodDeleteAction::Leave,
                    ..Default::default()
                },
                &ctx,
            )
            .await
            .unwrap();
            match out {
                PodDeleteOutput::Leave(l) => {
                    assert_eq!(l.rows_removed, 0);
                    assert!(l.peers.is_empty());
                }
                _ => panic!("expected Leave variant"),
            }
        })
        .await;
    }

    // ── reset_if_stale_mesh_identity: outcome → bool mapping ──────────────────

    #[tokio::test]
    async fn reset_if_stale_returns_false_for_unenrolled_host() {
        // A pki dir with neither leaf nor CA is a pre-pod host: the reconcile
        // returns NotEnrolled, and the wrapper reports "nothing changed".
        let app = tempfile::tempdir().unwrap();
        system::host_identity::init(app.path()).unwrap();
        let pki = tempfile::tempdir().unwrap();
        let tmp = tmp_db();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let changed = reset_if_stale_mesh_identity(pki.path()).unwrap();
            assert!(!changed, "unenrolled host must report no on-disk change");
        })
        .await;
    }

    #[tokio::test]
    async fn reset_if_stale_reports_change_when_leaf_migrated() {
        // A CA-holding host whose leaf carries a CN that cannot match this
        // machine's real machine_id() is drifted → migrated in place, so the
        // wrapper reports `true` (on-disk state changed).
        let app = tempfile::tempdir().unwrap();
        system::host_identity::init(app.path()).unwrap();
        let pki = tempfile::tempdir().unwrap();
        // Some CN guaranteed != machine_id() (a 32-hex string), forcing drift.
        utils::pki::init_mesh_ca(pki.path(), "not-the-real-machine-id").unwrap();
        let tmp = tmp_db();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let changed = reset_if_stale_mesh_identity(pki.path()).unwrap();
            assert!(changed, "a drifted leaf that migrates must report a change");
            // The leaf was re-issued under the real machine_id().
            let pem =
                std::fs::read_to_string(utils::pki::mesh_client_cert_path(pki.path())).unwrap();
            let cn = utils::pki::cert_summary(&pem).unwrap().cn;
            assert_eq!(cn, system::host_identity::machine_id().to_string());
        })
        .await;
    }

    // ── pod_update: Trust DB-backed arm (non-push) ────────────────────────────

    #[tokio::test]
    async fn pod_update_trust_sets_local_secure_and_persists() {
        let tmp = tmp_db();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let pid = utils::id::new();
            let conn = db::open_default().unwrap();
            // Seed a peer on a loopback addr + refused port so the best-effort
            // notify fails fast into the `warn:` arm instead of hanging.
            db::pod::upsert_peer(&conn, &pid, "host-t", "127.0.0.1", 1, Some("fp"), "ca").unwrap();
            drop(conn);
            let out = pod_update(
                PodUpdateArgs {
                    action: PodUpdateAction::Trust,
                    peer_id: Some(pid.clone()),
                    on: Some(true),
                    ..Default::default()
                },
                &ctx,
            )
            .await
            .unwrap();
            match out {
                PodUpdateOutput::Trust(t) => {
                    assert_eq!(t.peer_id, pid);
                    assert!(t.local_secure, "trust on must set local_secure");
                    // Peer never trusted us back, so the link is not mutual.
                    assert!(!t.peer_secure);
                    assert!(!t.mutual);
                    // The unreachable loopback:1 target means notify could not
                    // succeed — the arm records a warning, never a silent ok.
                    assert!(
                        t.notify_result.starts_with("warn:"),
                        "unreachable peer must warn, got: {}",
                        t.notify_result
                    );
                }
                _ => panic!("expected Trust variant"),
            }
            // Toggling trust off persists the new value.
            let out = pod_update(
                PodUpdateArgs {
                    action: PodUpdateAction::Trust,
                    peer_id: Some(pid.clone()),
                    on: Some(false),
                    ..Default::default()
                },
                &ctx,
            )
            .await
            .unwrap();
            match out {
                PodUpdateOutput::Trust(t) => {
                    assert!(!t.local_secure, "trust off clears local_secure")
                }
                _ => panic!("expected Trust variant"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn pod_update_trust_unknown_peer_errors() {
        let tmp = tmp_db();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let err = expect_err(
                pod_update(
                    PodUpdateArgs {
                        action: PodUpdateAction::Trust,
                        peer_id: Some("ghost".into()),
                        on: Some(true),
                        ..Default::default()
                    },
                    &ctx,
                )
                .await,
            );
            assert!(
                format!("{err:#}").contains("no such peer: ghost"),
                "got: {err:#}"
            );
        })
        .await;
    }

    // ── pod_update: Sync arm requires a registered replication transport ───────

    #[tokio::test]
    async fn pod_update_sync_without_transport_bails() {
        let ctx = empty_ctx();
        // No daemon has registered a replication transport in this unit-test
        // process, so the Sync arm surfaces the "pair this host first" error.
        let err = expect_err(
            pod_update(
                PodUpdateArgs {
                    action: PodUpdateAction::Sync,
                    ..Default::default()
                },
                &ctx,
            )
            .await,
        );
        assert!(
            format!("{err:#}").contains("no replication transport registered"),
            "got: {err:#}"
        );
    }

    // ── pod_delete: Kick DB-backed arm ────────────────────────────────────────

    #[tokio::test]
    async fn pod_delete_kick_removes_seeded_peer_rows() {
        let tmp = tmp_db();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let pid = utils::id::new();
            let conn = db::open_default().unwrap();
            db::pod::upsert_peer(&conn, &pid, "host-k", "127.0.0.1", 1, Some("fp"), "ca").unwrap();
            assert_eq!(db::pod::list_peers(&conn).unwrap().len(), 1);
            drop(conn);
            let out = pod_delete(
                PodDeleteArgs {
                    action: PodDeleteAction::Kick,
                    peer_id: Some(pid.clone()),
                },
                &ctx,
            )
            .await
            .unwrap();
            match out {
                PodDeleteOutput::Kick(l) => {
                    assert_eq!(l.peer_id, pid);
                    assert_eq!(l.rows_removed, 2, "kick drops pod_peers + pod_trust rows");
                }
                _ => panic!("expected Kick variant"),
            }
            // The peer row is actually gone from the DB after the kick.
            let conn = db::open_default().unwrap();
            assert!(db::pod::list_peers(&conn).unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn pod_delete_kick_unknown_peer_errors() {
        let tmp = tmp_db();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let err = expect_err(
                pod_delete(
                    PodDeleteArgs {
                        action: PodDeleteAction::Kick,
                        peer_id: Some("ghost".into()),
                    },
                    &ctx,
                )
                .await,
            );
            assert!(
                format!("{err:#}").contains("no such peer: ghost"),
                "got: {err:#}"
            );
        })
        .await;
    }

    // ── exec_peer: peer-identity resolution failure (pre-network) ──────────────

    #[tokio::test]
    async fn exec_peer_unknown_peer_id_errors_before_dial() {
        let tmp = tmp_db();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let err = expect_err(exec_peer("ghost", "pod.list", serde_json::json!({})).await);
            assert!(
                format!("{err:#}").contains("no such peer: ghost"),
                "got: {err:#}"
            );
        })
        .await;
    }

    // ── exec / ping: mTLS dial fails fast without on-disk mesh identity ────────
    //
    // Repoints HOME to an empty tempdir (so `pki_dir()` has no client bundle),
    // driving the future on a same-thread runtime while HOME_ENV_LOCK is held —
    // the guard is never carried across an `.await`, mirroring cert_rotation's
    // `with_home`.
    fn with_home<T>(dir: &std::path::Path, body: impl FnOnce(&tokio::runtime::Runtime) -> T) -> T {
        let _guard = crate::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("HOME").ok();
        // SAFETY: HOME is set for the closure's duration and restored right
        // after; serialized behind HOME_ENV_LOCK.
        unsafe { std::env::set_var("HOME", dir) };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = body(&rt);
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        out
    }

    #[test]
    fn exec_without_mesh_client_bundle_errors_on_load() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), |rt| {
            let err =
                expect_err(rt.block_on(exec("10.255.255.1", "pod.list", serde_json::json!({}))));
            // connect_pod_tls loads the mesh client bundle first, so an
            // un-initialised host fails there — never reaching a socket.
            assert!(
                format!("{err:#}").contains("load mesh client bundle"),
                "got: {err:#}"
            );
        });
    }

    #[test]
    fn ping_without_mesh_client_bundle_errors_on_load() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), |rt| {
            let err = expect_err(rt.block_on(ping("10.255.255.1")));
            assert!(
                format!("{err:#}").contains("load mesh client bundle"),
                "got: {err:#}"
            );
        });
    }

    // NOTE: `collect_pod_instances` / `collect_pod_snapshot` are intentionally
    // NOT exercised here. Their shared `assemble_members` → `list_enriched`
    // path performs a live self-probe over the loopback runtime socket (and
    // remote-peer enrichment), so on an ephemeral DB it blocks on real network
    // timeouts and its member set depends on the ambient daemon rather than the
    // seeded rows. That is the "async network / live daemon" class the task
    // scopes out; covering it would require injecting a probe seam into
    // production code. The pure classification/projection helpers they call
    // (`classify_snapshot`, `match_clusters`, `build_instance`) are already
    // covered directly in `pod_snapshot_tests` / `added_coverage`.
}
