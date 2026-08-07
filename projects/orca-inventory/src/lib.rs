//! Server-side pod topology aggregator. `pod.detail` returns a nested
//! cluster → roots → recursive nodes structure the systems UI renders as
//! visually-contained parent cards wrapping child cards. The topology is a
//! POD-level concept (the mesh of systems and the services they hold); the
//! node model is deliberately composable UPWARD so a future `network` parent
//! (routers/firewalls above the pod) can wrap these clusters without a rework.
//!
//! - Parent inference by MAC-claim matching (`system.claims[].macs`
//!   intersected with `system.interfaces[].mac`).
//! - `system.parent_peer_id` overrides MAC inference when set and the
//!   referenced peer exists.
//! - Non-peer entities (guest VMs/LXCs, containers, compose stacks that don't
//!   run orca) render as synthesized [`ClaimNode`] leaves under their host —
//!   see [`synthesize_claim_nodes`]. Parented by `TopologyClaim.runs_on` when
//!   the provider reports it, else the reporting peer.
//! - Cluster bucketing using `contract::ClusterRoster` (today populated by
//!   the proxmox plugin).
//! - Local row first among roots; siblings alphabetic by hostname.
//! - Server returns the full tree every render; expansion is client-local.
//!
//! Per-node drill-down comes from the level-specific detail verbs
//! (`system.detail`, `service.status`); `network.topology_view` sits above.

use anyhow::Result;
use derive::orca_tool;
use pod::{PodInstance, collect_pod_instances, match_clusters_instances};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

// ── Output shapes ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PodTopologyOutput {
    pub clusters: Vec<InventoryCluster>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct InventoryCluster {
    /// `None` = ungrouped bucket (rendered last).
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ClusterSummary>,
    pub roots: Vec<InventoryNode>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct InventoryNode {
    pub source: NodeSource,
    pub children: Vec<InventoryNode>,
}

/// A node's identity. Either an orca **peer** (runs the daemon, has a full
/// `system` snapshot) or a **claim** — a non-peer entity (guest VM/LXC,
/// container, compose stack) that a host reports running but which does not
/// itself run orca. Claim nodes are synthesized from `system.claims[]`.
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(tag = "node_type", rename_all = "snake_case")]
pub enum NodeSource {
    Peer(Box<PodInstance>),
    Claim(Box<ClaimNode>),
}

/// A non-peer entity synthesized from a host's [`contract::TopologyClaim`].
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct ClaimNode {
    /// Synthetic stable id: `claim:{provider}:{provider_instance}:{kind}:{native_id}`.
    pub id: String,
    /// Display name — guest hostname, container name, or stack name.
    pub label: String,
    /// `"vm"`, `"lxc"`, `"container"`, `"stack"`.
    pub kind: String,
    pub provider: String,
    pub provider_instance: String,
    /// Provider-native id (proxmox vmid, docker short id, stack name).
    pub native_id: String,
    /// Hostname of the node this entity runs on, when the provider reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs_on: Option<String>,
    /// Endpoints (ports) this workload listens on. Passthrough from the claim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<contract::topology::ClaimEndpoint>,
    /// Network addresses this entity is reachable at. Passthrough from the
    /// claim; same channel vocabulary peers carry.
    #[serde(default, skip_serializing_if = "contract::topology::Routes::is_empty")]
    pub routes: contract::topology::Routes,
    /// Container image / template ref, when known. Passthrough from the claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Provider labels/metadata. Passthrough from the claim.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Service role after correlation: a matched runtime registration wins over
    /// the provider's claim hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_role: Option<String>,
    /// Host-scoped compose-stack correlation key (a DESCRIPTIVE attribute, not
    /// an id). Passthrough from `TopologyClaim.service_identity`; the key used
    /// to group this container under a synthesized `stack` node. `None` on a
    /// stack node itself and on claims the provider can't attribute to a stack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_identity: Option<String>,
    /// The runtime service identity correlated to this node by `(host, port)`,
    /// when a registration matches one of its endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<contract::service_identity::ServiceRegistration>,
    /// Normalized runtime run-state (`"running"`/`"stopped"`/`"paused"`) when
    /// the provider reports it. Passthrough from the claim; drives the
    /// topology node's `status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Every control pathway to this entity — the provider(s) that can observe
    /// and act on it. A single-provider node carries one entry; when the same
    /// logical container is reported by more than one provider on the same host
    /// (docker's socket AND unraid's GraphQL; docker AND dockge) all pathways
    /// are preserved so callers can tell whether it is controllable over
    /// docker, dockge, both, or unraid. Never collapsed to one — mirrors
    /// `unit::UnitSource`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controllers: Vec<Controller>,
    /// Container claims nested under a synthesized `stack` node. Empty on leaf
    /// (container/vm/lxc) nodes; populated only on `kind == "stack"` nodes,
    /// which group the container claims that share a `service_identity`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ClaimNode>,
}

/// A single control pathway to a synthesized entity: the provider (and which
/// instance of it) through which the entity can be observed and acted on.
#[derive(Serialize, Deserialize, JsonSchema, Clone, PartialEq, Eq)]
pub struct Controller {
    pub provider: String,
    pub provider_instance: String,
    /// Provider-native id under this pathway; may differ across providers for
    /// the same logical entity.
    pub native_id: String,
}

impl InventoryNode {
    /// Stable identifier regardless of node kind (peer_id or synthetic claim id).
    pub fn id(&self) -> &str {
        match &self.source {
            NodeSource::Peer(p) => p.peer_id.as_str(),
            NodeSource::Claim(c) => c.id.as_str(),
        }
    }

    /// The peer, if this node is an orca peer.
    pub fn peer(&self) -> Option<&PodInstance> {
        match &self.source {
            NodeSource::Peer(p) => Some(p),
            NodeSource::Claim(_) => None,
        }
    }

    /// The claim, if this node is a synthesized non-peer entity.
    pub fn claim(&self) -> Option<&ClaimNode> {
        match &self.source {
            NodeSource::Claim(c) => Some(c.as_ref()),
            NodeSource::Peer(_) => None,
        }
    }
}

/// Cluster header summary. Online/total counts derived from
/// `contract::ClusterRoster::list_clusters()`.
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct ClusterSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quorate: Option<bool>,
    pub online: u32,
    pub total: u32,
}

// ── Args ────────────────────────────────────────────────────────────────────

// ── Tool ────────────────────────────────────────────────────────────────────

/// Which slice of pod detail to return.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PodDetailView {
    /// Pre-classified membership / pairing / roster rollup. Default.
    #[default]
    Summary,
    /// Mesh cert days-remaining + rotation + `self_secure` state.
    Certs,
    /// Per-host snapshot history timeseries (needs `peer_id`; `local` for self).
    History,
    /// Recursive cluster/system/service topology tree.
    Topology,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PodDetailArgs {
    /// `summary` (default), `certs`, `history`, or `topology`.
    #[arg(long, default_value = "summary")]
    pub view: PodDetailView,
    /// (history) Peer whose history to read. `local` for this host. Defaults to
    /// `local` when omitted.
    #[arg(long)]
    pub peer_id: Option<String>,
    /// (history) Return only rows with `snapshot_at_unix > since_unix`.
    #[arg(long)]
    pub since_unix: Option<i64>,
    /// (history) Maximum rows to return (default 256 in storage).
    #[arg(long)]
    pub limit: Option<u32>,
}

/// View-tagged detail payload. One verb, four slices — the former `pod.certs`,
/// `pod.history`, and `network.topology_view` fold in here.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "view", content = "data", rename_all = "snake_case")]
pub enum PodDetailOutput {
    Summary(Box<pod::PodSnapshotOutput>),
    Certs(Box<pod::PodCertStatusOutput>),
    History(pod::status::HostStatusRows),
    Topology(PodTopologyOutput),
}

