//! Proxmox cluster-status wrapper.
//!
//! Wraps the generated `GET /cluster/status` call into a typed shape that
//! splits the single response array into the cluster envelope (name +
//! quorate) and its member nodes. A standalone Proxmox host (no cluster
//! configured) returns an array with only a node-typed item — we surface
//! that as `name: None, quorate: None, nodes: [that single node]` instead
//! of erroring, matching how the systems UI wants to render it.
//!
//! Consumed by `tools::proxmox_cluster_status` / `proxmox_cluster_list`
//! and by the frontend systems map for grouping peers by cluster.

use crate::generated::{self, types as gtypes};

#[derive(Debug, Clone)]
pub struct ClusterStatus {
    /// Cluster name. `None` when the endpoint is standalone (no
    /// corosync cluster configured).
    pub name: Option<String>,
    /// Quorate flag from the cluster envelope. `None` on standalone.
    pub quorate: Option<bool>,
    /// All node entries reported by `/cluster/status`. For standalone
    /// this is the single responding node.
    pub nodes: Vec<ClusterNode>,
}

#[derive(Debug, Clone)]
pub struct ClusterNode {
    pub name: String,
    pub ip: Option<String>,
    pub online: Option<bool>,
    pub node_id: Option<i64>,
    pub local: Option<bool>,
}

/// Hit `/cluster/status` on the supplied client and partition the
/// response into cluster envelope + node list.
pub async fn fetch_cluster_status(client: &generated::Client) -> anyhow::Result<ClusterStatus> {
    let items = client
        .get_get_status_cluster_status()
        .await
        .map_err(|e| anyhow::anyhow!("proxmox cluster_status: {e}"))?
        .into_inner();

    let mut cluster_name: Option<String> = None;
    let mut quorate: Option<bool> = None;
    let mut nodes: Vec<ClusterNode> = Vec::new();

    for item in items {
        match item.type_ {
            gtypes::GetGetStatusClusterStatusResponseItemType::Cluster => {
                cluster_name = Some(item.name);
                quorate = item.quorate;
            }
            gtypes::GetGetStatusClusterStatusResponseItemType::Node => {
                nodes.push(ClusterNode {
                    name: item.name,
                    ip: item.ip,
                    online: item.online,
                    node_id: item.nodeid,
                    local: item.local,
                });
            }
        }
    }

    Ok(ClusterStatus {
        name: cluster_name,
        quorate,
        nodes,
    })
}
