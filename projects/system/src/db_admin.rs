//! orca.db admin domain — schema status, migrate, up, down, stats,
//! retention sweep, compact (full VACUUM).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

fn run_migrate(
    direction: db::MigrateDirection,
    steps: usize,
    label: &str,
) -> anyhow::Result<DbMigrateReport> {
    let conn = db::open_default()?;
    let before_applied = db::applied_count(&conn)?;
    let before = db::schema_version(&conn)?;
    let after = db::migrate(&conn, direction, steps)?;
    let after_applied = db::applied_count(&conn)?;
    let applied = after_applied.abs_diff(before_applied);
    Ok(DbMigrateReport {
        before,
        after,
        applied,
        direction: label.into(),
    })
}

// ── Shared outputs ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DbStatusReport {
    /// Highest applied migration version (YYYYMMDDHHMMSS timestamp, or 0 if
    /// only the apply_schema baseline has run).
    pub current: i64,
    /// Total migrations compiled into this orca binary.
    pub total: u32,
    /// Pending migration count (total - applied).
    pub pending: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DbMigrateReport {
    pub before: i64,
    pub after: i64,
    /// Number of migrations applied (or rolled back) in this call.
    pub applied: u32,
    pub direction: String,
}

// ── Args (all empty — db lives in a fixed path) ────────────────────────────

macro_rules! empty_args {
    ($name:ident) => {
        #[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
        pub struct $name {}
    };
}
empty_args!(DbStatusArgs);

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct DbUpdateArgs {
    /// "migrate" | "up" | "down"
    pub action: String,
}

/// Show current schema version and pending-migration count.
#[orca_tool(domain = "db", verb = "detail")]
async fn db_detail(
    _args: DbStatusArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DbStatusReport> {
    let conn = db::open_default()?;
    let current = db::schema_version(&conn)?;
    let total = db::migration_count() as u32;
    let applied = db::applied_count(&conn)?;
    Ok(DbStatusReport {
        current,
        total,
        pending: total.saturating_sub(applied),
    })
}

/// [MUTATES STATE] Drive the migration runner. `action`:
/// - `migrate`: apply all pending migrations.
/// - `up`: apply the next pending migration (one step).
/// - `down`: revert the most recently applied migration (one step).
#[orca_tool(domain = "db", verb = "update")]
async fn db_update(
    args: DbUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DbMigrateReport> {
    match args.action.as_str() {
        "migrate" => run_migrate(db::MigrateDirection::Up, usize::MAX, "up-all"),
        "up" => run_migrate(db::MigrateDirection::Up, 1, "up"),
        "down" => run_migrate(db::MigrateDirection::Down, 1, "down"),
        other => anyhow::bail!("unknown action '{other}' (expected migrate|up|down)"),
    }
}

// ── stats ────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DbStatsArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TableStatRow {
    pub name: String,
    pub bytes: i64,
    pub rows: i64,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DbStatsReport {
    /// Total bytes across all user tables (sum of `tables[].bytes`).
    pub total_bytes: i64,
    /// Per-table storage cost, sorted largest-first.
    pub tables: Vec<TableStatRow>,
}

/// Per-table storage cost (bytes + row count). Backed by the SQLite
/// `dbstat` virtual table — compiled in via `SQLITE_ENABLE_DBSTAT_VTAB`.
/// Use to find which table is responsible for db file growth.
#[orca_tool(domain = "db", verb = "stats")]
async fn db_stats(_args: DbStatsArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<DbStatsReport> {
    let conn = db::open_default()?;
    let rows = db::maintenance::table_stats(&conn)?;
    let total_bytes = rows.iter().map(|r| r.bytes).sum();
    let tables = rows
        .into_iter()
        .map(|r| TableStatRow {
            name: r.name,
            bytes: r.bytes,
            rows: r.rows,
        })
        .collect();
    Ok(DbStatsReport {
        total_bytes,
        tables,
    })
}

// ── sweep ────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DbSweepArgs {
    /// Table to sweep. Currently only `session_events` is supported —
    /// extend as more retention policies land.
    #[arg(long)]
    pub table: String,
    /// Delete rows older than this many days. Default 14.
    #[arg(long, default_value_t = 14)]
    pub days: u32,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DbSweepReport {
    pub table: String,
    pub days: u32,
    pub rows_removed: u64,
}

/// [MUTATES STATE] Delete rows older than `days` from `table`. FTS5
/// mirrors cascade via existing triggers. Run `db.compact` afterwards
/// (or wait for incremental_vacuum) to actually reclaim disk space.
#[orca_tool(domain = "db", verb = "sweep")]
async fn db_sweep(args: DbSweepArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<DbSweepReport> {
    let conn = db::open_default()?;
    let rows_removed = match args.table.as_str() {
        "session_events" => db::maintenance::sweep_session_events(&conn, args.days)?,
        other => anyhow::bail!("unknown sweep table '{other}' (supported: session_events)"),
    };
    Ok(DbSweepReport {
        table: args.table,
        days: args.days,
        rows_removed,
    })
}

// ── compact ──────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DbCompactArgs {
    /// If true, run an `incremental_vacuum(pages)` instead of a full
    /// VACUUM — cheap, no lock, but only effective after a one-shot
    /// full VACUUM has activated `auto_vacuum=INCREMENTAL`.
    #[arg(long, default_value_t = false)]
    pub incremental: bool,
    /// Pages to reclaim when `incremental=true`. Ignored for full VACUUM.
    #[arg(long, default_value_t = 4096)]
    pub pages: u32,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DbCompactReport {
    pub mode: String,
    pub bytes_before: i64,
    pub bytes_after: i64,
}

/// [MUTATES STATE] Reclaim disk space. Default = full `VACUUM`
/// (acquires write lock for the duration; needs ~2× db size in temp
/// disk). Pass `incremental=true` for a cheap incremental pass.
///
/// First-time use note: a full VACUUM is also required ONCE on an
/// existing database to activate `auto_vacuum=INCREMENTAL`. After that
/// the incremental path can keep the file compact without locking.
#[orca_tool(domain = "db", verb = "compact")]
async fn db_compact(
    args: DbCompactArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DbCompactReport> {
    let conn = db::open_default()?;
    let bytes_before: i64 = conn.query_row(
        "SELECT page_count * page_size FROM pragma_page_count, pragma_page_size",
        [],
        |r| r.get(0),
    )?;
    let mode = if args.incremental {
        db::maintenance::incremental_vacuum(&conn, args.pages)?;
        "incremental".to_string()
    } else {
        db::maintenance::vacuum(&conn)?;
        "full".to_string()
    };
    let bytes_after: i64 = conn.query_row(
        "SELECT page_count * page_size FROM pragma_page_count, pragma_page_size",
        [],
        |r| r.get(0),
    )?;
    Ok(DbCompactReport {
        mode,
        bytes_before,
        bytes_after,
    })
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
            db_path: PathBuf::from("/tmp/orca-db-admin-test.db"),
            ports: Default::default(),
        }))
    }

    fn migrate_args(action: &str) -> DbUpdateArgs {
        DbUpdateArgs {
            action: action.into(),
        }
    }

    #[tokio::test]
    async fn db_update_rejects_unknown_action() {
        let ctx = empty_ctx();
        assert!(db_update(migrate_args("bogus"), &ctx).await.is_err());
    }
}
