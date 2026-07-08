//! Server-side inventory aggregator. `inventory.tree` returns a nested
//! cluster → roots → recursive nodes structure the systems UI renders as
//! visually-contained parent cards wrapping child cards.
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
//! Subsequent slices will add `inventory.list`, `inventory.detail`,
//! `inventory.dependency_graph`, and `network.topology_view`.

use anyhow::Result;
use derive::orca_tool;
use pod::{PodInstance, collect_pod_instances, match_clusters_instances};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

// ── Output shapes ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct InventoryTreeOutput {
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
    Claim(ClaimNode),
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
            NodeSource::Claim(c) => Some(c),
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

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct InventoryTreeArgs {}

// ── Tool ────────────────────────────────────────────────────────────────────

/// Unified systems-view inventory tree. Returns clusters of recursive
/// `InventoryNode` trees. Each node is either an orca peer or a non-peer
/// entity (guest VM/LXC, container, compose stack) synthesized from a host's
/// topology claims. Peers are parent-inferred via MAC claims (with
/// `system.parent_peer_id` overrides); claim nodes hang under the host that
/// runs them. The UI renders each node as a card that visually contains its
/// children.
#[orca_tool(domain = "inventory", verb = "tree")]
async fn inventory_tree(
    _args: InventoryTreeArgs,
    ctx: &contract::ToolCtx,
) -> Result<InventoryTreeOutput> {
    let instances_out = collect_pod_instances().await?;
    let instances = instances_out.members;

    let clusters = match ctx.service::<std::sync::Arc<dyn contract::ClusterRoster>>() {
        Ok(svc) => svc.list_clusters().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut cluster_by_peer = match_clusters_instances(&instances, &clusters);
    augment_clusters_from_system(&instances, &mut cluster_by_peer);
    let summaries = build_cluster_summaries(&clusters);

    let (roots, children_of, claim_children) = build_forest(&instances);
    let bucketed = bucket_roots(
        roots,
        &children_of,
        &claim_children,
        &cluster_by_peer,
        &summaries,
    );
    Ok(InventoryTreeOutput { clusters: bucketed })
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
fn build_forest(instances: &[PodInstance]) -> Forest {
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
            for iface in &sys.interfaces {
                let Some(mac) = iface.mac.as_deref() else {
                    continue;
                };
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

    let claim_children = synthesize_claim_nodes(instances);
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
fn synthesize_claim_nodes(instances: &[PodInstance]) -> HashMap<String, Vec<ClaimNode>> {
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
        for iface in &sys.interfaces {
            if let Some(mac) = iface.mac.as_deref()
                && !mac.is_empty()
            {
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
        let node = ClaimNode {
            id: format!(
                "claim:{}:{}:{}:{}",
                c.provider, c.provider_instance, c.kind, c.id
            ),
            label: c.name.clone(),
            kind: c.kind.clone(),
            provider: c.provider.clone(),
            provider_instance: c.provider_instance.clone(),
            native_id: c.id.clone(),
            runs_on: c.runs_on.clone(),
        };
        out.entry(parent).or_default().push(node);
    }
    for v in out.values_mut() {
        v.sort_by_key(|a| a.label.to_lowercase());
    }
    out
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
    // Non-peer claim entities render as leaf children after the peer children.
    if let Some(claims) = claim_children.get(&inst.peer_id) {
        for c in claims {
            children.push(InventoryNode {
                source: NodeSource::Claim(c.clone()),
                children: Vec::new(),
            });
        }
    }
    InventoryNode {
        source: NodeSource::Peer(Box::new(inst.clone())),
        children,
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

// ── network.topology_view ──────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct NetworkTopologyOutput {
    pub nodes: Vec<TopologyNode>,
    pub edges: Vec<TopologyEdge>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct TopologyNode {
    pub id: String,
    pub label: String,
    pub kind: NodeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub status: NodeStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub badges: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Host,
    Vm,
    Lxc,
    Container,
    Stack,
    Internet,
    Cluster,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Up,
    Down,
    Unknown,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct TopologyEdge {
    pub id: String,
    pub source: String,
    pub target: String,
    pub kind: EdgeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    MacClaim,
    ParentPeer,
    /// Host → non-peer entity it runs (guest/container/stack claim node).
    Runs,
    NfsMount,
    Network,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct NetworkTopologyArgs {}

/// Network topology graph for the systems view. Returns a flat node + edge
/// list (NOT a nested tree) suitable for force-directed canvas rendering.
/// Nodes are peers plus the non-peer entities they run (guests/containers/
/// stacks). Edges are parent-inference relationships between peers (MAC-claim
/// or explicit `parent_peer_id`) and `Runs` edges from a host to each non-peer
/// entity. Clusters surface as compound nodes when `ClusterRoster` populates
/// them.
#[orca_tool(domain = "network", verb = "topology_view")]
async fn network_topology_view(
    _args: NetworkTopologyArgs,
    ctx: &contract::ToolCtx,
) -> Result<NetworkTopologyOutput> {
    let instances_out = collect_pod_instances().await?;
    let instances = instances_out.members;

    let clusters = match ctx.service::<std::sync::Arc<dyn contract::ClusterRoster>>() {
        Ok(svc) => svc.list_clusters().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut cluster_by_peer = match_clusters_instances(&instances, &clusters);
    augment_clusters_from_system(&instances, &mut cluster_by_peer);

    Ok(build_topology(&instances, &cluster_by_peer))
}

fn build_topology(
    instances: &[PodInstance],
    cluster_by_peer: &BTreeMap<String, String>,
) -> NetworkTopologyOutput {
    let by_peer: HashMap<&str, &PodInstance> =
        instances.iter().map(|i| (i.peer_id.as_str(), i)).collect();

    // Resolve parent-of-each-instance + edge kind that won the inference.
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

    let mut edges: Vec<TopologyEdge> = Vec::new();
    for inst in instances {
        let Some(sys) = inst.system.as_ref() else {
            continue;
        };
        // parent_peer_id wins.
        if let Some(server_parent) = sys.parent_peer_id.as_deref()
            && server_parent != inst.peer_id
            && by_peer.contains_key(server_parent)
        {
            edges.push(TopologyEdge {
                id: format!("parent:{}->{}", server_parent, inst.peer_id),
                source: server_parent.to_string(),
                target: inst.peer_id.clone(),
                kind: EdgeKind::ParentPeer,
                label: None,
            });
            continue;
        }
        // Otherwise: first matching MAC claim.
        let mut claimed: Option<String> = None;
        for iface in &sys.interfaces {
            let Some(mac) = iface.mac.as_deref() else {
                continue;
            };
            if mac.is_empty() {
                continue;
            }
            if let Some(claimer) = mac_index.get(&mac.to_lowercase())
                && claimer != &inst.peer_id
                && by_peer.contains_key(claimer.as_str())
            {
                claimed = Some(claimer.clone());
                break;
            }
        }
        if let Some(parent) = claimed {
            edges.push(TopologyEdge {
                id: format!("mac:{}->{}", parent, inst.peer_id),
                source: parent,
                target: inst.peer_id.clone(),
                kind: EdgeKind::MacClaim,
                label: None,
            });
        }
    }

    // Cluster compound parents.
    let mut cluster_names: Vec<String> = cluster_by_peer
        .values()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    cluster_names.sort();

    let mut nodes: Vec<TopologyNode> = Vec::new();
    for cname in &cluster_names {
        nodes.push(TopologyNode {
            id: format!("cluster:{cname}"),
            label: cname.clone(),
            kind: NodeKind::Cluster,
            parent_id: None,
            status: NodeStatus::Unknown,
            badges: Vec::new(),
        });
    }

    for inst in instances {
        let parent_id = cluster_by_peer
            .get(&inst.peer_id)
            .map(|c| format!("cluster:{c}"));
        nodes.push(TopologyNode {
            id: inst.peer_id.clone(),
            label: inst
                .system
                .as_ref()
                .and_then(|s| s.hostname.as_deref())
                .unwrap_or(inst.label.as_str())
                .to_string(),
            kind: classify(inst),
            parent_id,
            status: classify_status(inst),
            badges: badges_for(inst),
        });
    }

    // Non-peer entities (guests/containers/stacks) as nodes, each with a
    // `Runs` edge from its parent host peer.
    let claim_children = synthesize_claim_nodes(instances);
    let mut parents: Vec<&String> = claim_children.keys().collect();
    parents.sort();
    for parent in parents {
        for c in &claim_children[parent] {
            nodes.push(TopologyNode {
                id: c.id.clone(),
                label: c.label.clone(),
                kind: classify_claim(&c.kind),
                parent_id: None,
                status: NodeStatus::Unknown,
                badges: vec![c.provider.clone()],
            });
            edges.push(TopologyEdge {
                id: format!("runs:{}->{}", parent, c.id),
                source: parent.clone(),
                target: c.id.clone(),
                kind: EdgeKind::Runs,
                label: None,
            });
        }
    }

    NetworkTopologyOutput { nodes, edges }
}

/// Map a claim's `kind` string to a topology [`NodeKind`].
fn classify_claim(kind: &str) -> NodeKind {
    match kind {
        "lxc" => NodeKind::Lxc,
        "container" => NodeKind::Container,
        "stack" => NodeKind::Stack,
        // "vm" and anything unrecognized fall back to Vm.
        _ => NodeKind::Vm,
    }
}

fn classify(inst: &PodInstance) -> NodeKind {
    let Some(sys) = inst.system.as_ref() else {
        return NodeKind::Host;
    };
    let sys_type = sys.system_type.as_deref().unwrap_or("");
    let virt = sys.virtualization.as_deref().unwrap_or("none");
    match sys_type {
        "proxmox-ve" | "unraid" | "truenas-scale" | "truenas-core" | "proxmox-backup-server" => {
            NodeKind::Host
        }
        _ => match virt {
            "lxc" => NodeKind::Lxc,
            "docker" => NodeKind::Container,
            "none" | "" => NodeKind::Host,
            // kvm/qemu/vmware/etc.
            _ => NodeKind::Vm,
        },
    }
}

fn classify_status(inst: &PodInstance) -> NodeStatus {
    match inst.health.as_str() {
        "up" => NodeStatus::Up,
        "down" | "stale" | "offline" => NodeStatus::Down,
        _ => NodeStatus::Unknown,
    }
}

fn badges_for(inst: &PodInstance) -> Vec<String> {
    let Some(sys) = inst.system.as_ref() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    if let Some(label) = sys.system_type_label.as_deref() {
        out.push(label.to_string());
    }
    out
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use contract::TopologyClaim;
    use system::system_info_types::{NetIfaceDto, SystemInfoReport};

    fn empty_sys() -> SystemInfoReport {
        SystemInfoReport::default()
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
        let sys = i.system.as_mut().unwrap();
        sys.interfaces.push(NetIfaceDto {
            name: "eth0".into(),
            mac: Some(mac.to_string()),
            ipv4: vec![],
            ipv6: vec![],
            loopback: false,
        });
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
        let (roots, kids, claims) = build_forest(&[only]);
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
        let (roots, kids, claims) = build_forest(&[host, kid_b, kid_a]);
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
        let (roots, kids, claims) = build_forest(&[a, local, m]);
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
        let (roots, kids, claims) = build_forest(&[host, guest]);
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
        let (roots, kids, claims) = build_forest(&[host_a, host_b, guest]);
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
        let (roots, kids, claims) = build_forest(&[orphan]);
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
        let (roots, kids, claims) = build_forest(&[host]);
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

    #[test]
    fn claim_matching_a_peer_is_not_duplicated() {
        // host claims MAC ee:01; guest peer owns ee:01 → guest renders as a
        // real peer child, and the claim must NOT also synthesize a leaf.
        let host = with_claim_mac(inst("host", "local", "host"), "aa:bb:cc:dd:ee:01");
        let guest = with_iface_mac(inst("guest", "system", "guest"), "AA:BB:CC:DD:EE:01");
        let (roots, kids, claims) = build_forest(&[host, guest]);
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
        let (roots, kids, claims) = build_forest(&[a, b]);
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
        let (roots, kids, claims) =
            build_forest(&[mk("p3", "gamma"), mk("p1", "alpha"), mk("p2", "beta")]);
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

    #[test]
    fn topology_emits_claim_node_and_runs_edge() {
        let host = with_claim(
            inst("host", "local", "host"),
            "container",
            "abc123",
            "nginx",
            "docker",
            "local",
            None,
        );
        let out = build_topology(&[host], &BTreeMap::new());
        let cnode = out
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Container)
            .expect("container node");
        assert_eq!(cnode.id, "claim:docker:local:container:abc123");
        assert_eq!(cnode.label, "nginx");
        let edge = out
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Runs)
            .expect("runs edge");
        assert_eq!(edge.source, "host");
        assert_eq!(edge.target, "claim:docker:local:container:abc123");
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
        let (roots, kids, claims) = build_forest(&insts);
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
    fn topology_empty_input_emits_empty_graph() {
        let out = build_topology(&[], &BTreeMap::new());
        assert!(out.nodes.is_empty());
        assert!(out.edges.is_empty());
    }

    #[test]
    fn topology_mac_claim_emits_node_pair_and_edge() {
        let host = with_claim_mac(inst("host", "local", "host"), "aa:bb:cc:dd:ee:01");
        let guest = with_iface_mac(inst("guest", "system", "guest"), "AA:BB:CC:DD:EE:01");
        let out = build_topology(&[host, guest], &BTreeMap::new());
        assert_eq!(out.nodes.len(), 2);
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].source, "host");
        assert_eq!(out.edges[0].target, "guest");
        assert_eq!(out.edges[0].kind, EdgeKind::MacClaim);
    }

    #[test]
    fn topology_parent_peer_id_overrides_mac_claim() {
        let host_a = with_claim_mac(inst("host_a", "local", "ahost"), "aa:bb:cc:dd:ee:01");
        let host_b = inst("host_b", "system", "bhost");
        let mut guest = with_iface_mac(inst("guest", "system", "guest"), "aa:bb:cc:dd:ee:01");
        guest.system.as_mut().unwrap().parent_peer_id = Some("host_b".into());
        let out = build_topology(&[host_a, host_b, guest], &BTreeMap::new());
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].source, "host_b");
        assert_eq!(out.edges[0].target, "guest");
        assert_eq!(out.edges[0].kind, EdgeKind::ParentPeer);
    }

    #[test]
    fn topology_proxmox_ve_classified_as_host() {
        let i = with_system_type(inst("p", "local", "p"), "proxmox-ve");
        let out = build_topology(&[i], &BTreeMap::new());
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].kind, NodeKind::Host);
    }

    #[test]
    fn topology_lxc_virtualization_classified_as_lxc() {
        let mut i = inst("c", "system", "c");
        i.system.as_mut().unwrap().virtualization = Some("lxc".into());
        let out = build_topology(&[i], &BTreeMap::new());
        assert_eq!(out.nodes[0].kind, NodeKind::Lxc);
    }

    #[test]
    fn topology_cluster_emits_compound_parent() {
        let a = inst("a", "system", "a");
        let mut cluster_by_peer = BTreeMap::new();
        cluster_by_peer.insert("a".to_string(), "alpha".to_string());
        let out = build_topology(&[a], &cluster_by_peer);
        assert_eq!(out.nodes.len(), 2);
        let cluster_node = out
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Cluster)
            .unwrap();
        assert_eq!(cluster_node.id, "cluster:alpha");
        let peer_node = out.nodes.iter().find(|n| n.id == "a").unwrap();
        assert_eq!(peer_node.parent_id.as_deref(), Some("cluster:alpha"));
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
        let (roots, kids, claims) = build_forest(&[a, b, c]);
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
