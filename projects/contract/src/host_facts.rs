//! Host-self-facts: a plugin reporting facts about the host it runs on.
//!
//! Some host-level attributes are only knowable through a platform-specific
//! API that a plugin owns — e.g. a Proxmox node's cluster membership, which
//! the proxmox plugin reads from the PVE API (`/cluster/status`). Core must
//! not embed that platform knowledge (it stays generic), yet the fact needs
//! to land in the host's mesh-propagated `system` snapshot so any vantage —
//! including a laptop with no proxmox plugin loaded — can consume it.
//!
//! A colocated provider contributes facts through a [`HostFactsProvider`]
//! registered into a process-global registry — either in-process or, for an
//! external cdylib plugin, a [`register_from_def`] JSON proxy the
//! plugin-loader installs for `domain = "host_facts"`. The `system` crate's
//! snapshot refresher walks [`providers`] and folds the results into the
//! snapshot, the same way the `topology` domain already works.

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Facts a plugin reports about its own host. Extensible: fields are additive
/// and optional so a provider fills only what it knows and the merge in the
/// `system` crate takes the first non-empty value per field.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct HostFacts {
    /// Cluster this host belongs to, when the provider can determine it
    /// (e.g. the proxmox corosync cluster name, via the PVE API). `None`
    /// when the host is standalone or the provider can't tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
}

impl HostFacts {
    /// Fold `other` into `self`, keeping the first non-empty value per field.
    pub fn merge(&mut self, other: HostFacts) {
        if self.cluster.is_none() {
            self.cluster = other.cluster;
        }
    }
}

// ── Provider registry ────────────────────────────────────────────────────────

/// A source of [`HostFacts`] about the local host — one per provider.
/// Registered into the process-global registry so the system crate's snapshot
/// refresher can fan out across providers plugin-agnostically.
#[async_trait::async_trait]
pub trait HostFactsProvider: Send + Sync {
    /// Provider/registry name (e.g. `"proxmox"`). Registry key; used to
    /// replace-in-place on re-register and to deregister on plugin unload.
    fn name(&self) -> &str;

    async fn get_facts(&self) -> Result<HostFacts>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn HostFactsProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a host-facts provider with the process-global registry.
/// Re-registering the same `name()` replaces the existing entry so a dev
/// rebuild / plugin reload doesn't duplicate providers.
pub fn register_provider(provider: Arc<dyn HostFactsProvider>) {
    let mut g = GLOBAL.write().expect("host_facts registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Snapshot of every registered provider.
pub fn providers() -> Vec<Arc<dyn HostFactsProvider>> {
    GLOBAL.read().expect("host_facts registry poisoned").clone()
}

/// Deregister the provider named `name`, if present. The reversal path a
/// plugin unload needs. Returns `true` if a provider was removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("host_facts registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

/// Aggregate every registered provider's facts into one [`HostFacts`],
/// first-non-empty-wins. A provider that errors is logged and skipped so one
/// broken plugin never blanks the whole result. The `system` crate calls this
/// on its refresh cadence and stamps the result into the snapshot.
pub async fn collect() -> HostFacts {
    let mut out = HostFacts::default();
    for provider in providers() {
        match provider.get_facts().await {
            Ok(f) => out.merge(f),
            Err(e) => tracing::warn!(
                provider = %provider.name(),
                error = %e,
                "host_facts provider failed",
            ),
        }
    }
    out
}

// ── FFI proxy for cdylib plugins ──────────────────────────────────────────────

/// The synchronous invoke thunk a cdylib plugin's host-facts provider is driven
/// through: `(op, args_json) -> Result<result_json, error_string>`. Plain `Fn`
/// of strings so `contract` stays free of any ABI/loader dependency (no cycle).
///
/// Host-side cdylib proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

/// Operation name the [`HostFactsProxy`] invokes across the FFI boundary.
/// The plugin exposes a tool `"{invoke_prefix}.{FACTS_OP}"` returning a JSON
/// [`HostFacts`].
pub const FACTS_OP: &str = "get_facts";

/// Build and register a [`HostFactsProvider`] from a plugin backend descriptor
/// plus an [`InvokeThunk`]. The cdylib plugin-loader calls this from its domain
/// dispatch table for `domain = "host_facts"`.
///
/// Host-side cdylib proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_provider(Arc::new(HostFactsProxy { name, invoke }));
    Ok(())
}

/// A [`HostFactsProvider`] backed by a cdylib plugin reached over the JSON-proxy
/// FFI boundary. `get_facts()` offloads the synchronous [`InvokeThunk`] onto
/// `spawn_blocking` and deserializes the JSON result.
///
/// Host-side cdylib proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
struct HostFactsProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
#[async_trait::async_trait]
impl HostFactsProvider for HostFactsProxy {
    fn name(&self) -> &str {
        &self.name
    }

    async fn get_facts(&self) -> Result<HostFacts> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        let out = tokio::task::spawn_blocking(move || invoke(FACTS_OP, "{}".to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("host_facts '{name}' invoke task panicked: {e}"))?
            .map_err(|e| anyhow::anyhow!("host_facts '{name}' invoke failed: {e}"))?;
        let facts: HostFacts = serde_json::from_str(&out)
            .map_err(|e| anyhow::anyhow!("host_facts '{name}' returned invalid JSON: {e}"))?;
        Ok(facts)
    }
}
