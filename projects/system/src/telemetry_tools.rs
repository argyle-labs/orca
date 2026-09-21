//! `system.telemetry.list` — per-host snapshot timeseries.
//!
//! The recorded `host_status` snapshots for a host, newest-first (the UI's
//! sparkline/timeseries source). This is the reframed home of the former
//! `pod.detail view=history` — a distinct dataset from `system.history` (which
//! is the `db::metrics` series). Named for what it is: periodic host telemetry
//! snapshots, not "history".
//!
//! Storage holds only this host's own rows (telemetry is local-only, fetched on
//! demand). A request for a remote peer is dispatched to that peer via the
//! generic `--peer` path, so the rows read here are always this host's own — the
//! wire DTO's `peer_id` / `source` fields are stamped at read time to keep the
//! API stable. Read-only — writers live in the server's background tasks.
//!
//! Lives in the `system` crate (not `pod`): it reads only `hosts::host_status`,
//! `db::metrics`, and this crate's `SystemInfoReport`, so it carries no pod
//! dependency — part of dissolving `pod.detail`.

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::system_info_types::SystemInfoReport;

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct TelemetrySnapshotRow {
    pub peer_id: String,
    pub snapshot_at_unix: i64,
    pub received_at_unix: i64,
    /// Always `"local"` — telemetry is local-only. Kept on the wire for
    /// backward compatibility with existing consumers.
    pub source: String,
    /// Decoded snapshot. Absent if the stored payload couldn't be parsed
    /// (typically: a schema mismatch after an upgrade).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemInfoReport>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct TelemetrySnapshots(pub Vec<TelemetrySnapshotRow>);

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemTelemetryListArgs {
    /// Peer whose snapshots to read. `local` (default) reads this host's own
    /// rows; target another host with the top-level `--peer` flag, which
    /// dispatches this verb to that host.
    #[arg(long)]
    pub peer_id: Option<String>,
    /// Return only rows with `snapshot_at_unix > since_unix`. Omit for the
    /// full retained history (capped in storage).
    #[arg(long)]
    pub since_unix: Option<i64>,
    /// Maximum rows to return. Defaults to 256 — a day at 1/min with room to
    /// spare; pass lower for sparkline-style queries.
    #[arg(long)]
    pub limit: Option<u32>,
}

/// Storage no longer carries `peer_id` / `source` (rows are always this host's
/// own local telemetry), so they're stamped from the request: the requested
/// `peer_id` and the constant `"local"` source.
fn rows_to_dtos(
    rows: Vec<hosts::host_status::HostStatusRow>,
    peer_id: &str,
) -> Vec<TelemetrySnapshotRow> {
    rows.into_iter()
        .map(|r| {
            let system = serde_json::from_str::<SystemInfoReport>(&r.payload_json).ok();
            TelemetrySnapshotRow {
                peer_id: peer_id.to_string(),
                snapshot_at_unix: r.snapshot_at_unix,
                received_at_unix: r.received_at_unix,
                source: "local".to_string(),
                system,
            }
        })
        .collect()
}

/// Per-host snapshot timeseries, newest-first. Latest snapshot is already on
/// `system.list` (each member row enriches its `system` field from the same
/// `host_status` table), so this verb is the timeseries tail behind it.
#[orca_tool(domain = "system.telemetry", verb = "list")]
async fn system_telemetry_list(
    args: SystemTelemetryListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<TelemetrySnapshots> {
    let peer_id = args.peer_id.unwrap_or_else(|| "local".to_string());
    let limit = args.limit.unwrap_or(256) as usize;
    let rows = db::metrics::with_conn(|conn| {
        hosts::host_status::rows_since(conn, args.since_unix, limit)
    })?;
    Ok(TelemetrySnapshots(rows_to_dtos(rows, &peer_id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        utils::time::now().unix_seconds()
    }

    fn metrics_conn() -> db::Conn {
        let conn = db::Conn::open_in_memory().expect("open_in_memory");
        db::metrics::init_schema(&conn).expect("init metrics schema");
        conn
    }

    /// This host's own rows, one payload malformed to exercise the `system =
    /// None` branch. Recent timestamps so age-based pruning doesn't evict them.
    fn seed(conn: &db::Conn, t: i64) {
        hosts::host_status::insert_status(conn, t - 200, "not json at all", t, 86_400).unwrap();
        hosts::host_status::insert_status(conn, t - 100, "not json at all", t, 86_400).unwrap();
    }

    fn detail(conn: &db::Conn, since_unix: Option<i64>, limit: usize) -> Vec<TelemetrySnapshotRow> {
        let rows = hosts::host_status::rows_since(conn, since_unix, limit).unwrap();
        rows_to_dtos(rows, "local")
    }

    #[test]
    fn telemetry_returns_snapshots_newest_first() {
        let conn = metrics_conn();
        let t = now();
        seed(&conn, t);
        let out = detail(&conn, None, 256);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].snapshot_at_unix, t - 100);
        assert_eq!(out[1].snapshot_at_unix, t - 200);
        assert!(out[0].system.is_none(), "unparseable payload → None");
        // peer_id / source are stamped from the request, not storage.
        assert_eq!(out[0].peer_id, "local");
        assert_eq!(out[0].source, "local");
    }

    #[test]
    fn telemetry_honors_since_unix_watermark() {
        let conn = metrics_conn();
        let t = now();
        seed(&conn, t);
        let out = detail(&conn, Some(t - 150), 256);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].snapshot_at_unix, t - 100);
    }

    #[test]
    fn telemetry_honors_limit() {
        let conn = metrics_conn();
        let t = now();
        seed(&conn, t);
        let out = detail(&conn, None, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].snapshot_at_unix, t - 100);
    }

    #[test]
    fn telemetry_empty_when_no_rows() {
        let conn = metrics_conn();
        let out = detail(&conn, None, 256);
        assert_eq!(out.len(), 0);
    }
}