/// Pod detail, sliced by `view`. `summary` is the pre-classified membership /
/// pairing / roster rollup; `certs` the mesh-trust + `self_secure` state;
/// `history` a per-host snapshot timeseries; `topology` the recursive
/// cluster/system/service tree (each node an orca peer or a non-peer entity —
/// guest VM/LXC, container, compose stack — parent-inferred via MAC claims with
/// `system.parent_peer_id` overrides). `pod.list` is the flat systems roster.
#[orca_tool(domain = "pod", verb = "detail")]
async fn pod_detail(args: PodDetailArgs, ctx: &contract::ToolCtx) -> Result<PodDetailOutput> {
    match args.view {
        PodDetailView::Summary => Ok(PodDetailOutput::Summary(Box::new(
            pod::collect_pod_snapshot(ctx).await?,
        ))),
        PodDetailView::Certs => Ok(PodDetailOutput::Certs(Box::new(pod::server_pod::status()?))),
        PodDetailView::History => {
            let peer_id = args.peer_id.unwrap_or_else(|| "local".to_string());
            let rows = pod::status::host_status_detail(
                pod::status::HostStatusDetailArgs {
                    peer_id,
                    since_unix: args.since_unix,
                    limit: args.limit,
                },
                ctx,
            )
            .await?;
            Ok(PodDetailOutput::History(rows))
        }
        PodDetailView::Topology => Ok(PodDetailOutput::Topology(pod_topology_view(ctx).await?)),
    }
}

/// The recursive pod topology tree (former `pod.detail` body). Invoked by
/// `pod.detail view=topology`.
async fn pod_topology_view(ctx: &contract::ToolCtx) -> Result<PodTopologyOutput> {
    let instances_out = collect_pod_instances().await?;
    let instances = instances_out.members;

    let clusters = match ctx.service::<std::sync::Arc<dyn contract::ClusterRoster>>() {
        Ok(svc) => svc.list_clusters().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut cluster_by_peer = match_clusters_instances(&instances, &clusters);
    augment_clusters_from_system(&instances, &mut cluster_by_peer);
    let summaries = build_cluster_summaries(&clusters);

    let regs = contract::service_identity::collect_registrations().await;
    let (roots, children_of, claim_children) = build_forest(&instances, &regs);
    let bucketed = bucket_roots(
        roots,
        &children_of,
        &claim_children,
        &cluster_by_peer,
        &summaries,
    );
    Ok(PodTopologyOutput { clusters: bucketed })
}

// ── Algorithm ───────────────────────────────────────────────────────────────

/// Fold each peer's self-reported `system.cluster` into the peer→cluster map.
/// The local `ClusterRoster` (when a provider is loaded) wins; self-report
/// fills the gaps — so a daemon with no proxmox plugin (e.g. a laptop) still
/// groups PVE peers by the cluster name they each gossip in their snapshot.
fn augment_clusters_from_system(
    instances: &[PodInstance],
    cluster_by_peer: &mut BTreeMap<String, String>,
) {
    for inst in instances {
        if let Some(sys) = inst.system.as_ref()
            && let Some(c) = sys.cluster.as_deref()
            && !c.is_empty()
        {
            cluster_by_peer
                .entry(inst.peer_id.clone())
                .or_insert_with(|| c.to_string());
        }
    }
}

fn build_cluster_summaries(clusters: &[contract::ClusterEntry]) -> HashMap<String, ClusterSummary> {
    let mut out: HashMap<String, ClusterSummary> = HashMap::new();
    for entry in clusters {
        let Some(cname) = entry.name.as_deref() else {
            continue;
        };
        let total = entry.nodes.len() as u32;
        let online = entry
            .nodes
            .iter()
            .filter(|n| n.online == Some(true))
            .count() as u32;
        let summary = ClusterSummary {
            name: cname.to_string(),
            quorate: entry.quorate,
            online,
            total,
        };
        match out.get(cname) {
            Some(prev) if prev.online >= summary.online => {}
            _ => {
                out.insert(cname.to_string(), summary);
            }
        }
    }
    out
}

/// Roots (sorted: local first, then alphabetic), a `peer_id -> sorted peer
/// children` map, and a `peer_id -> sorted claim-node children` map.
type Forest = (
    Vec<PodInstance>,
    HashMap<String, Vec<PodInstance>>,
    HashMap<String, Vec<ClaimNode>>,
);

/// Build the parent-inference forest.
/// Presentation-boundary canonical peer identity: the bare uuidv7, stripped of
/// the legacy `peer.` secure-registration prefix. Per the uuidv7 identity rule a
/// peer has exactly ONE id; the secure/insecure registration form is a separate
/// concern the transport/PKI layer owns (and `PodInstance::secure` already
/// carries), never part of the identity the inventory + topology views expose.
fn canonical_peer_id(id: &str) -> &str {
    id.strip_prefix("peer.").unwrap_or(id)
}

/// Normalize a peer-instance list for the view layer: rewrite each `peer_id`
/// (and any `system.parent_peer_id`) to its canonical bare uuidv7, and collapse
/// secure/insecure twins of the same peer to a single instance (keep first).
fn canonicalize_instances(instances: &[PodInstance]) -> Vec<PodInstance> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for inst in instances {
        let mut c = inst.clone();
        c.peer_id = canonical_peer_id(&c.peer_id).to_string();
        c.id = canonical_peer_id(&c.id).to_string();
        if let Some(sys) = c.system.as_mut()
            && let Some(p) = sys.parent_peer_id.as_mut()
        {
            *p = canonical_peer_id(p).to_string();
        }
        if seen.insert(c.peer_id.clone()) {
            out.push(c);
        }
    }
    out
}

fn build_forest(
    instances: &[PodInstance],
    regs: &[contract::service_identity::ServiceRegistration],
) -> Forest {
    let instances = canonicalize_instances(instances);
    let instances = instances.as_slice();
    let by_peer: HashMap<&str, &PodInstance> =
        instances.iter().map(|i| (i.peer_id.as_str(), i)).collect();

    let mut mac_index: HashMap<String, String> = HashMap::new();
    for inst in instances {
        let Some(sys) = inst.system.as_ref() else {
            continue;
        };
        for c in &sys.claims {
            for m in &c.macs {
                if !m.is_empty() {
                    mac_index.insert(m.to_lowercase(), inst.peer_id.clone());
                }
            }
        }
    }

    let infer_parent = |inst: &PodInstance| -> Option<String> {
        if let Some(sys) = inst.system.as_ref() {
            if let Some(server_parent) = sys.parent_peer_id.as_deref()
                && server_parent != inst.peer_id
                && by_peer.contains_key(server_parent)
            {
                return Some(server_parent.to_string());
            }
            for mac in &sys.macs {
                if mac.is_empty() {
                    continue;
                }
                if let Some(claimer) = mac_index.get(&mac.to_lowercase())
                    && claimer != &inst.peer_id
                    && by_peer.contains_key(claimer.as_str())
                {
                    return Some(claimer.clone());
                }
            }
        }
        None
    };

    let mut children_of: HashMap<String, Vec<PodInstance>> = HashMap::new();
    let mut roots: Vec<PodInstance> = Vec::new();
    for inst in instances {
        match infer_parent(inst) {
            Some(parent) => children_of.entry(parent).or_default().push(inst.clone()),
            None => roots.push(inst.clone()),
        }
    }

    let sort_key = |inst: &PodInstance| -> String {
        inst.system
            .as_ref()
            .and_then(|s| s.hostname.as_deref())
            .unwrap_or(inst.label.as_str())
            .to_lowercase()
    };
    for arr in children_of.values_mut() {
        arr.sort_by_key(sort_key);
    }
    roots.sort_by(|a, b| match (a.role.as_str(), b.role.as_str()) {
        ("local", "local") => sort_key(a).cmp(&sort_key(b)),
        ("local", _) => std::cmp::Ordering::Less,
        (_, "local") => std::cmp::Ordering::Greater,
        _ => sort_key(a).cmp(&sort_key(b)),
    });

    let claim_children = synthesize_claim_nodes(instances, regs);
    (roots, children_of, claim_children)
}

