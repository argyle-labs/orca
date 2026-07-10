//! In-process cron scheduler. Replaces system cron entirely — single
//! source of truth (the `config_rows` table) and uniform dispatch
//! (calls into the same `dispatch::dispatch` the CLI/MCP/REST use).
//!
//! ## Shape
//!
//! One `periodic::spawn` ticks every 60s. Each tick:
//!   1. Loads `noun = 'schedule'` rows from the config store.
//!   2. For each row, parses its cron expression and asks "since the
//!      last completed run (or daemon start, whichever is later),
//!      should we have fired by now?".
//!   3. If yes, dispatches the row's `job` (canonical tool name) by
//!      calling `dispatch::dispatch`. Records the run history.
//!
//! ## Row shape (config_rows.json)
//!
//! ```json
//! {
//!   "job":  "host.backup.run",
//!   "cron": "0 * * * *",
//!   "args": { ... }            // optional, passed as tool input
//! }
//! ```
//!
//! ## Missed-while-down replay
//!
//! Out of scope for v1. The scheduler does NOT backfill missed runs —
//! after a long downtime, the next due tick fires once and only once.
//! A configurable replay policy is a follow-up.

// Scheduler args are intentionally opaque: each scheduled tool has its own
// typed Args struct. The schedule row routes a payload to the canonical
// tool, where validation happens against that tool's input schema.
#![allow(clippy::disallowed_types)]

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use contract::ToolCtx;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};
use utils::schedule::Schedule;
use utils::time::{self, Timestamp};

use crate::periodic;

/// How often the scheduler wakes to check what's due. Cron expressions
/// in the config store have minute resolution, so 60s is the right
/// cadence.
const TICK_INTERVAL: Duration = Duration::from_secs(60);

/// The schedule row payload, as stored in `config_rows.json`.
#[derive(Debug, Clone, Deserialize)]
struct ScheduleRow {
    /// Canonical tool name to invoke, e.g. `"host.backup.run"`.
    job: String,
    /// Cron expression. 5-field (Unix) or 6-field (with seconds) accepted
    /// per the `cron` crate.
    cron: String,
    /// Optional input to the tool. If omitted, dispatches with `{}`.
    #[serde(default)]
    args: Option<Value>,
}

/// Spawn the scheduler. Returns the periodic-loop handle; daemon should
/// drop it ("leak it") for the lifetime of the process.
pub fn spawn(ctx: Arc<ToolCtx>) -> tokio::task::JoinHandle<()> {
    let daemon_start = time::now();
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "scheduler.run",
            initial_delay: Duration::from_secs(5),
            interval: TICK_INTERVAL,
        },
        periodic::boxed(move || {
            let ctx = ctx.clone();
            async move { tick(&ctx, daemon_start).await }
        }),
    )
}

async fn tick(ctx: &ToolCtx, daemon_start: Timestamp) -> Result<()> {
    let conn = db::open_default().context("open db for scheduler tick")?;
    let rows = db::config_store::list(&conn, Some("schedule"), None)?;
    drop(conn);

    let now = time::now();

    for row in rows {
        // Replicas are read-only mirrors of another host's schedules —
        // the owning host runs them.
        if row.is_replica {
            continue;
        }
        let parsed: ScheduleRow = match serde_json::from_str(&row.json) {
            Ok(v) => v,
            Err(e) => {
                warn!("[scheduler] {}: malformed schedule row: {e}", row.name);
                continue;
            }
        };
        let schedule = match Schedule::parse(&parsed.cron) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    "[scheduler] {}: invalid cron '{}': {e}",
                    row.name, parsed.cron
                );
                continue;
            }
        };

        if !is_due(&schedule, &parsed.job, daemon_start, now)? {
            continue;
        }

        // Dispatch. The periodic primitive records *this* tick's outcome
        // under `scheduler.run`; the dispatched job records its own row
        // under its canonical name so `schedule status` can show both.
        let args = parsed.args.unwrap_or_else(|| serde_json::json!({}));
        let job_name = parsed.job.clone();
        let started_at = time::now();
        let t0 = std::time::Instant::now();
        let outcome = dispatch::dispatch(&job_name, args, ctx).await;
        let finished_at = time::now();
        let ok = outcome.is_ok();
        let error = outcome.as_ref().err().map(|e| format!("{e:#}"));

        if let Err(e) = &outcome {
            warn!("[scheduler] {job_name}: {e:#}");
        } else {
            info!("[scheduler] {job_name}: ok");
        }

        // Best-effort: record the dispatched job's run so `schedule status`
        // shows last-run/outcome under the job's canonical name (not the
        // scheduler's own loop).
        if let Ok(conn) = db::open_default() {
            _ = db::scheduler_runs::record(
                &conn,
                &job_name,
                &started_at.to_rfc3339(),
                &finished_at.to_rfc3339(),
                ok,
                error.as_deref(),
                t0.elapsed().as_millis() as i64,
            );
        }
    }
    Ok(())
}

/// Has `schedule` been due at least once since the later of (a) the job's
/// last completed run and (b) the daemon start?
///
/// This is only the orca-specific wiring: read the job's last run from the db
/// and compute the baseline. The firing decision itself is the reusable
/// [`utils::schedule::Schedule::is_due`]. "Daemon start" is the cutoff so a
/// long downtime doesn't trigger a backfill storm. v1 explicitly does not
/// replay missed runs.
fn is_due(
    schedule: &Schedule,
    job_name: &str,
    daemon_start: Timestamp,
    now: Timestamp,
) -> Result<bool> {
    let conn = db::open_default()?;
    let last = db::scheduler_runs::last(&conn, job_name)?;
    drop(conn);

    let baseline = match last {
        Some(r) => Timestamp::parse_rfc3339(&r.finished_at)
            .unwrap_or(daemon_start)
            .max(daemon_start),
        None => daemon_start,
    };

    Ok(schedule.is_due(baseline, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_parser_accepts_five_field_expression() {
        // Standard Unix cron — 5 fields, no seconds. The normalization lives in
        // `utils::schedule`; here we just confirm the scheduler consumes it.
        let s = Schedule::parse("0 * * * *").unwrap();
        assert!(s.next_from_now().is_some());
    }

    #[test]
    fn is_due_returns_false_for_recently_run_job() {
        // Schedule math (no DB): a job that fires hourly, with baseline = now,
        // should not be due within the same minute.
        let schedule = Schedule::parse("@hourly").unwrap();
        let now = time::now();
        assert!(!schedule.is_due(now, now));
    }
}
