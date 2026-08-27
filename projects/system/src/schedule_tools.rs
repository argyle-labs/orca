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
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScheduleListOutput {
    pub schedules: Vec<ScheduleEntry>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total schedules across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
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

/// Which facet `schedule.detail` reports. `status` (per-job last-run history) is
/// the only view today; more fold in here as the enum grows.
#[derive(
    Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum,
)]
#[serde(rename_all = "camelCase")]
pub enum ScheduleDetailView {
    #[default]
    Status,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct ScheduleStatusArgs {
    /// Which facet to report. Defaults to `status`.
    #[arg(long, value_enum, default_value = "status")]
    #[serde(default)]
    pub view: ScheduleDetailView,
    /// If provided, return only this job's status.
    pub job: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScheduleStatusOutput {
    pub jobs: Vec<JobStatus>,
}

/// The `schedule.create` action. Only `run` today; the discriminant keeps the
/// surface six-verb-uniform.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleCreateAction {
    /// Invoke the row's job immediately, out-of-band from the loop.
    #[default]
    Run,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleRunArgs {
    /// Which create action to run. Defaults to `run`.
    #[arg(long, value_enum, default_value = "run")]
    #[serde(default)]
    pub action: ScheduleCreateAction,
    /// Schedule row name (the `name` in config_rows). Invokes the row's
    /// `job` immediately, out-of-band from the scheduler loop.
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
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
    let schedules: Vec<ScheduleEntry> = rows
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
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(schedules, &params);
    Ok(ScheduleListOutput {
        schedules: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}

/// Show per-job last-run status from the scheduler_runs history.
/// `view=status` (the default and only view) reports the run history.
#[orca_tool(domain = "schedule", verb = "detail")]
async fn schedule_status(
    args: ScheduleStatusArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ScheduleStatusOutput> {
    let ScheduleDetailView::Status = args.view;
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
/// `action=run` (the default and only action) runs the row's job now.
#[orca_tool(domain = "schedule", verb = "create")]
async fn schedule_run(
    args: ScheduleRunArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<ScheduleRunOutput> {
    let ScheduleCreateAction::Run = args.action;
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
        assert!(names.contains(&"schedule.detail"), "got: {names:?}");
        assert!(names.contains(&"schedule.create"), "got: {names:?}");
    }

    #[test]
    fn next_run_valid_cron_is_some_invalid_is_none() {
        // A well-formed cron has a future occurrence → Some RFC3339 string.
        let s = native_support::next_run("0 * * * *").expect("valid cron yields a next run");
        assert!(s.contains('T'), "expected an RFC3339 timestamp, got: {s}");
        // Garbage never parses → None.
        assert!(native_support::next_run("not a cron").is_none());
    }

    fn ctx() -> contract::ToolCtx {
        use contract::config::{Config, Model};
        use std::sync::Arc;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("orca-sched-ctx-{}-{}", std::process::id(), n));
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
            db_path: dir.join("sched-test.db"),
            ports: Default::default(),
        }))
    }

    /// Bind a temp DB on this thread (seeding a deterministic `host.display_name`)
    /// and drive an async closure to completion on a current-thread runtime while
    /// the thread-local DB path stays bound. `seed` runs synchronously with a live
    /// connection before the async body.
    fn with_db_block<Fut, T>(seed: impl FnOnce(&db::Conn), f: impl FnOnce() -> Fut) -> T
    where
        Fut: std::future::Future<Output = T>,
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schedule-tools.db");
        db::with_thread_db_path(&path, || {
            let conn = db::open_default().expect("open temp db");
            db::settings::set(&conn, "host.display_name", "testhost").expect("seed host name");
            seed(&conn);
            drop(conn);
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            rt.block_on(f())
        })
    }

    fn seed_schedule(conn: &db::Conn, name: &str, json: &str) {
        db::config_store::set(conn, "testhost", "testhost", "schedule", name, json, "test")
            .expect("seed schedule row");
    }

    #[test]
    fn list_parses_valid_rows_and_skips_malformed() {
        let out = with_db_block(
            |conn| {
                seed_schedule(
                    conn,
                    "nightly",
                    r#"{"job":"host.backup.run","cron":"0 3 * * *"}"#,
                );
                // Malformed (missing required `job`/`cron`) → filtered out.
                seed_schedule(conn, "broken", r#"{"foo":"bar"}"#);
            },
            || async { schedule_list(ScheduleListArgs::default(), &ctx()).await },
        )
        .expect("list");

        assert_eq!(out.schedules.len(), 1, "malformed row must be skipped");
        let e = &out.schedules[0];
        assert_eq!(e.name, "nightly");
        assert_eq!(e.job, "host.backup.run");
        assert_eq!(e.cron, "0 3 * * *");
        assert_eq!(e.host_owner, "testhost");
        assert!(!e.is_replica);
        assert!(
            e.next_run.is_some(),
            "a valid cron yields a next firing time"
        );
    }

    #[test]
    fn status_reports_seeded_runs_and_filters_by_job() {
        let out = with_db_block(
            |conn| {
                db::scheduler_runs::record(
                    conn,
                    "host.backup.run",
                    "2026-01-01T03:00:00Z",
                    "2026-01-01T03:00:05Z",
                    true,
                    None,
                    5000,
                )
                .expect("record ok run");
                db::scheduler_runs::record(
                    conn,
                    "other.job",
                    "2026-01-01T04:00:00Z",
                    "2026-01-01T04:00:01Z",
                    false,
                    Some("boom"),
                    1000,
                )
                .expect("record failed run");
            },
            || async {
                // Filtered to a single job.
                schedule_status(
                    ScheduleStatusArgs {
                        view: ScheduleDetailView::Status,
                        job: Some("other.job".into()),
                    },
                    &ctx(),
                )
                .await
            },
        )
        .expect("status");

        assert_eq!(out.jobs.len(), 1);
        let j = &out.jobs[0];
        assert_eq!(j.job_name, "other.job");
        assert_eq!(j.last_run_ok, Some(false));
        assert_eq!(j.last_run_error.as_deref(), Some("boom"));
        assert_eq!(j.last_run_duration_ms, Some(1000));
    }

    #[test]
    fn status_all_jobs_returns_last_per_job() {
        let out = with_db_block(
            |conn| {
                db::scheduler_runs::record(conn, "a.job", "s", "f", true, None, 1).unwrap();
                db::scheduler_runs::record(conn, "b.job", "s", "f", true, None, 2).unwrap();
            },
            || async { schedule_status(ScheduleStatusArgs::default(), &ctx()).await },
        )
        .expect("status");
        let mut names: Vec<_> = out.jobs.iter().map(|j| j.job_name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["a.job".to_string(), "b.job".to_string()]);
    }

    #[test]
    fn run_missing_schedule_errors() {
        let err = with_db_block(
            |_conn| {},
            || async {
                schedule_run(
                    ScheduleRunArgs {
                        action: ScheduleCreateAction::Run,
                        name: "ghost".into(),
                    },
                    &ctx(),
                )
                .await
            },
        )
        .expect_err("running an unknown schedule must error");
        assert!(
            err.to_string().contains("no schedule named 'ghost'"),
            "got: {err}"
        );
    }

    #[test]
    fn run_malformed_row_errors() {
        let err = with_db_block(
            |conn| seed_schedule(conn, "bad", r#"{"nope":true}"#),
            || async {
                schedule_run(
                    ScheduleRunArgs {
                        action: ScheduleCreateAction::Run,
                        name: "bad".into(),
                    },
                    &ctx(),
                )
                .await
            },
        )
        .expect_err("malformed schedule row must error");
        assert!(
            err.to_string().contains("malformed schedule row"),
            "got: {err}"
        );
    }

    #[test]
    fn run_dispatches_the_rows_job() {
        // A schedule whose job is a real registered tool (`schedule.list`,
        // which takes no required args) dispatches successfully out-of-band.
        let out = with_db_block(
            |conn| {
                seed_schedule(
                    conn,
                    "selflist",
                    r#"{"job":"schedule.list","cron":"0 * * * *"}"#,
                )
            },
            || async {
                schedule_run(
                    ScheduleRunArgs {
                        action: ScheduleCreateAction::Run,
                        name: "selflist".into(),
                    },
                    &ctx(),
                )
                .await
            },
        )
        .expect("dispatch ok");
        assert_eq!(out.job, "schedule.list");
        assert!(
            out.ok,
            "dispatching schedule.list should succeed: {:?}",
            out.error
        );
        assert!(out.error.is_none());
    }
}