/// Turn each host's unmatched `system.claims[]` into non-peer child nodes.
///
/// A claim whose MACs resolve to an existing orca peer is skipped — that
/// guest already renders as a real peer node (e.g. an Alpine VM running the
/// daemon). Everything else (guests/containers/stacks that don't run orca)
/// becomes a synthetic [`ClaimNode`] parented to a peer:
///
/// - by `runs_on` → the peer whose hostname matches, when the provider
///   reported it (needed for cluster-shared sources like proxmox pmxcfs,
///   where every cluster peer reports every guest);
/// - otherwise → the reporting peer (correct for single-host providers).
///
/// Claims are deduped by `(provider, provider_instance, kind, native_id)`,
/// preferring the copy that carries `runs_on`, then the lexicographically
/// smallest reporting hostname (deterministic across a cluster).
///
/// Each synthesized node is then correlated against `regs`: a runtime
/// [`contract::service_identity::ServiceRegistration`] joins to the node when
/// its `host` resolves to the node's parent peer (or the claim reports that
/// host directly) and its `port` matches one of the claim's endpoints. A match
/// sets `service`/`service_role` (registration wins over the provider hint).
fn synthesize_claim_nodes(
    instances: &[PodInstance],
    regs: &[contract::service_identity::ServiceRegistration],
) -> HashMap<String, Vec<ClaimNode>> {
    // MACs owned by real peers → skip claims that are actually peers.
    let mut peer_macs: HashSet<String> = HashSet::new();
    // Resolution index for `runs_on`: hostname AND every network address a
    // peer is known by (LAN v4/v6, tailscale, primary_ipv4) → peer_id. A
    // provider may report `runs_on` as a bare hostname or as whatever host
    // segment it has (e.g. a remote instance's `base_url` IP), so we match
    // against all of them, lowercased.
    let mut peer_by_key: HashMap<String, String> = HashMap::new();
    for inst in instances {
        let Some(sys) = inst.system.as_ref() else {
            continue;
        };
        for mac in &sys.macs {
            if !mac.is_empty() {
                peer_macs.insert(mac.to_lowercase());
            }
        }
        let mut add = |k: &str| {
            if !k.is_empty() {
                peer_by_key
                    .entry(k.to_lowercase())
                    .or_insert_with(|| inst.peer_id.clone());
            }
        };
        if let Some(h) = sys.hostname.as_deref() {
            add(h);
        }
        if let Some(ip) = sys.primary_ipv4.as_deref() {
            add(ip);
        }
        for a in &inst.addresses {
            add(&a.value);
        }
    }

    struct Cand {
        claim: contract::TopologyClaim,
        reporting_peer: String,
        reporting_host: String,
    }
    let mut chosen: HashMap<String, Cand> = HashMap::new();
    for inst in instances {
        let Some(sys) = inst.system.as_ref() else {
            continue;
        };
        let rhost = sys.hostname.clone().unwrap_or_else(|| inst.label.clone());
        for c in &sys.claims {
            let is_peer = c
                .macs
                .iter()
                .any(|m| !m.is_empty() && peer_macs.contains(&m.to_lowercase()));
            if is_peer {
                continue;
            }
            let key = format!(
                "{}\u{1}{}\u{1}{}\u{1}{}",
                c.provider, c.provider_instance, c.kind, c.id
            );
            let cand = Cand {
                claim: c.clone(),
                reporting_peer: inst.peer_id.clone(),
                reporting_host: rhost.clone(),
            };
            match chosen.get(&key) {
                None => {
                    chosen.insert(key, cand);
                }
                Some(prev) => {
                    let replace = match (prev.claim.runs_on.is_some(), cand.claim.runs_on.is_some())
                    {
                        (false, true) => true,
                        (true, false) => false,
                        _ => {
                            cand.reporting_host.to_lowercase() < prev.reporting_host.to_lowercase()
                        }
                    };
                    if replace {
                        chosen.insert(key, cand);
                    }
                }
            }
        }
    }

    let mut out: HashMap<String, Vec<ClaimNode>> = HashMap::new();
    for cand in chosen.into_values() {
        let c = &cand.claim;
        let parent = c
            .runs_on
            .as_deref()
            .and_then(|h| peer_by_key.get(&h.to_lowercase()).cloned())
            .unwrap_or_else(|| cand.reporting_peer.clone());
        // Correlate a runtime service registration to this node: its host must
        // resolve to the same parent peer (or match the claim's own reported
        // host), and its port must match one of the claim's endpoints.
        let claim_hosts: HashSet<String> = std::iter::once(cand.reporting_host.to_lowercase())
            .chain(c.runs_on.iter().map(|h| h.to_lowercase()))
            .collect();
        let service = regs
            .iter()
            .find(|r| {
                let host_lc = r.host.to_lowercase();
                let host_ok =
                    peer_by_key.get(&host_lc) == Some(&parent) || claim_hosts.contains(&host_lc);
                let port_ok = c
                    .endpoints
                    .iter()
                    .any(|e| e.port == r.port || e.published_port == Some(r.port));
                host_ok && port_ok
            })
            .cloned();
        let service_role = service
            .as_ref()
            .map(|r| r.role.clone())
            .or_else(|| c.service_role.clone());
        // The node id is the source-assigned UUIDv7 (`c.uuid`). The provider/
        // instance/kind/native-id fields remain as searchable attrs on the
        // node, never baked into the id. Transitional guard: a pre-uuid
        // reporter mid-rollout sends an empty `uuid`; fall back to the legacy
        // composed key so the node still renders (it converges to the UUIDv7
        // once that peer updates).
        let node_id = if c.uuid.is_empty() {
            format!(
                "claim:{}:{}:{}:{}",
                c.provider, c.provider_instance, c.kind, c.id
            )
        } else {
            c.uuid.clone()
        };
        let node = ClaimNode {
            id: node_id,
            label: c.name.clone(),
            kind: c.kind.clone(),
            provider: c.provider.clone(),
            provider_instance: c.provider_instance.clone(),
            native_id: c.id.clone(),
            runs_on: c.runs_on.clone(),
            endpoints: c.endpoints.clone(),
            routes: c.routes.clone(),
            image: c.image.clone(),
            labels: c.labels.clone(),
            service_role,
            service,
            service_identity: c.service_identity.clone(),
            state: c.state.clone(),
            controllers: vec![Controller {
                provider: c.provider.clone(),
                provider_instance: c.provider_instance.clone(),
                native_id: c.id.clone(),
            }],
            children: Vec::new(),
        };
        out.entry(parent).or_default().push(node);
    }
    // Consolidate control pathways: the same logical container can be reported
    // by more than one provider on the same host (docker's socket AND unraid's
    // GraphQL; docker AND dockge). Collapse container-kind nodes that share a
    // normalized name under one parent into a single node whose `controllers`
    // enumerate every pathway. This also sorts each parent's children.
    for v in out.values_mut() {
        consolidate_controllers(v);
    }
    // Group containers that carry a `service_identity` under a synthesized
    // `stack` node — the compose stack they belong to. Docker and dockge claims
    // sharing a service_identity dedup onto the SAME stack node (the container
    // consolidation above already merged their control pathways, so a single
    // container node now carries both controllers before it is nested). Runs
    // after container consolidation so a stack nests the ALREADY-collapsed
    // container, never two provider-specific copies.
    for v in out.values_mut() {
        group_stacks(v);
    }
    out
}

/// Nest container nodes that share a `service_identity` under a synthesized
/// `stack` node keyed by that identity. The stack node's id is a uuidv7 minted
/// via `system::unit_identity::resolve_or_mint` (identity stays pure-uuidv7;
/// the service_identity string is the DESCRIPTIVE natural key it is minted
/// from, never the id itself). Containers without a service_identity, and non-
/// container kinds (vm/lxc/already-synthesized stacks), pass through unnested.
///
/// When the identity registry DB is unavailable (e.g. a fresh test process, or
/// the same transient the sibling `assign_claim_uuids` guards) the stack id
/// falls back to the deterministic `stack:{service_identity}` key so nesting
/// still happens and converges to the uuidv7 once the registry is reachable.
fn group_stacks(nodes: &mut Vec<ClaimNode>) {
    // Preserve deterministic first-seen order of stacks by their identity key.
    let mut stack_order: Vec<String> = Vec::new();
    let mut stacks: HashMap<String, Vec<ClaimNode>> = HashMap::new();
    let mut passthrough: Vec<ClaimNode> = Vec::new();

    for n in nodes.drain(..) {
        match (n.kind.as_str(), n.service_identity.clone()) {
            ("container", Some(sid)) if !sid.is_empty() => {
                if !stacks.contains_key(&sid) {
                    stack_order.push(sid.clone());
                }
                stacks.entry(sid).or_default().push(n);
            }
            _ => passthrough.push(n),
        }
    }

    let mut out: Vec<ClaimNode> = passthrough;
    for sid in stack_order {
        let mut members = stacks.remove(&sid).unwrap_or_default();
        members.sort_by_key(|a| a.label.to_lowercase());
        // Union every member's control pathways onto the stack node so callers
        // can tell which providers manage the stack as a whole.
        let mut controllers: Vec<Controller> = Vec::new();
        for m in &members {
            for c in &m.controllers {
                if !controllers.contains(c) {
                    controllers.push(c.clone());
                }
            }
        }
        // Stack display name: the last path segment of the compose working dir
        // (or the bare project name), whichever the service_identity encodes —
        // strip the host-scope prefix and any leading path.
        let label = stack_label(&sid);
        // Identity: pure uuidv7 minted from the descriptive service_identity
        // key; deterministic string fallback when the registry DB is absent.
        let stack_id = match system::unit_identity::resolve_or_mint(&sid) {
            Ok(uuid) => uuid.to_string(),
            Err(e) => {
                tracing::warn!(
                    service_identity = %sid, error = %e,
                    "inventory: stack-id mint deferred; using fallback key",
                );
                format!("stack:{sid}")
            }
        };
        // runs_on / provider_instance describe the stack via its members; take
        // them from the first member so the stack parents to the right host.
        let runs_on = members.first().and_then(|m| m.runs_on.clone());
        let provider = members
            .first()
            .map(|m| m.provider.clone())
            .unwrap_or_default();
        let provider_instance = members
            .first()
            .map(|m| m.provider_instance.clone())
            .unwrap_or_default();
        out.push(ClaimNode {
            id: stack_id,
            label,
            kind: "stack".to_string(),
            provider,
            provider_instance,
            native_id: sid.clone(),
            runs_on,
            endpoints: Vec::new(),
            routes: Default::default(),
            image: None,
            labels: BTreeMap::new(),
            service_role: None,
            service: None,
            service_identity: Some(sid),
            state: None,
            controllers,
            children: members,
        });
    }
    out.sort_by_key(|a| a.label.to_lowercase());
    *nodes = out;
}

