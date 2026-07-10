//! Trait for discovering pod-host cluster membership without depending on
//! any specific virtualization-platform plugin.
//!
//! Domain crates that want to group peers by cluster (the systems UI being
//! the canonical consumer) resolve a `ClusterRoster` service from `ToolCtx`
//! and walk its `list_clusters()` output. Plugins (proxmox today, others
//! later) register concrete impls — in-process or, for an external cdylib
//! plugin, a [`register_from_def`] JSON proxy — at daemon start so the
//! rollup stays plugin-agnostic.
//!
//! ## Registry + aggregator
//!
//! Providers register into a process-global registry ([`register_backend`] /
//! [`register_from_def`]). The host installs a single [`AggregateClusterRoster`]
//! as the `ToolCtx` service; it fans `list_clusters()` out across every
//! registered provider so a consumer sees one roster regardless of how many
//! plugins contribute. This mirrors the `storage`/`notifications` domain
//! registries the cdylib plugin-loader already drives.

use std::sync::{Arc, LazyLock, RwLock};

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
    /// Provider/registry name (e.g. `"proxmox"`). The registry key; used to
    /// replace-in-place on re-register and to deregister on plugin unload.
    fn name(&self) -> &str;

    async fn list_clusters(&self) -> Result<Vec<ClusterEntry>>;
}

// ── Process-global registry ─────────────────────────────────────────────────

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn ClusterRoster>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a cluster-roster provider with the process-global registry.
/// Re-registering the same `name()` replaces the existing entry so a dev
/// rebuild / plugin reload doesn't duplicate providers.
pub fn register_backend(backend: Arc<dyn ClusterRoster>) {
    let mut g = GLOBAL.write().expect("cluster_roster registry poisoned");
    let name = backend.name().to_string();
    if let Some(slot) = g.iter_mut().find(|b| b.name() == name) {
        *slot = backend;
    } else {
        g.push(backend);
    }
}

/// Snapshot of every registered provider.
pub fn backends() -> Vec<Arc<dyn ClusterRoster>> {
    GLOBAL
        .read()
        .expect("cluster_roster registry poisoned")
        .clone()
}

/// Deregister the provider named `name`, if present. The reversal path a
/// plugin unload needs so a dropped cdylib leaves no roster pointing at a dead
/// invoke thunk. Returns `true` if a provider was removed.
pub fn deregister_backend(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("cluster_roster registry poisoned");
    let before = g.len();
    g.retain(|b| b.name() != name);
    before != g.len()
}

/// The synchronous invoke thunk a cdylib plugin's roster backend is driven
/// through: `(op, args_json) -> Result<result_json, error_string>`. The loader
/// supplies a closure that marshals `op` into a `"{invoke_prefix}.{op}"` tool
/// call across the FFI `invoke` boundary. Plain `Fn` of strings so `contract`
/// stays free of any dependency on the ABI/loader crates (no cycle).
///
/// Host-side cdylib proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

/// Operation name the [`ClusterRosterProxy`] invokes across the FFI boundary.
/// The plugin exposes a tool `"{invoke_prefix}.{ROSTER_OP}"` returning a JSON
/// `Vec<ClusterEntry>`.
pub const ROSTER_OP: &str = "list_clusters";

/// Build and register a [`ClusterRoster`] from a plugin backend descriptor
/// plus an [`InvokeThunk`]. The cdylib plugin-loader calls this from its domain
/// dispatch table for `domain = "cluster_roster"`. Registration replaces any
/// existing provider of the same name (idempotent reload).
///
/// Host-side cdylib proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_backend(Arc::new(ClusterRosterProxy { name, invoke }));
    Ok(())
}

/// A [`ClusterRoster`] backed by a cdylib plugin reached over the JSON-proxy
/// FFI boundary. `list_clusters()` offloads the synchronous [`InvokeThunk`]
/// onto `spawn_blocking` (so a slow/wedged plugin never blocks the async
/// runtime) and deserializes the JSON `Vec<ClusterEntry>` result.
///
/// Host-side cdylib proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
struct ClusterRosterProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
#[async_trait::async_trait]
impl ClusterRoster for ClusterRosterProxy {
    fn name(&self) -> &str {
        &self.name
    }

    async fn list_clusters(&self) -> Result<Vec<ClusterEntry>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        let out = tokio::task::spawn_blocking(move || invoke(ROSTER_OP, "{}".to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("cluster_roster '{name}' invoke task panicked: {e}"))?
            .map_err(|e| anyhow::anyhow!("cluster_roster '{name}' invoke failed: {e}"))?;
        let entries: Vec<ClusterEntry> = serde_json::from_str(&out)
            .map_err(|e| anyhow::anyhow!("cluster_roster '{name}' returned invalid JSON: {e}"))?;
        Ok(entries)
    }
}

/// The single [`ClusterRoster`] the host installs as the `ToolCtx` service.
/// Fans `list_clusters()` out across every registered provider and
/// concatenates, so a consumer (`pod.snapshot`, inventory) sees one roster
/// regardless of how many plugins contribute. A provider that errors is logged
/// and skipped — one broken plugin must not blank out the whole roster.
pub struct AggregateClusterRoster;

#[async_trait::async_trait]
impl ClusterRoster for AggregateClusterRoster {
    fn name(&self) -> &str {
        "aggregate"
    }

    async fn list_clusters(&self) -> Result<Vec<ClusterEntry>> {
        let mut out = Vec::new();
        for backend in backends() {
            match backend.list_clusters().await {
                Ok(mut v) => out.append(&mut v),
                Err(e) => tracing::warn!(
                    provider = %backend.name(),
                    error = %e,
                    "cluster roster provider failed",
                ),
            }
        }
        Ok(out)
    }
}
