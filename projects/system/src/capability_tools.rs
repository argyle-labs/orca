//! Shared projection types for the per-host capability registry.
//!
//! The capability surface collapsed onto the six canonical verbs: the read is
//! `system.detail{view=capabilities}` (this file's [`CapabilityListOutput`]),
//! and the imperatives are `system.update{action=enable_cap|disable_cap|
//! recheck_cap}` (in `commands.rs`). Both dispatch through the single daemon
//! handler so CLI / REST / MCP / UI share one path
//! ([[feedback-cli-api-mcp-one-path]]). The provider-registry logic itself
//! lives in `capability.rs`; this module only owns the wire projection.

use db::host_capabilities::HostCapability;
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

/// Read shape for `system.detail{view=capabilities}`: every provider this host
/// has ever probed or had set by an operator. Empty before the first daemon
/// startup probe runs.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityListOutput {
    pub capabilities: Vec<CapabilityRow>,
}