/// Human label for a synthesized stack from its `service_identity` key. The key
/// is `"<host>\u{1f}<signal>"` where signal is a compose working dir or project
/// name; the label is the final path segment of the signal.
fn stack_label(service_identity: &str) -> String {
    let signal = service_identity
        .split('\u{1f}')
        .next_back()
        .unwrap_or(service_identity);
    signal
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(signal)
        .to_string()
}

/// Container names compared for cross-provider identity: case-folded, with the
/// leading `/` docker sometimes prefixes stripped.
fn norm_container_name(s: &str) -> String {
    s.trim_start_matches('/').to_lowercase()
}

/// The real-world-entity identity key for cross-provider merging, scoped to one
/// host (this runs per-parent). Two claims that resolve to the SAME key denote
/// the SAME physical/logical thing seen by different providers/instances and
/// MUST collapse to one node — NOT stay separate because their
/// `provider_instance` differs (that produced the fleet's double-counted guests:
/// the pmxcfs `local` conf-reader and the proxmox-plugin `yggdrasil` API reader
/// each emitted every running guest, minting distinct uuids). Identity is the
/// entity, never the reporter.
///
/// - `container` → normalized container name (docker socket, dockge, and the
///   unraid GraphQL collector all agree on the name; the container id is not
///   exposed by every provider, the name is the reliable cross-provider join).
/// - `vm`/`lxc` → the provider-native id (the PVE vmid is cluster-unique and
///   identical across every reader), falling back to the normalized name for a
///   provider that can't supply an id.
/// - everything else (`stack`, …) → `None`: not a shared-control atom, passes
///   through untouched.
fn entity_merge_key(n: &ClaimNode) -> Option<String> {
    match n.kind.as_str() {
        "container" => Some(format!("container\u{1}{}", norm_container_name(&n.label))),
        "vm" | "lxc" => {
            let ident = if !n.native_id.is_empty() {
                n.native_id.to_lowercase()
            } else {
                norm_container_name(&n.label)
            };
            // Group vm+lxc together under one guest namespace: a single PVE vmid
            // is one guest regardless of how a reader typed it.
            Some(format!("guest\u{1}{ident}"))
        }
        _ => None,
    }
}

