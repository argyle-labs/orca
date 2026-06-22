//! Server-side inventory aggregator. `inventory.tree` returns a nested
//! cluster → roots → recursive nodes structure the systems UI renders as
//! visually-contained parent cards wrapping child cards.
//!
//! - Parent inference by MAC-claim matching (`system.claims[].macs`
//!   intersected with `system.interfaces[].mac`).
//! - `system.parent_peer_id` overrides MAC inference when set and the
//!   referenced peer exists.
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
    pub instance: PodInstance,
    pub children: Vec<InventoryNode>,
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
/// `InventoryNode` trees (parent-inferred via MAC claims, with
/// `system.parent_peer_id` overrides). The UI renders each node as a card
/// that visually contains its children.
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
    let cluster_by_peer = match_clusters_instances(&instances, &clusters);
    let summaries = build_cluster_summaries(&clusters);

    let (roots, children_of) = build_forest(&instances);
    let bucketed = bucket_roots(roots, &children_of, &cluster_by_peer, &summaries);
    Ok(InventoryTreeOutput { clusters: bucketed })
}

// ── Algorithm ───────────────────────────────────────────────────────────────

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

/// Build the parent-inference forest. Returns roots (sorted: local first,
/// then alphabetic) and a `peer_id -> sorted children` map.
fn build_forest(
    instances: &[PodInstance],
) -> (Vec<PodInstance>, HashMap<String, Vec<PodInstance>>) {
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

    (roots, children_of)
}

/// Recursively materialize a node and its descendants. `visited` guards
/// against cycles (shouldn't happen with current inference rules).
fn build_node(
    inst: &PodInstance,
    children_of: &HashMap<String, Vec<PodInstance>>,
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
            children.push(build_node(k, children_of, visited));
        }
    }
    InventoryNode {
        instance: inst.clone(),
        children,
    }
}

