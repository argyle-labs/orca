//! Fleet-update seam: result types + the hook `system.update` with no `--id`
//! drives.
//!
//! The fan-out itself lives in the `mesh` crate (it needs the peer roster and
//! the mesh transport). This crate owns only the TYPES and the trait, so
//! `system.update` can expose the fleet scope without depending on `mesh` —
//! the same seam shape as [`crate::host::HostRefreshHook`]. The server wires
//! mesh's implementation in at startup.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Outcome of one fleet-update row, daemon or plugin.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateRowStatus {
    UpToDate,
    /// A newer version exists and this host can obtain it.
    UpdateAvailable,
    /// This run applied a new version (execute only).
    Updated,
    /// A newer version exists but this host cannot fetch it.
    Blocked,
    Failed,
    /// The host's answer does not decide the outcome.
    #[default]
    Unknown,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct FleetSystemResult {
    /// Display hostname of the host.
    pub host: String,
    /// The system's id; empty for the local host.
    ///
    /// Systems are addressed by this id, and orca resolves routes to reach them.
    /// It is the same value `system.health` reports as `machineId`, and it is
    /// stored in `mesh_peers.peer_id` pending that table's rename to `systems`.
    pub id: String,
    /// Current daemon version probed on the host.
    pub current: Option<String>,
    /// Channel-latest the host would move to.
    pub target: Option<String>,
    /// Version applied; set only when `status` is `updated`.
    pub applied: Option<String>,
    pub status: UpdateRowStatus,
    /// Why the row is `blocked`, `failed` or `unknown`.
    pub reason: Option<String>,
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
    /// Version installed by this run; set only when `status` is `updated`.
    pub applied: Option<String>,
    pub status: UpdateRowStatus,
    /// Why the row is `blocked`, `failed` or `unknown`, or why an
    /// `up_to_date` row was left alone (e.g. installed newer than the catalog).
    pub reason: Option<String>,
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
    /// Absolute path of the run record this fan-out writes after every host.
    /// Surfaced as a field, not only as a note, because it is the ONLY way a
    /// caller recovers the outcome of an `execute` roll: the last act of the
    /// roll restarts the local daemon and severs the caller's connection
    /// (#625). `None` when no orca home was resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_record: Option<String>,
}

/// What an operator asked a fleet roll to do. A struct rather than a widening
/// argument list so adding a knob does not churn every implementor of
/// [`FleetUpdateHook`].
#[derive(Debug, Clone, Copy, Default)]
pub struct FleetUpdateRequest {
    /// Apply. Without it the fan-out only probes (and takes no fleet lock:
    /// a read-only probe is not a roll and must never block one).
    pub execute: bool,
    /// Resolve plugins to their newest prerelease rather than newest stable.
    pub prerelease: bool,
    /// Take the fleet lock even though another roll holds it. For a genuinely
    /// stuck lock only — it defeats the single-flight guarantee (#616).
    pub break_lock: bool,
    /// Skip the plugin phase: daemon versions only. Makes the commonest
    /// question of this verb cheap (#626).
    pub daemons_only: bool,
}

/// Hook the server registers at startup so a fleet-wide `system.update` can
/// drive mesh's fleet fan-out without this domain crate depending on `mesh`.
#[async_trait::async_trait]
pub trait FleetUpdateHook: Send + Sync {
    async fn fleet_update(
        &self,
        req: FleetUpdateRequest,
        ctx: &contract::ToolCtx,
    ) -> anyhow::Result<FleetUpdateOutput>;
}

pub trait ProvideFleetUpdate {
    fn fleet_update(&self) -> std::sync::Arc<dyn FleetUpdateHook + Send + Sync>;
}

pub fn register_fleet_update(ctx: &mut contract::ToolCtx, p: &impl ProvideFleetUpdate) {
    ctx.register_service(p.fleet_update());
}
