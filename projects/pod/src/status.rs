//! `pod.history` — snapshot history for one peer.
//!
//! Latest-snapshot-per-peer is already returned by `pod.list` (each member
//! row carries an optional `system` field enriched from the local
//! `host_status` table), so no separate `pod.status.list` is needed. What
//! remains is the per-peer timeseries query used by the UI charts and the
//! sync puller's watermarked pull.
//!
//! Authority: the receiving host's DB owns its own rows; every other row
//! was mirrored from a peer via the pull-based sync task. The tool is
//! read-only — writers live in the server's background tasks.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;
use system::system_info_types::SystemInfoReport;

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

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
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

fn rows_to_dtos(rows: Vec<db::host_status::HostStatusRow>) -> Vec<HostStatusRowDto> {
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

/// Snapshot history for one peer, newest-first. Both the UI (timeseries)
/// and the sync puller (watermarked pull) use this. Latest-per-peer is
/// already on `pod.list` (each member row enriches its `system` field
/// from the same `host_status` table), so no separate list verb exists.
#[orca_tool(domain = "pod", verb = "history")]
async fn host_status_detail(
    args: HostStatusDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<HostStatusRows> {
    let conn = db::open_default()?;
    let limit = args.limit.unwrap_or(256) as usize;
    let rows = db::host_status::rows_for_peer(&conn, &args.peer_id, args.since_unix, limit)?;
    Ok(HostStatusRows(rows_to_dtos(rows)))
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
        chrono::Utc::now().timestamp()
    }

    fn seed(conn: &db::Conn, t: i64) {
        // Two peers, multiple rows each, one with malformed payload to exercise
        // the `system = None` branch. Use recent timestamps so age-based pruning
        // doesn't evict them. Caller passes `t` so a single `now()` reading is
        // shared between seed and the assertions — otherwise a wall-clock
        // tick between the two calls produces off-by-one snapshot_at_unix.
        db::host_status::insert_status(conn, "alpha", t - 200, "not json at all", t, "local")
            .unwrap();
        db::host_status::insert_status(conn, "alpha", t - 100, "not json at all", t, "local")
            .unwrap();
        db::host_status::insert_status(conn, "beta", t - 150, "not json at all", t, "synced")
            .unwrap();
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
                    peer_id: "alpha".into(),
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
            // watermark between the two alpha rows; only t-100 survives.
            let out = host_status_detail(
                HostStatusDetailArgs {
                    peer_id: "alpha".into(),
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
                    peer_id: "alpha".into(),
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
    async fn host_status_detail_unknown_peer_is_empty() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = empty_ctx();
        db::with_db_path(tmp.path().to_path_buf(), async move {
            seed(&db::open_default().unwrap(), now());
            let out = host_status_detail(
                HostStatusDetailArgs {
                    peer_id: "nope".into(),
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
