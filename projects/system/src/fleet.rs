//! Fleet-update seam: result types + the hook `system.update --scope fleet`
//! drives.
//!
//! The fan-out itself lives in the `pod` crate (it needs the peer roster and
//! the mesh transport). This crate owns only the TYPES and the trait, so
//! `system.update` can expose the fleet scope without depending on `pod` —
//! the same seam shape as [`crate::host::HostRefreshHook`]. The server wires
//! pod's implementation in at startup.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct FleetSystemResult {
    /// Display hostname of the host.
    pub host: String,
    /// Peer id (empty for the local host).
    pub peer_id: String,
    /// Current daemon version probed on the host.
    pub current: Option<String>,
    /// Channel-latest the host would move to.
    pub target: Option<String>,
    /// True when `target` is strictly newer than `current`.
    pub update_available: bool,
    /// Version applied (execute only); `None` on a dry run or no-op.
    pub applied: Option<String>,
    /// Per-host error (probe/apply/health-gate). The fan-out continues past it.
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct FleetPluginResult {
    /// Display hostname of the host.
    pub host: String,
    /// Plugin / target-software name.
    pub name: String,
    /// Installed version on the host, or `None` when not installed.
    pub installed: Option<String>,
    /// Newest version resolved for the plugin.
    pub target: Option<String>,
    /// True when `target` is strictly newer than `installed`.
    pub update_available: bool,
    /// True when this plugin was actually (re)installed (execute only).
    pub updated: bool,
    /// Human-readable note — e.g. why an unreleased/sideloaded plugin was skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Per-plugin error. The fan-out continues past it.
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct FleetUpdateOutput {
    /// True when nothing was applied (no `--execute`).
    pub dry_run: bool,
    /// Per-host daemon update plan/results, in apply order (local last).
    pub systems: Vec<FleetSystemResult>,
    /// Per-host, per-plugin update plan/results.
    pub plugins: Vec<FleetPluginResult>,
    /// Human-readable progress/summary notes.
    pub notes: Vec<String>,
    /// Fan-out-level errors (peer enumeration, etc.), distinct from per-host ones.
    pub errors: Vec<String>,
}

/// Hook the server registers at startup so `system.update --scope fleet` can
/// drive pod's fleet fan-out without this domain crate depending on `pod`.
#[async_trait::async_trait]
pub trait FleetUpdateHook: Send + Sync {
    async fn fleet_update(
        &self,
        execute: bool,
        prerelease: bool,
        ctx: &contract::ToolCtx,
    ) -> anyhow::Result<FleetUpdateOutput>;
}

pub trait ProvideFleetUpdate {
    fn fleet_update(&self) -> std::sync::Arc<dyn FleetUpdateHook + Send + Sync>;
}

pub fn register_fleet_update(ctx: &mut contract::ToolCtx, p: &impl ProvideFleetUpdate) {
    ctx.register_service(p.fleet_update());
}
