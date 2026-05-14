//! Scheduler run history. Backs the `periodic::spawn` observability layer
//! and the future `orca schedule status` view.
//!
//! Retention is bounded per-job: `record` trims rows beyond `RETAIN_PER_JOB`
//! so a chatty tick (e.g. 60s) doesn't grow unbounded.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// Per-job retention cap. 1440 ≈ 24h of minute-cadence runs.
pub const RETAIN_PER_JOB: i64 = 1440;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulerRun {
    pub id: i64,
    pub job_name: String,
    pub started_at: String,
    pub finished_at: String,
    pub ok: bool,
    pub error: Option<String>,
    pub duration_ms: i64,
}

/// Record a completed run and trim history for this job.
pub fn record(
    conn: &Connection,
    job_name: &str,
    started_at: &str,
    finished_at: &str,
    ok: bool,
    error: Option<&str>,
    duration_ms: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO scheduler_runs (job_name, started_at, finished_at, ok, error, duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![job_name, started_at, finished_at, ok as i64, error, duration_ms],
    )?;
    // Trim — keep the newest RETAIN_PER_JOB rows per job.
    conn.execute(
        "DELETE FROM scheduler_runs
         WHERE job_name = ?1
           AND id NOT IN (
               SELECT id FROM scheduler_runs
               WHERE job_name = ?1
               ORDER BY id DESC
               LIMIT ?2
           )",
        params![job_name, RETAIN_PER_JOB],
    )?;
    Ok(())
}

/// Most recent run for a job, if any.
pub fn last(conn: &Connection, job_name: &str) -> Result<Option<SchedulerRun>> {
    let r = conn
        .query_row(
            "SELECT id, job_name, started_at, finished_at, ok, error, duration_ms
             FROM scheduler_runs WHERE job_name = ?1
             ORDER BY id DESC LIMIT 1",
            params![job_name],
            row_from,
        )
        .optional()?;
    Ok(r)
}

/// Recent runs for a job, newest first.
pub fn recent(conn: &Connection, job_name: &str, limit: i64) -> Result<Vec<SchedulerRun>> {
    let mut stmt = conn.prepare(
        "SELECT id, job_name, started_at, finished_at, ok, error, duration_ms
         FROM scheduler_runs WHERE job_name = ?1
         ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![job_name, limit], row_from)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One latest-row-per-job summary across all jobs.
pub fn last_per_job(conn: &Connection) -> Result<Vec<SchedulerRun>> {
    let mut stmt = conn.prepare(
        "SELECT id, job_name, started_at, finished_at, ok, error, duration_ms
         FROM scheduler_runs r
         WHERE id = (
             SELECT MAX(id) FROM scheduler_runs WHERE job_name = r.job_name
         )
         ORDER BY job_name",
    )?;
    let rows = stmt.query_map([], row_from)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<SchedulerRun> {
    Ok(SchedulerRun {
        id: r.get(0)?,
        job_name: r.get(1)?,
        started_at: r.get(2)?,
        finished_at: r.get(3)?,
        ok: r.get::<_, i64>(4)? != 0,
        error: r.get(5)?,
        duration_ms: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn record_and_last() {
        let conn = test_conn();
        record(&conn, "x.run", "2026-05-14T00:00:00Z", "2026-05-14T00:00:01Z", true, None, 1000).unwrap();
        let r = last(&conn, "x.run").unwrap().unwrap();
        assert!(r.ok);
        assert_eq!(r.duration_ms, 1000);
    }

    #[test]
    fn last_per_job_returns_one_row_each() {
        let conn = test_conn();
        record(&conn, "a", "t1", "t2", true, None, 1).unwrap();
        record(&conn, "a", "t3", "t4", true, None, 2).unwrap();
        record(&conn, "b", "t5", "t6", false, Some("boom"), 9).unwrap();
        let v = last_per_job(&conn).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].job_name, "a");
        assert_eq!(v[0].duration_ms, 2); // newest a-run
        assert_eq!(v[1].job_name, "b");
        assert!(!v[1].ok);
    }

    #[test]
    fn retention_trims_oldest() {
        let conn = test_conn();
        for i in 0..(RETAIN_PER_JOB + 5) {
            record(&conn, "noisy", &format!("{i}"), &format!("{i}"), true, None, 0).unwrap();
        }
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM scheduler_runs WHERE job_name = 'noisy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, RETAIN_PER_JOB);
    }
}
