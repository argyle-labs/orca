//! Cross-crate topology types.
//!
//! `TopologyClaim` is emitted by colocated provider plugins (proxmox,
//! unraid, docker, ...) describing "this host runs that child" and consumed
//! by the system crate's inference task to derive parent_peer_id edges via
//! MAC matching. Lives here (not in `system`) so plugins can produce claims
//! without depending on `system`.
//!
//! ## Collector registry
//!
//! A colocated provider contributes claims through a [`TopologyCollector`]
//! registered into a process-global registry — either in-process or, for an
//! external cdylib plugin, a [`register_from_def`] JSON proxy the
//! plugin-loader installs for `domain = "topology"`. The system crate's
//! `collect_claims()` walks [`collectors`] so it stays plugin-agnostic, the
//! same way the `storage`/`notifications` domains already work.

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One child entity a host claims to run. The inference layer matches each
/// claim's `macs` against other peers' `interfaces[].mac` to derive
/// `parent_peer_id`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct TopologyClaim {
    /// `"vm"`, `"container"`, `"lxc"`.
    pub kind: String,
    /// Provider-native id (proxmox vmid, docker container id short, ...).
    pub id: String,
    pub name: String,
    /// MAC addresses associated with this child (lowercase, colon-separated).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macs: Vec<String>,
    /// Provider that emitted this claim (`"proxmox"`, `"docker"`,
    /// `"unraid"`, ...).
    pub provider: String,
    /// Provider instance id. For docker = `"local"`; for proxmox = the
    /// endpoint name from `db::proxmox`; for secret-keyed providers = the
    /// `<instance>` segment of `<provider>.<instance>.<field>`.
    pub provider_instance: String,
}

// ── Collector registry ──────────────────────────────────────────────────────

/// A source of [`TopologyClaim`]s — one per provider (proxmox, docker, …).
/// Registered into the process-global registry so the system crate's
/// `collect_claims()` can fan out across providers plugin-agnostically.
#[async_trait::async_trait]
pub trait TopologyCollector: Send + Sync {
    /// Provider/registry name (e.g. `"proxmox"`). Registry key; used to
    /// replace-in-place on re-register and to deregister on plugin unload.
    fn name(&self) -> &str;

    async fn collect_claims(&self) -> Result<Vec<TopologyClaim>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn TopologyCollector>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a topology collector with the process-global registry.
/// Re-registering the same `name()` replaces the existing entry so a dev
/// rebuild / plugin reload doesn't duplicate collectors.
pub fn register_collector(collector: Arc<dyn TopologyCollector>) {
    let mut g = GLOBAL.write().expect("topology registry poisoned");
    let name = collector.name().to_string();
    if let Some(slot) = g.iter_mut().find(|c| c.name() == name) {
        *slot = collector;
    } else {
        g.push(collector);
    }
}

/// Snapshot of every registered collector.
pub fn collectors() -> Vec<Arc<dyn TopologyCollector>> {
    GLOBAL.read().expect("topology registry poisoned").clone()
}

/// Deregister the collector named `name`, if present. The reversal path a
/// plugin unload needs. Returns `true` if a collector was removed.
pub fn deregister_collector(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("topology registry poisoned");
    let before = g.len();
    g.retain(|c| c.name() != name);
    before != g.len()
}

/// The synchronous invoke thunk a cdylib plugin's topology collector is driven
/// through: `(op, args_json) -> Result<result_json, error_string>`. Plain `Fn`
/// of strings so `contract` stays free of any ABI/loader dependency (no cycle).
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

/// Operation name the [`TopologyCollectorProxy`] invokes across the FFI
/// boundary. The plugin exposes a tool `"{invoke_prefix}.{COLLECT_OP}"`
/// returning a JSON `Vec<TopologyClaim>`.
pub const COLLECT_OP: &str = "collect_claims";

/// Build and register a [`TopologyCollector`] from a plugin backend descriptor
/// plus an [`InvokeThunk`]. The cdylib plugin-loader calls this from its domain
/// dispatch table for `domain = "topology"`.
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_collector(Arc::new(TopologyCollectorProxy { name, invoke }));
    Ok(())
}

/// A [`TopologyCollector`] backed by a cdylib plugin reached over the
/// JSON-proxy FFI boundary. `collect_claims()` offloads the synchronous
/// [`InvokeThunk`] onto `spawn_blocking` and deserializes the JSON result.
struct TopologyCollectorProxy {
    name: String,
    invoke: InvokeThunk,
}

#[async_trait::async_trait]
impl TopologyCollector for TopologyCollectorProxy {
    fn name(&self) -> &str {
        &self.name
    }

    async fn collect_claims(&self) -> Result<Vec<TopologyClaim>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        let out = tokio::task::spawn_blocking(move || invoke(COLLECT_OP, "{}".to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("topology '{name}' invoke task panicked: {e}"))?
            .map_err(|e| anyhow::anyhow!("topology '{name}' invoke failed: {e}"))?;
        let claims: Vec<TopologyClaim> = serde_json::from_str(&out)
            .map_err(|e| anyhow::anyhow!("topology '{name}' returned invalid JSON: {e}"))?;
        Ok(claims)
    }
}
