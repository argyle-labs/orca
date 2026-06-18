//! Trait for discovering pod-host cluster membership without depending on
//! any specific virtualization-platform plugin.
//!
//! Domain crates that want to group peers by cluster (the systems UI being
//! the canonical consumer) resolve a `ClusterRoster` service from `ToolCtx`
//! and walk its `list_clusters()` output. Plugins (proxmox today, others
//! later) register concrete impls at daemon start so the rollup stays
//! plugin-agnostic.

use anyhow::Result;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ClusterEntry {
    /// Logical endpoint name (e.g. the proxmox endpoint the cluster was
    /// fetched from). Multiple endpoints can report the same cluster.
    pub endpoint: String,
    /// Cluster name. `None` for standalone hosts that report no cluster.
    pub name: Option<String>,
    pub quorate: Option<bool>,
    pub nodes: Vec<ClusterNode>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ClusterNode {
    pub name: String,
    pub ip: Option<String>,
    pub online: Option<bool>,
}

#[async_trait::async_trait]
pub trait ClusterRoster: Send + Sync {
    async fn list_clusters(&self) -> Result<Vec<ClusterEntry>>;
}
