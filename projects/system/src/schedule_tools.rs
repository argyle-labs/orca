//! Schedule tools — operator view + control over the in-process scheduler.
//!
//! Schedule rows live in `config_rows` (noun = `schedule`); the scheduler
//! loop reads them every minute (see `server::scheduler`). These verbs
//! give operators visibility and an "invoke now" escape hatch.
//!
//! No `install` verb: schedules are in-process. Setting a `schedule` row
//! via `orca config upsert schedule …` is the install step.
//!
//! Lives in `db` (the crate that owns the rows) — proof-of-shape for
//! content crates carrying their own tools. The body calls
//! `crate::config_store` / `crate::scheduler_runs` directly without going
//! through any service trait.

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct ScheduleListArgs {
    /// Filter by host_owner.
    #[arg(long)]
    pub host: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScheduleListOutput {
    pub schedules: Vec<ScheduleEntry>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct JobStatus {
    pub job_name: String,
    pub last_run_started: Option<String>,
    pub last_run_finished: Option<String>,
    pub last_run_ok: Option<bool>,
    pub last_run_error: Option<String>,
    pub last_run_duration_ms: Option<i64>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct ScheduleStatusArgs {
    /// If provided, return only this job's status.
    pub job: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScheduleStatusOutput {
    pub jobs: Vec<JobStatus>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleRunArgs {
    /// Schedule row name (the `name` in config_rows). Invokes the row's
    /// `job` immediately, out-of-band from the scheduler loop.
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScheduleRunOutput {
    pub job: String,
    pub ok: bool,
    pub duration_ms: i64,
    pub error: Option<String>,
}

// ── Native support ───────────────────────────────────────────────────────────

// Scheduler args are intentionally opaque: each scheduled tool has its own
// typed Args struct. The schedule row routes a payload to the canonical
// tool, where validation happens. JsonAny is the established passthrough
// for this case.
#[allow(clippy::disallowed_types)]
mod native_support {
    use serde::Deserialize;

    use contract::JsonAny;
    use utils::schedule::Schedule;

    #[derive(Deserialize)]
    pub(super) struct ScheduleRow {
        pub job: String,
        pub cron: String,
        #[serde(default)]
        pub args: Option<JsonAny>,
    }

    /// The next firing time of `cron_expr` as an RFC 3339 string, or `None` if
    /// the expression is invalid or has no future occurrence. Cron parsing
    /// (including 5-field normalization) and the datetime lib are hidden behind
    /// `utils::schedule`.
    pub(super) fn next_run(cron_expr: &str) -> Option<String> {
        Schedule::parse(cron_expr)
            .ok()?
            .next_from_now()
            .map(|t| t.to_rfc3339())
    }
}

// ── Tools ────────────────────────────────────────────────────────────────────

/// List schedule rows with their next firing time.
#[orca_tool(domain = "schedule", verb = "list")]
async fn schedule_list(
    args: ScheduleListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ScheduleListOutput> {
    let conn = db::open_default()?;
    let rows = db::config_store::list(&conn, Some("schedule"), args.host.as_deref())?;
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
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ScheduleStatusOutput> {
    let conn = db::open_default()?;
    let runs = match args.job {
        Some(job) => db::scheduler_runs::last(&conn, &job)?
            .into_iter()
            .collect::<Vec<_>>(),
        None => db::scheduler_runs::last_per_job(&conn)?,
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
    ctx: &contract::ToolCtx,
) -> anyhow::Result<ScheduleRunOutput> {
    let conn = db::open_default()?;
    let row = db::config_store::get(&conn, "schedule", &args.name)?
        .ok_or_else(|| anyhow::anyhow!("no schedule named '{}'", args.name))?;
    drop(conn);

    let parsed: native_support::ScheduleRow = serde_json::from_str(&row.json)
        .map_err(|e| anyhow::anyhow!("malformed schedule row: {e}"))?;

    // Dispatch the scheduled job through the shared inventory. The free-fn
    // dispatcher walks `inventory::iter::<ToolRegistration>` directly — no
    // service-bag handoff. CLI invocations still work because the inventory
    // slice is populated at link time regardless of daemon state.
    let args_value = parsed
        .args
        .map(|j| j.0)
        .unwrap_or_else(|| serde_json::json!({}));
    let t0 = std::time::Instant::now();
    let outcome = dispatch::dispatch(&parsed.job, args_value, ctx).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_tools_register_from_db_crate() {
        let names = dispatch::names();
        assert!(names.contains(&"schedule.list"), "got: {names:?}");
        assert!(names.contains(&"schedule.status"), "got: {names:?}");
        assert!(names.contains(&"schedule.run"), "got: {names:?}");
    }

    #[test]
    fn next_run_parses_five_field_cron() {
        // A valid 5-field expression yields a future RFC3339 firing time.
        let next = native_support::next_run("0 * * * *");
        assert!(next.is_some());
        assert!(next.unwrap().contains('T'));
    }

    #[test]
    fn next_run_parses_six_field_cron() {
        assert!(native_support::next_run("0 0 * * * *").is_some());
    }

    #[test]
    fn next_run_none_for_garbage() {
        assert!(native_support::next_run("not a cron").is_none());
        assert!(native_support::next_run("").is_none());
    }

    #[test]
    fn schedule_row_deserializes_with_args() {
        let row: native_support::ScheduleRow = serde_json::from_str(
            r#"{"job":"host.backup.run","cron":"0 3 * * *","args":{"target":"nas"}}"#,
        )
        .unwrap();
        assert_eq!(row.job, "host.backup.run");
        assert_eq!(row.cron, "0 3 * * *");
        assert!(row.args.is_some());
    }

    #[test]
    fn schedule_row_deserializes_without_args() {
        let row: native_support::ScheduleRow =
            serde_json::from_str(r#"{"job":"j","cron":"@hourly"}"#).unwrap();
        assert_eq!(row.job, "j");
        assert!(row.args.is_none());
    }

    #[test]
    fn schedule_row_rejects_missing_job() {
        let res: Result<native_support::ScheduleRow, _> =
            serde_json::from_str(r#"{"cron":"@hourly"}"#);
        assert!(res.is_err());
    }

    #[test]
    fn schedule_entry_serializes_all_fields() {
        let e = ScheduleEntry {
            name: "nightly".into(),
            job: "host.backup.run".into(),
            cron: "0 3 * * *".into(),
            next_run: Some("2026-07-10T03:00:00+00:00".into()),
            host_owner: "owner-a".into(),
            is_replica: true,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["name"], "nightly");
        assert_eq!(v["job"], "host.backup.run");
        assert_eq!(v["cron"], "0 3 * * *");
        assert_eq!(v["next_run"], "2026-07-10T03:00:00+00:00");
        assert_eq!(v["host_owner"], "owner-a");
        assert_eq!(v["is_replica"], true);
    }

    #[test]
    fn job_status_serializes_options() {
        let js = JobStatus {
            job_name: "j".into(),
            last_run_started: Some("t0".into()),
            last_run_finished: Some("t1".into()),
            last_run_ok: Some(true),
            last_run_error: None,
            last_run_duration_ms: Some(42),
        };
        let v = serde_json::to_value(&js).unwrap();
        assert_eq!(v["job_name"], "j");
        assert_eq!(v["last_run_ok"], true);
        assert_eq!(v["last_run_error"], serde_json::Value::Null);
        assert_eq!(v["last_run_duration_ms"], 42);
    }

    #[test]
    fn run_output_reports_failure() {
        let out = ScheduleRunOutput {
            job: "j".into(),
            ok: false,
            duration_ms: 5,
            error: Some("boom".into()),
        };
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "boom");
        assert_eq!(v["duration_ms"], 5);
    }

    #[test]
    fn list_args_default_has_no_host_filter() {
        let args = ScheduleListArgs::default();
        assert!(args.host.is_none());
    }

    #[test]
    fn status_args_default_has_no_job() {
        let args = ScheduleStatusArgs::default();
        assert!(args.job.is_none());
    }
}
