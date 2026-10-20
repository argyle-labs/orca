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
//!   "args": { ... },           // optional, passed as tool input
//!   "run_as": { "user_id": "…", "role": "admin", "can_mutate": false }
//! }
//! ```
//!
//! `run_as` is stamped by `config.upsert` from the writing caller. Each job
//! dispatches as that user at the lower of the stamped role and the user's
//! current role; a row with no owner, or whose owner is gone, is refused.
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
use serde::{Deserialize, Serialize};
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
    #[serde(default)]
    run_as: Option<RunAs>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RunAs {
    user_id: String,
    role: String,
    #[serde(default)]
    can_mutate: bool,
}

const RUN_AS_FIELD: &str = "run_as";

pub fn stamp_run_as(json: &str, caller: &contract::CallerIdentity) -> Result<String> {
    let mut row: Value = serde_json::from_str(json).context("schedule row is not valid JSON")?;
    let obj = row
        .as_object_mut()
        .context("schedule row must be a JSON object")?;
    let run_as = RunAs {
        user_id: caller.user_id.clone(),
        role: caller.role.clone(),
        can_mutate: caller.can_mutate,
    };
    obj.insert(RUN_AS_FIELD.into(), serde_json::to_value(run_as)?);
    Ok(serde_json::to_string(&row)?)
}

fn job_caller(run_as: Option<&RunAs>) -> Result<contract::CallerIdentity> {
    let run_as = run_as.context("schedule has no owner; re-save it to run as yourself")?;
    let conn = db::open_default()?;
    let user = identities::users::find_by_id(&conn, &run_as.user_id)?
        .with_context(|| format!("schedule owner '{}' no longer exists", run_as.user_id))?;
    let rank = dispatch::tool_roles::role_rank;
    let role = if rank(&user.role) < rank(&run_as.role) {
        user.role
    } else {
        run_as.role.clone()
    };
    anyhow::ensure!(
        rank(&role) > 0,
        "schedule owner '{}' holds no role",
        user.username
    );
    Ok(contract::CallerIdentity {
        user_id: user.id,
        username: user.username,
        role,
        can_mutate: run_as.can_mutate,
    })
}

async fn dispatch_as_owner(
    job: &str,
    args: Value,
    run_as: Option<&RunAs>,
    ctx: &ToolCtx,
) -> Result<Value> {
    let caller = job_caller(run_as)?;
    let mut job_ctx = ctx.clone();
    job_ctx.set_caller(Some(caller.clone()));
    job_ctx.set_verified_caller(Some(caller));
    dispatch::dispatch(job, args, &job_ctx).await
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
        let outcome = dispatch_as_owner(&job_name, args, parsed.run_as.as_ref(), ctx).await;
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

    fn run_as(user_id: &str, role: &str) -> RunAs {
        RunAs {
            user_id: user_id.into(),
            role: role.into(),
            can_mutate: false,
        }
    }

    #[test]
    fn jobs_run_as_their_owner_never_above_the_owners_current_role() {
        let dir = tempfile::tempdir().unwrap();
        db::with_thread_db_path(&dir.path().join("sched.db"), || {
            let conn = db::open_default().unwrap();
            identities::users::insert(&conn, "u-ann", "ann", "x", "member", "2026-01-01T00:00:00Z")
                .unwrap();
            drop(conn);

            assert!(job_caller(None).is_err());
            assert!(job_caller(Some(&run_as("u-gone", "admin"))).is_err());

            let demoted = job_caller(Some(&run_as("u-ann", "admin"))).unwrap();
            assert_eq!(
                (demoted.user_id.as_str(), demoted.role.as_str()),
                ("u-ann", "member")
            );
            let narrowed = job_caller(Some(&run_as("u-ann", "read"))).unwrap();
            assert_eq!(narrowed.role, "read");
            assert!(job_caller(Some(&run_as("u-ann", "bogus"))).is_err());
        });
    }

    #[test]
    fn stamp_run_as_overwrites_a_supplied_owner() {
        let caller = contract::CallerIdentity {
            user_id: "u-ann".into(),
            username: "ann".into(),
            role: "user".into(),
            can_mutate: false,
        };
        let stamped = stamp_run_as(
            r#"{"job":"x.y","cron":"@hourly","run_as":{"user_id":"root","role":"super-admin"}}"#,
            &caller,
        )
        .unwrap();
        let row: ScheduleRow = serde_json::from_str(&stamped).unwrap();
        assert_eq!(row.run_as, Some(run_as("u-ann", "user")));
        assert!(stamp_run_as("[]", &caller).is_err());
    }
}
