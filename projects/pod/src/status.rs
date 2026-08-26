//! `pod.history` — snapshot history for one peer.
//!
//! Latest-snapshot is already returned by `pod.list` (each member row carries
//! an optional `system` field enriched from the local `host_status` table),
//! so no separate `pod.status.list` is needed. What remains is the timeseries
//! query used by the UI charts.
//!
//! Storage holds only this host's own rows (telemetry is local-only, fetched
//! on demand). A request for a remote peer is dispatched to that peer, so the
//! rows read here are always this host's own — the wire DTO's `peer_id` /
//! `source` fields are stamped at read time to keep the API stable. The tool
//! is read-only — writers live in the server's background tasks.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use system::system_info_types::SystemInfoReport;

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct HostStatusRowDto {
    pub peer_id: String,
    pub snapshot_at_unix: i64,
    pub received_at_unix: i64,
    /// Always `"local"` — telemetry is local-only. Kept on the wire for
    /// backward compatibility with existing `pod.history` consumers.
    pub source: String,
    /// Decoded snapshot. Absent if the stored payload couldn't be parsed
    /// (typically: a schema mismatch after an upgrade).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemInfoReport>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct HostStatusRows(pub Vec<HostStatusRowDto>);

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct HostStatusDetailArgs {
    /// Peer whose history to read. Use `local` to read this host's own rows.
    pub peer_id: String,
    /// Return only rows with `snapshot_at_unix > since`. Omit to read the
    /// full retained history (capped at `MAX_ROWS` in storage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_unix: Option<i64>,
    /// Maximum rows to return. Defaults to 256 — enough for a day at 1/min
    /// with room to spare; pass a lower value for sparkline-style queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Storage no longer carries `peer_id` / `source` (rows are always this
/// host's own local telemetry), so they're stamped from the request: the
/// requested `peer_id` and the constant `"local"` source.
fn rows_to_dtos(rows: Vec<db::host_status::HostStatusRow>, peer_id: &str) -> Vec<HostStatusRowDto> {
    rows.into_iter()
        .map(|r| {
            let system = serde_json::from_str::<SystemInfoReport>(&r.payload_json).ok();
            HostStatusRowDto {
                peer_id: peer_id.to_string(),
                snapshot_at_unix: r.snapshot_at_unix,
                received_at_unix: r.received_at_unix,
                source: "local".to_string(),
                system,
            }
        })
        .collect()
}

/// Snapshot history for one peer, newest-first. The UI (timeseries) uses this
/// via `pod.detail view=history`. Latest snapshot is already on `pod.list`
/// (each member row enriches its `system` field from the same `host_status`
/// table), so no separate list verb exists.
pub async fn host_status_detail(
    args: HostStatusDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<HostStatusRows> {
    let limit = args.limit.unwrap_or(256) as usize;
    let rows = db::pool::with_pooled_or_open(|conn| {
        db::host_status::rows_since(conn, args.since_unix, limit)
    })?;
    Ok(HostStatusRows(rows_to_dtos(rows, &args.peer_id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::ToolCtx;
    use contract::config::{Config, Model};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn empty_ctx() -> ToolCtx {
        ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/orca-pod-status-test.db"),
            ports: Default::default(),
        }))
    }

    fn now() -> i64 {
        utils::time::now().unix_seconds()
    }

    fn seed(conn: &db::Conn, t: i64) {
        // This host's own rows, one with malformed payload to exercise the
        // `system = None` branch. Use recent timestamps so age-based pruning
        // doesn't evict them. Caller passes `t` so a single `now()` reading is
        // shared between seed and the assertions — otherwise a wall-clock
        // tick between the two calls produces off-by-one snapshot_at_unix.
        db::host_status::insert_status(conn, t - 200, "not json at all", t).unwrap();
        db::host_status::insert_status(conn, t - 100, "not json at all", t).unwrap();
    }

    // host_status_list deleted 2026-06-07: pod.status.list folded into
    // pod.list (which already enriches members from the same DB table).

    #[tokio::test]
    async fn host_status_detail_returns_history_newest_first() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let t = now();
            seed(&db::open_default().unwrap(), t);
            let out = host_status_detail(
                HostStatusDetailArgs {
                    peer_id: "local".into(),
                    since_unix: None,
                    limit: None,
                },
                &ctx,
            )
            .await
            .unwrap();
            assert_eq!(out.0.len(), 2);
            assert_eq!(out.0[0].snapshot_at_unix, t - 100);
            assert_eq!(out.0[1].snapshot_at_unix, t - 200);
            assert!(out.0[0].system.is_none(), "unparseable payload → None");
            // peer_id / source are stamped from the request, not storage.
            assert_eq!(out.0[0].peer_id, "local");
            assert_eq!(out.0[0].source, "local");
        })
        .await;
    }

    #[tokio::test]
    async fn host_status_detail_honors_since_unix_watermark() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let t = now();
            seed(&db::open_default().unwrap(), t);
            // watermark between the two rows; only t-100 survives.
            let out = host_status_detail(
                HostStatusDetailArgs {
                    peer_id: "local".into(),
                    since_unix: Some(t - 150),
                    limit: None,
                },
                &ctx,
            )
            .await
            .unwrap();
            assert_eq!(out.0.len(), 1);
            assert_eq!(out.0[0].snapshot_at_unix, t - 100);
        })
        .await;
    }

    #[tokio::test]
    async fn host_status_detail_honors_limit() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let t = now();
            seed(&db::open_default().unwrap(), t);
            let out = host_status_detail(
                HostStatusDetailArgs {
                    peer_id: "local".into(),
                    since_unix: None,
                    limit: Some(1),
                },
                &ctx,
            )
            .await
            .unwrap();
            assert_eq!(out.0.len(), 1);
            assert_eq!(out.0[0].snapshot_at_unix, t - 100);
        })
        .await;
    }

    #[tokio::test]
    async fn host_status_detail_empty_when_no_rows() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            let out = host_status_detail(
                HostStatusDetailArgs {
                    peer_id: "local".into(),
                    since_unix: None,
                    limit: None,
                },
                &ctx,
            )
            .await
            .unwrap();
            assert_eq!(out.0.len(), 0);
        })
        .await;
    }
}
