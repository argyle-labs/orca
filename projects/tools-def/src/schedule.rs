//! Schedule tools — operator view + control over the in-process scheduler.
//!
//! Schedule rows live in `config_rows` (noun = `schedule`); the scheduler
//! loop reads them every minute (see `server::scheduler`). These verbs
//! give operators visibility and an "invoke now" escape hatch.
//!
//! No `install` verb: schedules are in-process. Setting a `schedule` row
//! via `orca config set schedule …` is the install step.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScheduleEntry {
    /// Row name (the `name` column in config_rows).
    pub name: String,
    /// Canonical tool name to invoke (e.g. `host.backup.run`).
    pub job: String,
    /// Cron expression as stored.
    pub cron: String,
    /// Next firing time (RFC3339, UTC) if the cron parses, else null.
    pub next_run: Option<String>,
    /// host_owner of the schedule row.
    pub host_owner: String,
    /// True if this is a replica from another owner.
    pub is_replica: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct ScheduleListArgs {
    /// Filter by host_owner.
    #[cfg_attr(feature = "cli", arg(long))]
    pub host: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScheduleListOutput {
    pub schedules: Vec<ScheduleEntry>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct JobStatus {
    pub job_name: String,
    pub last_run_started: Option<String>,
    pub last_run_finished: Option<String>,
    pub last_run_ok: Option<bool>,
    pub last_run_error: Option<String>,
    pub last_run_duration_ms: Option<i64>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct ScheduleStatusArgs {
    /// If provided, return only this job's status.
    pub job: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScheduleStatusOutput {
    pub jobs: Vec<JobStatus>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScheduleRunArgs {
    /// Schedule row name (the `name` in config_rows). Invokes the row's
    /// `job` immediately, out-of-band from the scheduler loop.
    pub name: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScheduleRunOutput {
    pub job: String,
    pub ok: bool,
    pub duration_ms: i64,
    pub error: Option<String>,
}

// ── Native support ───────────────────────────────────────────────────────────

#[cfg(feature = "native")]
// Scheduler args are intentionally opaque: each scheduled tool has its own
// typed Args struct. The schedule row routes a payload to the canonical
// tool, where validation happens. JsonAny is the established passthrough
// for this case (see `json_any.rs`).
#[allow(clippy::disallowed_types)]
mod native_support {
    use std::str::FromStr;

    use chrono::{SecondsFormat, Utc};
    use cron::Schedule;
    use serde::Deserialize;

    use crate::JsonAny;

    #[derive(Deserialize)]
    pub(super) struct ScheduleRow {
        pub job: String,
        pub cron: String,
        #[serde(default)]
        pub args: Option<JsonAny>,
    }

    pub(super) fn next_run(cron_expr: &str) -> Option<String> {
        let normalized = normalize_cron(cron_expr);
        let s = Schedule::from_str(&normalized).ok()?;
        s.upcoming(Utc)
            .next()
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
    }

    /// The `cron` crate parses 6- or 7-field expressions (with seconds at the
    /// front). Operators write 5-field Unix cron more naturally — accept both
    /// by prepending `0 ` to 5-field input.
    pub fn normalize_cron(expr: &str) -> String {
        let n = expr.split_whitespace().count();
        if n == 5 {
            format!("0 {expr}")
        } else {
            expr.to_string()
        }
    }
}

// ── Tools ────────────────────────────────────────────────────────────────────

/// List schedule rows with their next firing time.
#[orca_tool(domain = "schedule", verb = "list")]
async fn schedule_list(
    args: ScheduleListArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ScheduleListOutput> {
    let conn = orca_db::open_default()?;
    let rows = orca_db::config_store::list(&conn, Some("schedule"), args.host.as_deref())?;
    let schedules = rows
        .into_iter()
        .filter_map(|row| {
            let parsed: native_support::ScheduleRow = serde_json::from_str(&row.json).ok()?;
            Some(ScheduleEntry {
                name: row.name,
                next_run: native_support::next_run(&parsed.cron),
                job: parsed.job,
                cron: parsed.cron,
                host_owner: row.host_owner,
                is_replica: row.is_replica,
            })
        })
        .collect();
    Ok(ScheduleListOutput { schedules })
}

/// Show per-job last-run status from the scheduler_runs history.
#[orca_tool(domain = "schedule", verb = "status")]
async fn schedule_status(
    args: ScheduleStatusArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ScheduleStatusOutput> {
    let conn = orca_db::open_default()?;
    let runs = match args.job {
        Some(job) => orca_db::scheduler_runs::last(&conn, &job)?
            .into_iter()
            .collect::<Vec<_>>(),
        None => orca_db::scheduler_runs::last_per_job(&conn)?,
    };
    let jobs = runs
        .into_iter()
        .map(|r| JobStatus {
            job_name: r.job_name,
            last_run_started: Some(r.started_at),
            last_run_finished: Some(r.finished_at),
            last_run_ok: Some(r.ok),
            last_run_error: r.error,
            last_run_duration_ms: Some(r.duration_ms),
        })
        .collect();
    Ok(ScheduleStatusOutput { jobs })
}

/// Invoke a scheduled job immediately, out-of-band from the loop.
/// Useful for testing schedule wiring without waiting for the next firing.
#[orca_tool(domain = "schedule", verb = "run")]
async fn schedule_run(
    args: ScheduleRunArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ScheduleRunOutput> {
    let conn = orca_db::open_default()?;
    let row = orca_db::config_store::get(&conn, "schedule", &args.name)?
        .ok_or_else(|| anyhow::anyhow!("no schedule named '{}'", args.name))?;
    drop(conn);

    let parsed: native_support::ScheduleRow = serde_json::from_str(&row.json)
        .map_err(|e| anyhow::anyhow!("malformed schedule row: {e}"))?;

    // Pull the registry off the ToolCtx services. Daemon registers it on
    // startup (see scheduler wiring); CLI invocations have no daemon, so
    // we surface a clear error.
    let registry: std::sync::Arc<orca_utils::tool::ToolRegistry> = ctx
        .service::<std::sync::Arc<orca_utils::tool::ToolRegistry>>()
        .map_err(|_| {
            anyhow::anyhow!(
                "schedule.run requires daemon mode (registry-in-ctx not available from CLI)"
            )
        })?;
    let args_value = parsed
        .args
        .map(|j| j.0)
        .unwrap_or_else(|| serde_json::json!({}));
    let t0 = std::time::Instant::now();
    let outcome = registry.dispatch(&parsed.job, args_value, ctx).await;
    let duration_ms = t0.elapsed().as_millis() as i64;
    let (ok, error) = match &outcome {
        Ok(_) => (true, None),
        Err(e) => (false, Some(format!("{e:#}"))),
    };
    Ok(ScheduleRunOutput {
        job: parsed.job,
        ok,
        duration_ms,
        error,
    })
}
