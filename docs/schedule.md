# Scheduler

Cron-style job scheduling built on config rows and the tool registry. Any `#[orca_tool]` can be a scheduled job.

---

## How it works

A schedule is a `config_rows` row with `noun = "schedule"`. The payload:

```json
{
  "job": "host.backup.run",
  "cron": "0 2 * * *",
  "args": { }
}
```

- `job` — a canonical tool name (`domain.verb`). The scheduler dispatches it through the same inventory registry as CLI/MCP/HTTP (`dispatch::dispatch`); there is no separate job registry and no built-in jobs.
- `cron` — 5-field Unix cron (minute resolution) or 6-field (seconds first). 5-field is auto-normalized by prepending `0 `. Parsed by the `cron` crate (0.16). **All times are UTC.**
- `args` — optional, passed as the tool's typed input.

The daemon ticks every 60 seconds (`projects/system/src/scheduler.rs`): loads all schedule rows, skips replicas, fires anything due since its last completed run (or daemon start), and records the result.

**Semantics:** no jitter; no catch-up replay — if the daemon was down across N due firings, the next tick fires the job once and normal cadence resumes.

## Managing schedules

Create/edit via `config.set` (the schedule noun has no dedicated create tool):

```bash
orca config set schedule backup-daily '{"job":"host.backup.run","cron":"0 2 * * *"}'
orca config set schedule plex-scan '{"job":"plex.library.scan","cron":"0 * * * *","args":{"recursive":true}}'
orca config delete schedule backup-daily
```

Validation is lazy: the JSON must parse at write time, but the cron expression and tool name are only checked when the scheduler ticks — an invalid one is logged as a warning and skipped, not rejected at write.

## Inspecting

| Tool | What it does |
|---|---|
| `schedule.list [--host <h>]` | All schedule rows with computed `next_run` (null if the cron is invalid), owner, and replica flag |
| `schedule.status [--job <name>]` | Last-run history per job: started/finished, ok, error, duration |
| `schedule.run <row-name>` | Fire a schedule's job immediately, out-of-band from the loop — for testing wiring without waiting |

## Run history

Every execution (scheduled or `schedule.run`) is recorded in the `scheduler_runs` table: `job_name`, `started_at`/`finished_at` (RFC3339 UTC), `ok`, `error`, `duration_ms`. Retention is a rolling window of **1440 rows per job** (~24 h at minute cadence), trimmed automatically on insert (`projects/db/src/scheduler_runs.rs`).

## Pods: who runs a schedule

Every config row has a `host_owner`. **Only the owning host executes its schedules** — replicated copies (`is_replica = 1`) on peers are read-only cache and are always skipped by the tick loop. A host cannot write a row owned by another host; that write is refused today and will route via the pod mesh once peer dispatch lands (ROADMAP §3.3).
