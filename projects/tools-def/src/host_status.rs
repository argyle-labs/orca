//! `host_status` tools — persisted per-peer system snapshots.
//!
//! Two surfaces:
//!   * `host_status.list` — latest row for every peer present in the local
//!     DB. Drives the cross-mesh dashboard without a live RPC fanout.
//!   * `host_status.detail` — full snapshot history for one peer, with an
//!     optional `since` watermark. Used by the UI for charts and by the
//!     sync puller to ask peers for rows it doesn't have yet.
//!
//! Authority: the receiving host's DB owns its `peer_id=own` rows. Every
//! other row was mirrored from a peer via the pull-based sync task. Tools
//! never mutate state — writers live in the server's background tasks.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_lifecycle::SystemInfoReport;
use crate::orca_tool;

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct HostStatusRowDto {
    pub peer_id: String,
    pub snapshot_at_unix: i64,
    pub received_at_unix: i64,
    /// `"local"` = this host wrote it; `"synced"` = mirrored from a peer.
    pub source: String,
    /// Decoded snapshot. Absent if the stored payload couldn't be parsed
    /// (typically: a schema mismatch after an upgrade).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemInfoReport>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct HostStatusRows(pub Vec<HostStatusRowDto>);

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostStatusRowsArgs {}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostStatusDetailArgs {
    /// Peer whose history to read. Use `local` to read this host's own rows.
    pub peer_id: String,
    /// Return only rows with `snapshot_at_unix > since`. Omit to read the
    /// full retained history (capped at `MAX_ROWS_PER_PEER` in storage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_unix: Option<i64>,
    /// Maximum rows to return. Defaults to 256 — enough for a day at 1/min
    /// with room to spare; pass a lower value for sparkline-style queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[cfg(feature = "native")]
fn rows_to_dtos(rows: Vec<orca_db::host_status::HostStatusRow>) -> Vec<HostStatusRowDto> {
    rows.into_iter()
        .map(|r| {
            let system = serde_json::from_str::<SystemInfoReport>(&r.payload_json).ok();
            HostStatusRowDto {
                peer_id: r.peer_id,
                snapshot_at_unix: r.snapshot_at_unix,
                received_at_unix: r.received_at_unix,
                source: r.source,
                system,
            }
        })
        .collect()
}

/// Latest persisted snapshot per peer from the local DB. No network IO.
#[orca_tool(domain = "host_status", verb = "list", remote_ok = true)]
async fn host_status_list(
    _args: HostStatusRowsArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<HostStatusRows> {
    let conn = orca_db::open_default()?;
    let rows = orca_db::host_status::latest_per_peer(&conn)?;
    Ok(HostStatusRows(rows_to_dtos(rows)))
}

/// Snapshot history for one peer, newest-first. Both the UI (timeseries)
/// and the sync puller (watermarked pull) use this.
#[orca_tool(domain = "host_status", verb = "detail", remote_ok = true)]
async fn host_status_detail(
    args: HostStatusDetailArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<HostStatusRows> {
    let conn = orca_db::open_default()?;
    let limit = args.limit.unwrap_or(256) as usize;
    let rows = orca_db::host_status::rows_for_peer(&conn, &args.peer_id, args.since_unix, limit)?;
    Ok(HostStatusRows(rows_to_dtos(rows)))
}
