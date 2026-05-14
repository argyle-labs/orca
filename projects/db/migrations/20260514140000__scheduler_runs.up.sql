-- Scheduler run history — observability for every periodic loop spawned
-- via `server::periodic::spawn`. One row per tick, recording outcome and
-- duration. Bounded retention is enforced by the trim-on-write helper.
-- See docs/planned/orca-v1-scope.md §3.4.

CREATE TABLE IF NOT EXISTS scheduler_runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    job_name    TEXT NOT NULL,
    started_at  TEXT NOT NULL,
    finished_at TEXT NOT NULL,
    ok          INTEGER NOT NULL,
    error       TEXT,
    duration_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_scheduler_runs_job_started
    ON scheduler_runs(job_name, started_at DESC);