/// Bucket roots by cluster. Named clusters alphabetical; ungrouped bucket
/// (None) trails. When no clusters are configured, returns a single
/// ungrouped bucket holding every root.
fn bucket_roots(
    roots: Vec<PodInstance>,
    children_of: &HashMap<String, Vec<PodInstance>>,
    cluster_by_peer: &BTreeMap<String, String>,
    summaries: &HashMap<String, ClusterSummary>,
) -> Vec<InventoryCluster> {
    let mut visited: HashSet<String> = HashSet::new();

    if summaries.is_empty() {
        let nodes: Vec<InventoryNode> = roots
            .iter()
            .map(|r| build_node(r, children_of, &mut visited))
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
        let node = build_node(root, children_of, &mut visited);
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
    NfsMount,
    Network,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct NetworkTopologyArgs {}

/// Network topology graph for the systems view. Returns a flat node + edge
/// list (NOT a nested tree) suitable for force-directed canvas rendering.
/// Nodes are peers; edges are parent-inference relationships (MAC-claim or
/// explicit `parent_peer_id` overrides). Clusters surface as compound nodes
/// when `ClusterRoster` populates them.
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
    let cluster_by_peer = match_clusters_instances(&instances, &clusters);

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

    NetworkTopologyOutput { nodes, edges }
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
        });
        i
    }

    fn bucket_empty(
        roots: Vec<PodInstance>,
        children_of: &HashMap<String, Vec<PodInstance>>,
    ) -> Vec<InventoryCluster> {
        bucket_roots(roots, children_of, &BTreeMap::new(), &HashMap::new())
    }

    #[test]
    fn single_root_no_children_emits_one_node() {
        let only = inst("only", "local", "only");
        let (roots, kids) = build_forest(&[only]);
        let out = bucket_empty(roots, &kids);
        assert_eq!(out.len(), 1);
        assert!(out[0].name.is_none());
        assert_eq!(out[0].roots.len(), 1);
        assert_eq!(out[0].roots[0].instance.peer_id, "only");
        assert!(out[0].roots[0].children.is_empty());
    }

    #[test]
    fn root_with_two_children_sorted_alphabetic() {
        let host = with_claim_mac(inst("host", "local", "host"), "aa:bb:cc:dd:ee:01");
        let host = with_claim_mac(host, "aa:bb:cc:dd:ee:02");
        let kid_b = with_iface_mac(inst("kb", "system", "beta"), "aa:bb:cc:dd:ee:01");
        let kid_a = with_iface_mac(inst("ka", "system", "alpha"), "aa:bb:cc:dd:ee:02");
        let (roots, kids) = build_forest(&[host, kid_b, kid_a]);
        let out = bucket_empty(roots, &kids);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].roots.len(), 1);
        let root = &out[0].roots[0];
        assert_eq!(root.instance.peer_id, "host");
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].instance.peer_id, "ka");
        assert_eq!(root.children[1].instance.peer_id, "kb");
    }

    #[test]
    fn multi_root_local_first_then_alphabetic() {
        let a = inst("a", "system", "alpha");
        let local = inst("z", "local", "zulu");
        let m = inst("m", "system", "mike");
        let (roots, kids) = build_forest(&[a, local, m]);
        let out = bucket_empty(roots, &kids);
        assert_eq!(out[0].roots.len(), 3);
        assert_eq!(out[0].roots[0].instance.peer_id, "z");
        assert_eq!(out[0].roots[1].instance.peer_id, "a");
        assert_eq!(out[0].roots[2].instance.peer_id, "m");
    }

    #[test]
    fn mac_inference_nests_child_under_host() {
        let host = with_claim_mac(inst("host", "system", "host"), "aa:bb:cc:dd:ee:01");
        let guest = with_iface_mac(inst("guest", "system", "guest"), "AA:BB:CC:DD:EE:01");
        let (roots, kids) = build_forest(&[host, guest]);
        let out = bucket_empty(roots, &kids);
        assert_eq!(out[0].roots.len(), 1);
        assert_eq!(out[0].roots[0].instance.peer_id, "host");
        assert_eq!(out[0].roots[0].children.len(), 1);
        assert_eq!(out[0].roots[0].children[0].instance.peer_id, "guest");
    }

    #[test]
    fn parent_peer_id_override_takes_precedence_over_mac() {
        // Two potential hosts; child has MAC matching host_a's claim, but
        // parent_peer_id pins to host_b.
        let host_a = with_claim_mac(inst("host_a", "local", "ahost"), "aa:bb:cc:dd:ee:01");
        let host_b = inst("host_b", "system", "bhost");
        let mut guest = with_iface_mac(inst("guest", "system", "guest"), "aa:bb:cc:dd:ee:01");
        guest.system.as_mut().unwrap().parent_peer_id = Some("host_b".into());
        let (roots, kids) = build_forest(&[host_a, host_b, guest]);
        let out = bucket_empty(roots, &kids);
        // Two roots: host_a and host_b. guest nests under host_b.
        let root_b = out[0]
            .roots
            .iter()
            .find(|n| n.instance.peer_id == "host_b")
            .unwrap();
        assert_eq!(root_b.children.len(), 1);
        assert_eq!(root_b.children[0].instance.peer_id, "guest");
        let root_a = out[0]
            .roots
            .iter()
            .find(|n| n.instance.peer_id == "host_a")
            .unwrap();
        assert!(root_a.children.is_empty());
    }

    #[test]
    fn orphan_with_unknown_parent_peer_id_surfaces_as_root() {
        let mut orphan = inst("orphan", "system", "orphan");
        orphan.system.as_mut().unwrap().parent_peer_id = Some("ghost".into());
        let (roots, kids) = build_forest(&[orphan]);
        let out = bucket_empty(roots, &kids);
        assert_eq!(out[0].roots.len(), 1);
        assert_eq!(out[0].roots[0].instance.peer_id, "orphan");
    }

    fn with_system_type(mut i: PodInstance, t: &str) -> PodInstance {
        i.system.as_mut().unwrap().system_type = Some(t.to_string());
        i
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
        let (roots, kids) = build_forest(&[a, b, c]);
        let out = bucket_roots(roots, &kids, &cluster_by_peer, &summaries);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].name.as_deref(), Some("alpha"));
        assert_eq!(out[0].roots.len(), 1);
        assert_eq!(out[0].roots[0].instance.peer_id, "c");
        assert_eq!(out[1].name.as_deref(), Some("zeta"));
        assert_eq!(out[1].roots[0].instance.peer_id, "b");
        assert!(out[2].name.is_none());
        assert_eq!(out[2].roots[0].instance.peer_id, "a");
    }
}
