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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_info_types::SystemHistoryPoint;

    fn ctx() -> contract::ToolCtx {
        use contract::config::{Config, Model};
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("orca-hist-ctx-{}", std::process::id()));
        contract::ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: dir.clone(),
            memory_root: dir.clone(),
            db_path: dir.join("hist-test.db"),
            ports: Default::default(),
        }))
    }

    fn point(ts: i64) -> SystemHistoryPoint {
        SystemHistoryPoint {
            ts,
            cpu_percent: Some(ts as f32),
            ..Default::default()
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
    }

    /// One consolidated test: the metrics store is a process-global connection
    /// bound (lazily) to `$ORCA_HOME`, so all handler assertions share one temp
    /// home in a single test to avoid cross-test contamination of that static.
    #[test]
    fn history_reads_seeded_window_with_paging_since_and_bad_payload() {
        let home = tempfile::tempdir().expect("tempdir");
        // SAFETY: nextest runs each test in its own process, so this env write
        // races nothing. Set before the first metrics access binds the static.
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
        }

        // Seed samples at ts = 100,200,300 (seconds); stored at ts*1000 ms.
        for ts in [100_i64, 200, 300] {
            db::metrics::record_json(SERIES, ts * 1000, &point(ts)).expect("seed sample");
        }
        // A non-decodable payload at ts=50 (OLDER than the good points) must be
        // filtered out by the handler. Kept older-than-newest deliberately: the
        // storage limit applies to RAW rows before the handler drops junk, so a
        // junk row at the newest ts would consume a limit=1 page and yield zero
        // decoded points. Placing it oldest exercises junk-filtering without
        // perturbing the newest-first pagination assertions below.
        db::metrics::record(SERIES, 50_000, "not a history point").expect("seed junk");

        // Default window: newest-first, junk dropped, three good points.
        let out = rt()
            .block_on(system_history(SystemHistoryArgs::default(), &ctx()))
            .expect("history");
        let ts_order: Vec<i64> = out.points.iter().map(|p| p.ts).collect();
        assert_eq!(ts_order, vec![300, 200, 100], "newest-first, junk filtered");
        assert!(out.next_cursor.is_none());

        // limit=1 → newest point plus a cursor to page older.
        let page1 = rt()
            .block_on(system_history(
                SystemHistoryArgs {
                    limit: Some(1),
                    ..Default::default()
                },
                &ctx(),
            ))
            .expect("page1");
        assert_eq!(page1.points.len(), 1);
        assert_eq!(page1.points[0].ts, 300);
        let cursor = page1.next_cursor.expect("more pages remain");

        // Paging with the returned cursor yields the next-older point.
        let page2 = rt()
            .block_on(system_history(
                SystemHistoryArgs {
                    limit: Some(1),
                    cursor: Some(cursor),
                    ..Default::default()
                },
                &ctx(),
            ))
            .expect("page2");
        assert_eq!(page2.points[0].ts, 200);

        // `since` (unix seconds) lower-bounds the window: since=250 keeps only ts=300.
        let recent = rt()
            .block_on(system_history(
                SystemHistoryArgs {
                    since: Some(250),
                    ..Default::default()
                },
                &ctx(),
            ))
            .expect("since");
        let recent_ts: Vec<i64> = recent.points.iter().map(|p| p.ts).collect();
        assert_eq!(recent_ts, vec![300]);
    }
}
