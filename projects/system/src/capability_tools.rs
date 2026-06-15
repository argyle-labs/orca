//! Tool surface for the per-host capability registry.
//!
//! Four verbs, all dispatched through the single daemon handler so
//! CLI / REST / MCP / UI share one path
//! ([[feedback-cli-api-mcp-one-path]]):
//!
//! * `system.capability.list`    — read every row
//! * `system.capability.recheck` — re-probe one provider
//! * `system.capability.disable` — force `Disabled` (sticky across restarts)
//! * `system.capability.enable`  — clear `Disabled` and immediately re-probe

use crate::capability;
use db::host_capabilities::HostCapability;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Flat, JsonSchema-friendly projection of [`HostCapability`]. `state`
/// is a string at the boundary so consumers (UI / scripts) can match on
/// the literal values without depending on an enum type, mirroring the
/// `runtime` field on `containers.unhold` / `containers.unwedge`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityRow {
    pub provider: String,
    /// `available` | `absent` | `disabled`.
    pub state: String,
    /// Unix epoch seconds when the row was last written. Advances on
    /// probe, disable, and enable.
    pub last_probed: i64,
    /// Failure reason (Absent) or operator-supplied note (Disabled).
    /// `None` when Available.
    pub reason: Option<String>,
    /// Version string when Available (e.g. docker server version).
    pub detail: Option<String>,
}

impl From<HostCapability> for CapabilityRow {
    fn from(r: HostCapability) -> Self {
        CapabilityRow {
            provider: r.provider,
            state: r.state.as_str().to_string(),
            last_probed: r.last_probed,
            reason: r.reason,
            detail: r.detail,
        }
    }
}

// ── list ─────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CapabilityListArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityListOutput {
    pub capabilities: Vec<CapabilityRow>,
}

/// Every provider this host has ever probed or had set by an operator.
/// Returns an empty list before the first daemon startup probe runs.
#[orca_tool(domain = "system", verb = "capability_list")]
async fn system_capability_list(
    _args: CapabilityListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<CapabilityListOutput> {
    let rows = capability::list()?;
    Ok(CapabilityListOutput {
        capabilities: rows.into_iter().map(Into::into).collect(),
    })
}

// ── recheck ──────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityRecheckArgs {
    /// Provider name (e.g. `docker`, `proxmox`). Must match one of the
    /// built-in probes. Disabled rows are NOT re-probed by this verb —
    /// use `enable` to clear Disabled and probe in one step.
    #[arg(long)]
    pub name: String,
}

/// Force a fresh probe of one provider. Persists + returns the new
/// state. No-op for Disabled rows (operator intent wins).
#[orca_tool(domain = "system", verb = "capability_recheck")]
async fn system_capability_recheck(
    args: CapabilityRecheckArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<CapabilityRow> {
    let row = capability::recheck(&args.name).await?;
    Ok(row.into())
}

// ── disable ──────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDisableArgs {
    /// Provider name. Must match a built-in probe.
    #[arg(long)]
    pub name: String,
    /// Operator-visible reason, e.g. "intentionally off on this host".
    #[arg(long)]
    pub reason: String,
}

/// Mark a provider `Disabled`. Sticky across daemon restarts —
/// `probe_all_capabilities` leaves Disabled rows alone. Idempotent.
#[orca_tool(domain = "system", verb = "capability_disable")]
async fn system_capability_disable(
    args: CapabilityDisableArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<CapabilityRow> {
    let row = capability::disable(&args.name, &args.reason)?;
    Ok(row.into())
}

// ── enable ───────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityEnableArgs {
    /// Provider name. Must match a built-in probe.
    #[arg(long)]
    pub name: String,
}

/// Clear a `Disabled` row and immediately re-probe. Returned row
/// reflects whichever state the live probe lands in (`Available` or
/// `Absent`). No-op-like when the row wasn't Disabled — still re-probes
/// so the returned state is fresh.
#[orca_tool(domain = "system", verb = "capability_enable")]
async fn system_capability_enable(
    args: CapabilityEnableArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<CapabilityRow> {
    let row = capability::enable(&args.name).await?;
    Ok(row.into())
}
