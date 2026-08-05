//! `system.history` — cursor-paginated read of this host's `system` metrics
//! time-series from the encrypted history subsystem (`db::metrics`).
//!
//! This is the on-demand companion to the now-lean `system.detail`: the history
//! ring lives here, not embedded in the detail snapshot. Peer-dispatchable, so
//! `system history --peer <host>` reads a remote host's series over the mesh.
//! It is the template for future `<domain>.history` verbs, all thin projections
//! over the one generic subsystem.

use crate::system_info::history::SERIES;
use crate::system_info_types::SystemHistoryPoint;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemHistoryArgs {
    /// Max samples to return (clamped to [1, 5000]; default 200).
    #[arg(long)]
    pub limit: Option<usize>,
    /// Opaque cursor from a previous page's `nextCursor` (a `ts_ms` upper
    /// bound). Omit for the newest window; pass back to page older.
    #[arg(long)]
    pub cursor: Option<i64>,
    /// Lower bound: only samples at/after this unix-seconds time. Omit for the
    /// beginning of retained history.
    #[arg(long)]
    pub since: Option<i64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SystemHistoryOutput {
    /// Samples, newest-first.
    pub points: Vec<SystemHistoryPoint>,
    /// Cursor for the next (older) page, or `None` when the window is
    /// exhausted. Pass back as `cursor`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<i64>,
}

/// Read a cursor-paginated window of the host's `system` metric history.
#[orca_tool(domain = "system", verb = "history")]
async fn system_history(
    args: SystemHistoryArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SystemHistoryOutput> {
    let limit = args.limit.unwrap_or(200);
    let end_ms = args.cursor.unwrap_or(i64::MAX);
    let start_ms = args.since.map(|s| s.saturating_mul(1000)).unwrap_or(0);

    let page = db::metrics::query(SERIES, start_ms, end_ms, limit)?;
    let points = page
        .samples
        .into_iter()
        .filter_map(|s| serde_json::from_str::<SystemHistoryPoint>(&s.payload).ok())
        .collect();
    Ok(SystemHistoryOutput {
        points,
        next_cursor: page.next_cursor,
    })
}