/// Merge nodes that denote the SAME real entity across providers/instances,
/// unioning their control pathways and facets. Covers containers (docker /
/// dockge / unraid) AND guests (vm/lxc reported by both the pmxcfs conf-reader
/// and the proxmox API). `stack` and other non-atoms pass through untouched.
///
/// Deterministic: the representative is the node with the smallest
/// `(provider, provider_instance)`; every descriptive facet the rep lacks is
/// filled from a sibling, list facets (addresses/endpoints/labels) are unioned,
/// run-state prefers `running` (a live reader beats a config-only reader), and
/// `controllers` preserve every reporting pathway.
fn consolidate_controllers(nodes: &mut Vec<ClaimNode>) {
    let mut groups: BTreeMap<String, Vec<ClaimNode>> = BTreeMap::new();
    let mut passthrough: Vec<ClaimNode> = Vec::new();
    for n in nodes.drain(..) {
        match entity_merge_key(&n) {
            Some(key) => groups.entry(key).or_default().push(n),
            None => passthrough.push(n),
        }
    }
    let mut merged: Vec<ClaimNode> = Vec::new();
    for group in groups.into_values() {
        let mut group = group;
        group.sort_by(|a, b| {
            (a.provider.as_str(), a.provider_instance.as_str())
                .cmp(&(b.provider.as_str(), b.provider_instance.as_str()))
        });
        let mut rep = group[0].clone();
        let mut controllers: Vec<Controller> = Vec::new();
        for m in &group {
            for c in &m.controllers {
                if !controllers.contains(c) {
                    controllers.push(c.clone());
                }
            }
            if rep.service.is_none() && m.service.is_some() {
                rep.service = m.service.clone();
                rep.service_role = m.service_role.clone().or_else(|| rep.service_role.clone());
            }
            // Run-state: a live reader (`running`) wins over a config-only
            // reader that reports nothing/`unknown`, whichever order they merge.
            if (m.state.as_deref() == Some("running") || rep.state.is_none())
                && let Some(s) = m.state.clone()
            {
                rep.state = Some(s);
            }
            if rep.image.is_none() && m.image.is_some() {
                rep.image = m.image.clone();
            }
            if rep.service_identity.is_none() && m.service_identity.is_some() {
                rep.service_identity = m.service_identity.clone();
            }
            if rep.runs_on.is_none() && m.runs_on.is_some() {
                rep.runs_on = m.runs_on.clone();
            }
            // Union list facets so one provider's routes and another's
            // ports/labels/icons all survive on the single unified node.
            // `Routes::push` dedups by (kind, value), so no explicit guard.
            for a in &m.routes {
                rep.routes.push(a.clone());
            }
            for e in &m.endpoints {
                if !rep.endpoints.contains(e) {
                    rep.endpoints.push(e.clone());
                }
            }
            for (k, v) in &m.labels {
                rep.labels.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        rep.controllers = controllers;
        merged.push(rep);
    }
    merged.append(&mut passthrough);
    merged.sort_by_key(|a| a.label.to_lowercase());
    *nodes = merged;
}

/// Recursively materialize a node and its descendants. `visited` guards
/// against cycles (shouldn't happen with current inference rules).
fn build_node(
    inst: &PodInstance,
    children_of: &HashMap<String, Vec<PodInstance>>,
    claim_children: &HashMap<String, Vec<ClaimNode>>,
    visited: &mut HashSet<String>,
) -> InventoryNode {
    visited.insert(inst.peer_id.clone());
    let kids = children_of
        .get(&inst.peer_id)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut children: Vec<InventoryNode> = Vec::new();
    for k in kids {
        if !visited.contains(&k.peer_id) {
            children.push(build_node(k, children_of, claim_children, visited));
        }
    }
    // Non-peer claim entities render as children after the peer children. A
    // `stack` claim carries its container children nested; materialize them so
    // the tree shows host → stack → containers.
    if let Some(claims) = claim_children.get(&inst.peer_id) {
        for c in claims {
            children.push(claim_to_node(c));
        }
    }
    // The inventory graph carries identity + lean topology facts only; the fat
    // host snapshot + time-series are explicit drill-in calls
    // (`system.info.detail` / `system.history`), never bundled here.
    InventoryNode {
        source: NodeSource::Peer(Box::new(inst.clone())),
        children,
    }
}

/// Materialize a [`ClaimNode`] (and any nested stack children) into an
/// [`InventoryNode`]. A stack node's `children` become nested inventory nodes;
/// the stack node itself keeps an empty `children` vec on the `ClaimNode` copy
/// stored in `source` so the nested containers aren't duplicated on the wire.
fn claim_to_node(c: &ClaimNode) -> InventoryNode {
    let mut bare = c.clone();
    let nested = std::mem::take(&mut bare.children);
    InventoryNode {
        source: NodeSource::Claim(Box::new(bare)),
        children: nested.iter().map(claim_to_node).collect(),
    }
}

/// Bucket roots by cluster. Named clusters alphabetical; ungrouped bucket
/// (None) trails. When no clusters are configured, returns a single
/// ungrouped bucket holding every root.
fn bucket_roots(
    roots: Vec<PodInstance>,
    children_of: &HashMap<String, Vec<PodInstance>>,
    claim_children: &HashMap<String, Vec<ClaimNode>>,
    cluster_by_peer: &BTreeMap<String, String>,
    summaries: &HashMap<String, ClusterSummary>,
) -> Vec<InventoryCluster> {
    let mut visited: HashSet<String> = HashSet::new();

    // Only fall back to a single ungrouped bucket when there is NO cluster
    // signal at all — neither a roster summary nor a self-reported
    // `system.cluster`. Summaries may be empty while peers still self-report
    // a cluster name (mesh vantage without the proxmox plugin); those still
    // group, with `summary: None`.
    if cluster_by_peer.is_empty() {
        let nodes: Vec<InventoryNode> = roots
            .iter()
            .map(|r| build_node(r, children_of, claim_children, &mut visited))
            .collect();
        return vec![InventoryCluster {
            name: None,
            summary: None,
            roots: nodes,
        }];
    }

    let mut buckets: HashMap<Option<String>, Vec<InventoryNode>> = HashMap::new();
    let mut seen_keys: Vec<Option<String>> = Vec::new();
    for root in &roots {
        let cname = cluster_by_peer.get(&root.peer_id).cloned();
        if !buckets.contains_key(&cname) {
            seen_keys.push(cname.clone());
        }
        let node = build_node(root, children_of, claim_children, &mut visited);
        buckets.entry(cname).or_default().push(node);
    }

    let mut named: Vec<String> = seen_keys.iter().filter_map(|c| c.clone()).collect();
    named.sort();
    let has_ungrouped = seen_keys.iter().any(|c| c.is_none());

    let mut out: Vec<InventoryCluster> = Vec::new();
    for n in named {
        let summary = summaries.get(&n).cloned();
        let nodes = buckets.remove(&Some(n.clone())).unwrap_or_default();
        out.push(InventoryCluster {
            name: Some(n),
            summary,
            roots: nodes,
        });
    }
    if has_ungrouped {
        let nodes = buckets.remove(&None).unwrap_or_default();
        out.push(InventoryCluster {
            name: None,
            summary: None,
            roots: nodes,
        });
    }
    out
}

// Per-node drill-down (former `inventory.detail`) is retired: the pod topology
// tree (`pod.detail`) carries structure, and per-node richness comes from the
// level-specific detail verbs (`system.detail`, `service.status`).

/// Index a materialized tree by node id and record each node's parent id. Kept
/// for the topology-structure test below; the parent-inference forest it walks
/// still powers `pod.detail`.
#[cfg(test)]
fn index_tree(
    node: &InventoryNode,
    parent: Option<&str>,
    by_id: &mut HashMap<String, InventoryNode>,
    parent_of: &mut HashMap<String, String>,
) {
    let id = node.id().to_string();
    if let Some(p) = parent {
        parent_of.insert(id.clone(), p.to_string());
    }
    for child in &node.children {
        index_tree(child, Some(&id), by_id, parent_of);
    }
    by_id.insert(id, node.clone());
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use contract::TopologyClaim;
    use system::system::TopologyFacts;

    fn empty_sys() -> TopologyFacts {
        TopologyFacts::default()
    }

    /// The inventory graph carries the lean topology facts (identity + topology)
    /// verbatim — the fat host snapshot and time-series never ride the tree;
    /// they are explicit `system.info.detail` / `system.history` drill-ins.
    #[test]
    fn build_node_carries_lean_topology_facts() {
        let mut fat = inst("p1", "system", "host1");
        fat.system.as_mut().unwrap().primary_ipv4 = Some("10.0.0.1".into());
        let node = build_node(&fat, &HashMap::new(), &HashMap::new(), &mut HashSet::new());
        let NodeSource::Peer(p) = &node.source else {
            panic!("expected peer node");
        };
        let sys = p.system.as_ref().expect("system present");
        assert_eq!(
            sys.primary_ipv4.as_deref(),
            Some("10.0.0.1"),
            "lean topology facts must ride the graph node"
        );
    }

    fn inst(peer_id: &str, role: &str, hostname: &str) -> PodInstance {
        let mut sys = empty_sys();
        sys.hostname = Some(hostname.to_string());
        PodInstance {
            id: peer_id.to_string(),
            peer_id: peer_id.to_string(),
            label: hostname.to_string(),
            origin: String::new(),
            port: 12000,
            role: role.to_string(),
            version: None,
            target: None,
            mode: None,
            channel: None,
            pinned_to: None,
            update_available: false,
            update_latest: None,
            update_checked_secs: None,
            health: "up".to_string(),
            error: None,
            last_checked: Some(1000),
            secure: None,
            status: None,
            addresses: vec![],
            system: Some(sys),
            reachable_addrs: vec![],
            available_versions: vec![],
        }
    }

    fn with_iface_mac(mut i: PodInstance, mac: &str) -> PodInstance {
        i.system.as_mut().unwrap().macs.push(mac.to_string());
        i
    }

    fn with_claim_mac(mut i: PodInstance, mac: &str) -> PodInstance {
        let sys = i.system.as_mut().unwrap();
        sys.claims.push(TopologyClaim {
            kind: "guest".into(),
            id: "1".into(),
            name: "guest".into(),
            macs: vec![mac.to_string()],
            provider: "test".into(),
            provider_instance: "local".into(),
            runs_on: None,
            ..Default::default()
        });
        i
    }

    /// Push a fully-specified claim (no MAC → never matches a peer) onto a peer.
    fn with_claim(
        mut i: PodInstance,
        kind: &str,
        native_id: &str,
        name: &str,
        provider: &str,
        instance: &str,
        runs_on: Option<&str>,
    ) -> PodInstance {
        let sys = i.system.as_mut().unwrap();
        sys.claims.push(TopologyClaim {
            kind: kind.into(),
            id: native_id.into(),
            name: name.into(),
            macs: vec![],
            provider: provider.into(),
            provider_instance: instance.into(),
            runs_on: runs_on.map(|s| s.to_string()),
            ..Default::default()
        });
        i
    }

    fn bucket_empty(
        roots: Vec<PodInstance>,
        children_of: &HashMap<String, Vec<PodInstance>>,
        claim_children: &HashMap<String, Vec<ClaimNode>>,
    ) -> Vec<InventoryCluster> {
        bucket_roots(
            roots,
            children_of,
            claim_children,
            &BTreeMap::new(),
            &HashMap::new(),
        )
    }

    #[test]
    fn single_root_no_children_emits_one_node() {
        let only = inst("only", "local", "only");
        let (roots, kids, claims) = build_forest(&[only], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        assert_eq!(out.len(), 1);
        assert!(out[0].name.is_none());
        assert_eq!(out[0].roots.len(), 1);
        assert_eq!(out[0].roots[0].id(), "only");
        assert!(out[0].roots[0].children.is_empty());
    }

    #[test]
    fn root_with_two_children_sorted_alphabetic() {
        let host = with_claim_mac(inst("host", "local", "host"), "aa:bb:cc:dd:ee:01");
        let host = with_claim_mac(host, "aa:bb:cc:dd:ee:02");
        let kid_b = with_iface_mac(inst("kb", "system", "beta"), "aa:bb:cc:dd:ee:01");
        let kid_a = with_iface_mac(inst("ka", "system", "alpha"), "aa:bb:cc:dd:ee:02");
        let (roots, kids, claims) = build_forest(&[host, kid_b, kid_a], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].roots.len(), 1);
        let root = &out[0].roots[0];
        assert_eq!(root.id(), "host");
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].id(), "ka");
        assert_eq!(root.children[1].id(), "kb");
    }

    #[test]
    fn multi_root_local_first_then_alphabetic() {
        let a = inst("a", "system", "alpha");
        let local = inst("z", "local", "zulu");
        let m = inst("m", "system", "mike");
        let (roots, kids, claims) = build_forest(&[a, local, m], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        assert_eq!(out[0].roots.len(), 3);
        assert_eq!(out[0].roots[0].id(), "z");
        assert_eq!(out[0].roots[1].id(), "a");
        assert_eq!(out[0].roots[2].id(), "m");
    }

    #[test]
    fn mac_inference_nests_child_under_host() {
        let host = with_claim_mac(inst("host", "system", "host"), "aa:bb:cc:dd:ee:01");
        let guest = with_iface_mac(inst("guest", "system", "guest"), "AA:BB:CC:DD:EE:01");
        let (roots, kids, claims) = build_forest(&[host, guest], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        assert_eq!(out[0].roots.len(), 1);
        assert_eq!(out[0].roots[0].id(), "host");
        assert_eq!(out[0].roots[0].children.len(), 1);
        assert_eq!(out[0].roots[0].children[0].id(), "guest");
    }

    #[test]
    fn parent_peer_id_override_takes_precedence_over_mac() {
        // Two potential hosts; child has MAC matching host_a's claim, but
        // parent_peer_id pins to host_b.
        let host_a = with_claim_mac(inst("host_a", "local", "ahost"), "aa:bb:cc:dd:ee:01");
        let host_b = inst("host_b", "system", "bhost");
        let mut guest = with_iface_mac(inst("guest", "system", "guest"), "aa:bb:cc:dd:ee:01");
        guest.system.as_mut().unwrap().parent_peer_id = Some("host_b".into());
        let (roots, kids, claims) = build_forest(&[host_a, host_b, guest], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        // Two roots: host_a and host_b. guest nests under host_b.
        let root_b = out[0].roots.iter().find(|n| n.id() == "host_b").unwrap();
        assert_eq!(root_b.children.len(), 1);
        assert_eq!(root_b.children[0].id(), "guest");
        let root_a = out[0].roots.iter().find(|n| n.id() == "host_a").unwrap();
        assert!(root_a.children.is_empty());
    }

    #[test]
    fn orphan_with_unknown_parent_peer_id_surfaces_as_root() {
        let mut orphan = inst("orphan", "system", "orphan");
        orphan.system.as_mut().unwrap().parent_peer_id = Some("ghost".into());
        let (roots, kids, claims) = build_forest(&[orphan], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        assert_eq!(out[0].roots.len(), 1);
        assert_eq!(out[0].roots[0].id(), "orphan");
    }

    #[test]
    fn nonpeer_claim_renders_as_leaf_child() {
        // A host reports a guest that does not run orca → synthetic leaf node.
        let host = with_claim(
            inst("host", "local", "host"),
            "lxc",
            "113",
            "jellyfin",
            "proxmox",
            "local",
            None,
        );
        let (roots, kids, claims) = build_forest(&[host], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        let root = &out[0].roots[0];
        assert_eq!(root.id(), "host");
        assert_eq!(root.children.len(), 1);
        let child = &root.children[0];
        assert_eq!(child.id(), "claim:proxmox:local:lxc:113");
        let c = child.claim().expect("claim node");
        assert_eq!(c.label, "jellyfin");
        assert_eq!(c.kind, "lxc");
        assert_eq!(c.native_id, "113");
        assert!(child.peer().is_none());
    }

    /// Push a claim carrying a published endpoint (for correlation tests).
    fn with_claim_endpoint(
        mut i: PodInstance,
        kind: &str,
        native_id: &str,
        name: &str,
        port: u16,
    ) -> PodInstance {
        let sys = i.system.as_mut().unwrap();
        sys.claims.push(TopologyClaim {
            kind: kind.into(),
            id: native_id.into(),
            name: name.into(),
            provider: "docker".into(),
            provider_instance: "local".into(),
            endpoints: vec![contract::topology::ClaimEndpoint {
                port,
                published_port: Some(port),
                protocol: "tcp".into(),
                host_ip: None,
            }],
            ..Default::default()
        });
        i
    }

    fn reg(role: &str, host: &str, port: u16) -> contract::service_identity::ServiceRegistration {
        contract::service_identity::ServiceRegistration {
            role: role.into(),
            host: host.into(),
            port,
            provider: "sonarr".into(),
            version: None,
            primitives: Vec::new(),
        }
    }

    #[test]
    fn registration_correlates_to_claim_by_host_and_port() {
        let host = with_claim_endpoint(
            inst("host", "local", "freyr"),
            "container",
            "abc123",
            "sonarr",
            8989,
        );
        let regs = vec![reg("sonarr", "freyr", 8989)];
        let (roots, kids, claims) = build_forest(&[host], &regs);
        let out = bucket_empty(roots, &kids, &claims);
        let child = &out[0].roots[0].children[0];
        let c = child.claim().expect("claim node");
        assert_eq!(c.service_role.as_deref(), Some("sonarr"));
        assert_eq!(c.service.as_ref().map(|s| s.role.as_str()), Some("sonarr"));
    }

    #[test]
    fn registration_with_wrong_port_does_not_correlate() {
        let host = with_claim_endpoint(
            inst("host", "local", "freyr"),
            "container",
            "abc123",
            "sonarr",
            8989,
        );
        let regs = vec![reg("sonarr", "freyr", 9999)];
        let (roots, kids, claims) = build_forest(&[host], &regs);
        let out = bucket_empty(roots, &kids, &claims);
        let c = out[0].roots[0].children[0].claim().unwrap();
        assert!(c.service.is_none());
        assert!(c.service_role.is_none());
    }

    #[test]
    fn detail_traversal_yields_ancestors_and_descendants() {
        let host = with_claim_endpoint(
            inst("host", "local", "freyr"),
            "container",
            "abc123",
            "sonarr",
            8989,
        );
        let (roots, children_of, claim_children) = build_forest(&[host], &[]);
        let mut by_id = HashMap::new();
        let mut parent_of = HashMap::new();
        for root in &roots {
            let mut visited = HashSet::new();
            let tree = build_node(root, &children_of, &claim_children, &mut visited);
            index_tree(&tree, None, &mut by_id, &mut parent_of);
        }
        let claim_id = "claim:docker:local:container:abc123";
        // The claim node's parent is the host peer.
        assert_eq!(parent_of.get(claim_id).map(String::as_str), Some("host"));
        // Host has the claim as a descendant; host itself has no parent (root).
        assert!(!parent_of.contains_key("host"));
        let host_node = by_id.get("host").unwrap();
        assert_eq!(host_node.children.len(), 1);
        assert_eq!(host_node.children[0].id(), claim_id);
    }

    #[test]
    fn claim_matching_a_peer_is_not_duplicated() {
        // host claims MAC ee:01; guest peer owns ee:01 → guest renders as a
        // real peer child, and the claim must NOT also synthesize a leaf.
        let host = with_claim_mac(inst("host", "local", "host"), "aa:bb:cc:dd:ee:01");
        let guest = with_iface_mac(inst("guest", "system", "guest"), "AA:BB:CC:DD:EE:01");
        let (roots, kids, claims) = build_forest(&[host, guest], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        let root = &out[0].roots[0];
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].id(), "guest");
        assert!(root.children[0].peer().is_some());
    }

    #[test]
    fn runs_on_parents_claim_to_matching_peer() {
        // Two cluster peers both report the same guest (cluster-shared config);
        // runs_on pins it to the peer whose hostname matches.
        let a = with_claim(
            inst("a", "local", "alpha"),
            "vm",
            "200",
            "web",
            "proxmox",
            "local",
            Some("beta"),
        );
        let b = inst("b", "system", "beta");
        let (roots, kids, claims) = build_forest(&[a, b], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        let root_a = out[0].roots.iter().find(|n| n.id() == "a").unwrap();
        let root_b = out[0].roots.iter().find(|n| n.id() == "b").unwrap();
        // parented to beta (b) via runs_on, not to the reporting peer alpha (a).
        assert!(root_a.children.is_empty());
        assert_eq!(root_b.children.len(), 1);
        assert_eq!(root_b.children[0].id(), "claim:proxmox:local:vm:200");
    }

    #[test]
    fn cluster_shared_claim_deduped_across_reporting_peers() {
        // Three cluster peers each report the SAME guest (shared pmxcfs), all
        // runs_on=None → one synthetic node total, deterministically under the
        // lexicographically-smallest reporting hostname.
        let mk = |peer: &str, host: &str| {
            with_claim(
                inst(peer, "system", host),
                "lxc",
                "110",
                "plex",
                "proxmox",
                "local",
                None,
            )
        };
        let (roots, kids, claims) = build_forest(
            &[mk("p3", "gamma"), mk("p1", "alpha"), mk("p2", "beta")],
            &[],
        );
        let out = bucket_empty(roots, &kids, &claims);
        let total_claims: usize = out[0]
            .roots
            .iter()
            .map(|r| r.children.iter().filter(|c| c.claim().is_some()).count())
            .sum();
        assert_eq!(total_claims, 1);
        // under "alpha" (p1), the smallest reporting hostname.
        let root_a = out[0].roots.iter().find(|n| n.id() == "p1").unwrap();
        assert_eq!(root_a.children.len(), 1);
        assert_eq!(root_a.children[0].id(), "claim:proxmox:local:lxc:110");
    }

    /// Push a fully-specified container claim: uuid (so nodes collapse by
    /// identity, not the fallback composite key) + service_identity (so it
    /// groups under a stack). Every other field defaults.
    #[allow(clippy::too_many_arguments)]
    fn with_container_claim(
        mut i: PodInstance,
        native_id: &str,
        name: &str,
        provider: &str,
        instance: &str,
        uuid: &str,
        service_identity: Option<&str>,
    ) -> PodInstance {
        let sys = i.system.as_mut().unwrap();
        sys.claims.push(TopologyClaim {
            kind: "container".into(),
            id: native_id.into(),
            uuid: uuid.into(),
            name: name.into(),
            provider: provider.into(),
            provider_instance: instance.into(),
            service_identity: service_identity.map(|s| s.to_string()),
            ..Default::default()
        });
        i
    }

    /// (a) The same logical container reported by two providers (docker socket
    /// AND dockge) carries ONE shared uuidv7. It must collapse to a single node
    /// whose `controllers` enumerate both pathways — never two parallel nodes.
    #[test]
    fn two_container_reps_same_uuid_collapse_to_one() {
        let shared_uuid = "0190aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee";
        let host = with_container_claim(
            inst("host", "local", "host"),
            "abc123",
            "jellyfin",
            "docker",
            "local",
            shared_uuid,
            None,
        );
        let host = with_container_claim(
            host,
            "abc123",
            "jellyfin",
            "dockge",
            "instance1",
            shared_uuid,
            None,
        );
        let claims = synthesize_claim_nodes(&[host], &[]);
        let kids = claims.get("host").expect("host has claims");
        assert_eq!(kids.len(), 1, "two reps must collapse to one node");
        let node = &kids[0];
        assert_eq!(node.id, shared_uuid);
        let providers: HashSet<&str> = node
            .controllers
            .iter()
            .map(|c| c.provider.as_str())
            .collect();
        assert!(providers.contains("docker"));
        assert!(providers.contains("dockge"), "both pathways preserved");
    }

    /// (b) Two container claims sharing a `service_identity` nest under ONE
    /// synthesized `stack` node — docker and dockge claims dedup onto the same
    /// stack. (No identity-registry DB in a test process, so the stack id falls
    /// back to the deterministic `stack:<sid>` key; nesting still holds.)
    #[test]
    fn two_claims_sharing_service_identity_nest_under_one_stack() {
        let sid = "host1\u{1f}/opt/stacks/arr";
        let host = with_container_claim(
            inst("host", "local", "host"),
            "aaa",
            "sonarr",
            "docker",
            "local",
            "0190aaaa-bbbb-7ccc-8ddd-000000000a01",
            Some(sid),
        );
        let host = with_container_claim(
            host,
            "bbb",
            "radarr",
            "docker",
            "local",
            "0190aaaa-bbbb-7ccc-8ddd-000000000a02",
            Some(sid),
        );
        let claims = synthesize_claim_nodes(&[host], &[]);
        let kids = claims.get("host").expect("host has claims");
        // Exactly one top-level node under the host: the stack.
        assert_eq!(kids.len(), 1, "both containers roll up into one stack");
        let stack = &kids[0];
        assert_eq!(stack.kind, "stack");
        assert_eq!(stack.service_identity.as_deref(), Some(sid));
        assert_eq!(stack.label, "arr");
        // Both containers nested under it.
        assert_eq!(stack.children.len(), 2);
        let names: HashSet<&str> = stack.children.iter().map(|c| c.label.as_str()).collect();
        assert!(names.contains("sonarr"));
        assert!(names.contains("radarr"));
    }

    /// A container without a service_identity stays a top-level leaf (not forced
    /// under a phantom stack) — the grouping is opt-in per the claim.
    #[test]
    fn container_without_service_identity_stays_a_leaf() {
        let host = with_container_claim(
            inst("host", "local", "host"),
            "solo",
            "adguard",
            "docker",
            "local",
            "0190aaaa-bbbb-7ccc-8ddd-000000000b01",
            None,
        );
        let claims = synthesize_claim_nodes(&[host], &[]);
        let kids = claims.get("host").expect("host has claims");
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].kind, "container");
        assert!(kids[0].children.is_empty());
    }

    /// Set `routes` on the most recently pushed claim of a peer.
    fn with_claim_routes(mut i: PodInstance, routes: contract::topology::Routes) -> PodInstance {
        let claims = &mut i.system.as_mut().unwrap().claims;
        claims.last_mut().unwrap().routes = routes;
        i
    }

    /// Set `state` on the most recently pushed claim of a peer.
    fn with_claim_state(mut i: PodInstance, state: &str) -> PodInstance {
        let claims = &mut i.system.as_mut().unwrap().claims;
        claims.last_mut().unwrap().state = Some(state.to_string());
        i
    }

    #[test]
    fn same_container_across_providers_merges_control_pathways() {
        // One host reports the same container name via docker AND unraid
        // (leading-slash and case variants) → one node carrying both control
        // pathways, so a caller can see it is controllable over "both".
        let host = with_claim(
            inst("host", "local", "host"),
            "container",
            "abc123",
            "Sonarr",
            "docker",
            "local",
            None,
        );
        let host = with_claim(
            host,
            "container",
            "def456",
            "/sonarr",
            "unraid",
            "tower",
            None,
        );
        let claims = synthesize_claim_nodes(std::slice::from_ref(&host), &[]);
        let nodes = claims.get("host").expect("claims under host");
        assert_eq!(
            nodes.len(),
            1,
            "docker+unraid container collapses to one node"
        );
        let mut provs: Vec<_> = nodes[0]
            .controllers
            .iter()
            .map(|c| c.provider.as_str())
            .collect();
        provs.sort_unstable();
        assert_eq!(provs, vec!["docker", "unraid"]);
    }

    #[test]
    fn proxmox_guest_reported_by_two_readers_dedups_to_one() {
        // The fleet double-count: the built-in pmxcfs conf-reader (instance
        // "local", NO runtime state) and the proxmox plugin API (instance
        // "yggdrasil", state=running) both emit the SAME guest (vmid 101).
        // They must collapse to ONE node keyed on the entity (vmid), carrying
        // BOTH control pathways, with run-state resolved to `running`.
        let host = with_claim(
            inst("thor", "local", "thor"),
            "lxc",
            "101",
            "adguard",
            "proxmox",
            "local",
            Some("thor"),
        );
        let host = with_claim(
            host,
            "lxc",
            "101",
            "adguard",
            "proxmox",
            "yggdrasil",
            Some("thor"),
        );
        let host = with_claim_state(host, "running"); // on the yggdrasil claim
        let claims = synthesize_claim_nodes(std::slice::from_ref(&host), &[]);
        let nodes = claims.get("thor").expect("claims under thor");
        assert_eq!(nodes.len(), 1, "two proxmox readers collapse to one guest");
        assert_eq!(
            nodes[0].state.as_deref(),
            Some("running"),
            "live API state wins over the config-only reader"
        );
        let mut insts: Vec<_> = nodes[0]
            .controllers
            .iter()
            .map(|c| c.provider_instance.as_str())
            .collect();
        insts.sort_unstable();
        assert_eq!(insts, vec!["local", "yggdrasil"]);
    }

    #[test]
    fn distinct_guests_on_same_host_stay_separate() {
        // Different vmids are different guests — must NOT merge.
        let host = with_claim(
            inst("thor", "local", "thor"),
            "lxc",
            "101",
            "adguard",
            "proxmox",
            "yggdrasil",
            Some("thor"),
        );
        let host = with_claim(
            host,
            "vm",
            "105",
            "haos",
            "proxmox",
            "yggdrasil",
            Some("thor"),
        );
        let claims = synthesize_claim_nodes(std::slice::from_ref(&host), &[]);
        assert_eq!(claims.get("thor").expect("claims").len(), 2);
    }

    #[test]
    fn distinct_containers_keep_separate_single_pathways() {
        let host = with_claim(
            inst("host", "local", "host"),
            "container",
            "a",
            "sonarr",
            "docker",
            "local",
            None,
        );
        let host = with_claim(host, "container", "b", "radarr", "docker", "local", None);
        let claims = synthesize_claim_nodes(std::slice::from_ref(&host), &[]);
        let nodes = claims.get("host").expect("claims under host");
        assert_eq!(nodes.len(), 2);
        for n in nodes {
            assert_eq!(n.controllers.len(), 1, "single provider → single pathway");
        }
    }

    #[test]
    fn claim_routes_survive_into_synthesis_and_surface() {
        // A topology claim route is the schemeless mesh form tagged with its
        // provider source.
        let route = contract::topology::Route {
            source: Some("provider".into()),
            ..contract::topology::Route::mesh("lan_v4", "10.0.0.5", None)
        };
        let host = with_claim(
            inst("host", "local", "host"),
            "container",
            "abc123",
            "nginx",
            "docker",
            "local",
            None,
        );
        let host = with_claim_routes(host, contract::topology::Routes::from(vec![route.clone()]));

        // Synthesis: the ClaimNode carries the route.
        let nodes = synthesize_claim_nodes(std::slice::from_ref(&host), &[]);
        let cnode = &nodes["host"][0];
        assert_eq!(cnode.routes.len(), 1);
        assert_eq!(cnode.routes[0], route);

        // Surface: the materialized forest node carries the route too.
        let (roots, kids, claims) = build_forest(&[host], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        let snode = out[0].roots[0]
            .children
            .iter()
            .find(|n| n.id() == "claim:docker:local:container:abc123")
            .expect("claim surface node");
        let c = snode.claim().expect("claim node");
        assert_eq!(c.routes.len(), 1);
        assert_eq!(c.routes[0], route);
    }

    fn with_system_type(mut i: PodInstance, t: &str) -> PodInstance {
        i.system.as_mut().unwrap().system_type = Some(t.to_string());
        i
    }

    fn with_cluster(mut i: PodInstance, name: &str) -> PodInstance {
        i.system.as_mut().unwrap().cluster = Some(name.to_string());
        i
    }

    #[test]
    fn self_reported_cluster_groups_without_roster() {
        // No ClusterRoster (empty summaries), but peers gossip system.cluster —
        // the mesh-vantage path a laptop with no proxmox plugin relies on.
        let insts = vec![
            with_cluster(inst("a", "local", "alpha"), "ygg"),
            with_cluster(inst("b", "system", "beta"), "ygg"),
            inst("c", "system", "gamma"), // standalone, no cluster
        ];
        let (roots, kids, claims) = build_forest(&insts, &[]);
        let mut cbp = BTreeMap::new();
        augment_clusters_from_system(&insts, &mut cbp);
        let out = bucket_roots(roots, &kids, &claims, &cbp, &HashMap::new());
        let ygg = out
            .iter()
            .find(|c| c.name.as_deref() == Some("ygg"))
            .unwrap();
        assert_eq!(ygg.roots.len(), 2);
        assert!(ygg.summary.is_none()); // no roster → no summary, still grouped
        let ungrouped = out.iter().find(|c| c.name.is_none()).unwrap();
        assert_eq!(ungrouped.roots.len(), 1);
        assert_eq!(ungrouped.roots[0].id(), "c");
    }

    #[test]
    fn roster_cluster_wins_over_self_report() {
        // When both exist, the roster mapping takes precedence.
        let insts = vec![with_cluster(inst("a", "local", "alpha"), "self-name")];
        let mut cbp = BTreeMap::new();
        cbp.insert("a".to_string(), "roster-name".to_string());
        augment_clusters_from_system(&insts, &mut cbp);
        assert_eq!(cbp.get("a").map(String::as_str), Some("roster-name"));
    }

    #[test]
    fn empty_input_emits_empty_bucket() {
        let (roots, kids, claims) = build_forest(&[], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        assert_eq!(out.len(), 1);
        assert!(out[0].name.is_none());
        assert!(out[0].roots.is_empty());
    }

    // ── canonical peer id (task #17) ─────────────────────────────────────────

    #[test]
    fn virtualized_appliance_nests_under_its_hypervisor() {
        // A virtualized Unraid (e.g. maple = VM 111 on frigg) reports
        // system_type "unraid" yet genuinely runs as frigg's guest: frigg's
        // conf claims the VM's MAC, which equals maple's NIC MAC. It MUST nest
        // under frigg — system_type does not imply bare metal.
        let frigg = with_claim_mac(
            with_system_type(inst("frigg", "system", "frigg"), "proxmox-ve"),
            "bc:24:11:f8:0f:ac",
        );
        let maple = with_iface_mac(
            with_system_type(inst("maple", "system", "maple"), "unraid"),
            "BC:24:11:F8:0F:AC",
        );
        let (roots, kids, claims) = build_forest(&[frigg, maple], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        let frigg_root = out[0].roots.iter().find(|n| n.id() == "frigg").unwrap();
        assert_eq!(frigg_root.children.len(), 1, "the virtualized unraid nests");
        assert_eq!(frigg_root.children[0].id(), "maple");
    }

    #[test]
    fn real_vm_still_nests_under_hypervisor() {
        // The guard must not over-root: a real VM (kvm, no appliance
        // system_type) with a matching MAC still nests.
        let frigg = with_claim_mac(
            with_system_type(inst("frigg", "system", "frigg"), "proxmox-ve"),
            "aa:bb:cc:dd:ee:02",
        );
        let mut baldur = with_iface_mac(inst("baldur", "system", "baldur"), "aa:bb:cc:dd:ee:02");
        baldur.system.as_mut().unwrap().virtualization = Some("kvm".into());
        let (roots, kids, claims) = build_forest(&[frigg, baldur], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        let frigg_root = out[0].roots.iter().find(|n| n.id() == "frigg").unwrap();
        assert_eq!(frigg_root.children.len(), 1, "the vm nests");
        assert_eq!(frigg_root.children[0].id(), "baldur");
    }

    #[test]
    fn peer_prefix_stripped_and_twins_collapse() {
        // One peer registered twice — secure `peer.<uuid>` + bare `<uuid>` —
        // must collapse to a single node keyed by the canonical bare uuid.
        let secure = inst("019e7105-abc", "system", "frigg");
        let bare = inst("019e7105-abc", "system", "frigg");
        let (roots, kids, claims) = build_forest(&[secure, bare], &[]);
        let out = bucket_empty(roots, &kids, &claims);
        assert_eq!(out[0].roots.len(), 1, "secure/insecure twins collapse");
        assert_eq!(
            out[0].roots[0].id(),
            "019e7105-abc",
            "identity is the bare uuidv7, never the peer.-prefixed form"
        );
    }

    #[test]
    fn clusters_named_alphabetic_then_ungrouped_last() {
        let a = inst("a", "system", "a"); // ungrouped
        let b = inst("b", "system", "b"); // cluster "zeta"
        let c = inst("c", "system", "c"); // cluster "alpha"
        let mut cluster_by_peer = BTreeMap::new();
        cluster_by_peer.insert("b".to_string(), "zeta".to_string());
        cluster_by_peer.insert("c".to_string(), "alpha".to_string());
        let mut summaries = HashMap::new();
        summaries.insert(
            "alpha".to_string(),
            ClusterSummary {
                name: "alpha".into(),
                quorate: Some(true),
                online: 1,
                total: 1,
            },
        );
        summaries.insert(
            "zeta".to_string(),
            ClusterSummary {
                name: "zeta".into(),
                quorate: Some(true),
                online: 1,
                total: 1,
            },
        );
        let (roots, kids, claims) = build_forest(&[a, b, c], &[]);
        let out = bucket_roots(roots, &kids, &claims, &cluster_by_peer, &summaries);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].name.as_deref(), Some("alpha"));
        assert_eq!(out[0].roots.len(), 1);
        assert_eq!(out[0].roots[0].id(), "c");
        assert_eq!(out[1].name.as_deref(), Some("zeta"));
        assert_eq!(out[1].roots[0].id(), "b");
        assert!(out[2].name.is_none());
        assert_eq!(out[2].roots[0].id(), "a");
    }
}
