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

// ── Args (db lives in a fixed path — no locator args) ──────────────────────

/// Which facet `db.detail` reports. `summary` (schema version + pending count)
/// is the default; `stats` returns per-table storage cost.
#[derive(
    Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum,
)]
#[serde(rename_all = "camelCase")]
pub enum DbDetailView {
    #[default]
    Summary,
    Stats,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DbDetailArgs {
    /// Which facet to report. Defaults to `summary`.
    #[arg(long, value_enum, default_value = "summary")]
    #[serde(default)]
    pub view: DbDetailView,
}

/// `db.detail` payload — one variant per `view`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum DbDetailOutput {
    Summary(DbStatusReport),
    Stats(DbStatsReport),
}

/// Read-only DB detail. `view=summary` shows the current schema version and
/// pending-migration count; `view=stats` returns per-table storage cost (bytes +
/// row count) from the SQLite `dbstat` virtual table — use it to find which table
/// drives db file growth.
#[orca_tool(domain = "db", verb = "detail")]
async fn db_detail(args: DbDetailArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<DbDetailOutput> {
    let conn = db::open_default()?;
    match args.view {
        DbDetailView::Summary => {
            let current = db::schema_version(&conn)?;
            let total = db::migration_count() as u32;
            let applied = db::applied_count(&conn)?;
            Ok(DbDetailOutput::Summary(DbStatusReport {
                current,
                total,
                pending: total.saturating_sub(applied),
            }))
        }
        DbDetailView::Stats => {
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
            Ok(DbDetailOutput::Stats(DbStatsReport {
                total_bytes,
                tables,
            }))
        }
    }
}

/// The `db.update` action.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum DbUpdateAction {
    /// Apply all pending migrations.
    Migrate,
    /// Apply the next pending migration (one step).
    Up,
    /// Revert the most recently applied migration (one step).
    Down,
    /// Retention sweep: delete rows older than `days` from `table`.
    Sweep,
    /// Reclaim disk space (full `VACUUM`, or `incremental`).
    Compact,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DbUpdateArgs {
    /// Which maintenance action to run.
    #[arg(long, value_enum)]
    pub action: DbUpdateAction,
    /// `sweep`: table to sweep (currently only `session_events`).
    #[arg(long)]
    #[serde(default)]
    pub table: Option<String>,
    /// `sweep`: delete rows older than this many days. Default 14.
    #[arg(long, default_value_t = 14)]
    #[serde(default = "default_sweep_days")]
    pub days: u32,
    /// `compact`: run an `incremental_vacuum(pages)` instead of a full VACUUM.
    #[arg(long, default_value_t = false)]
    #[serde(default)]
    pub incremental: bool,
    /// `compact`: pages to reclaim when `incremental=true`. Ignored for full VACUUM.
    #[arg(long, default_value_t = 4096)]
    #[serde(default = "default_compact_pages")]
    pub pages: u32,
}

fn default_sweep_days() -> u32 {
    14
}

fn default_compact_pages() -> u32 {
    4096
}

/// `db.update` payload — one variant per `action`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum DbUpdateOutput {
    Migrate(DbMigrateReport),
    Sweep(DbSweepReport),
    Compact(DbCompactReport),
}

/// [MUTATES STATE] Drive DB maintenance. `action`:
/// - `migrate`: apply all pending migrations.
/// - `up`: apply the next pending migration (one step).
/// - `down`: revert the most recently applied migration (one step).
/// - `sweep`: delete rows older than `days` from `table` (FTS5 mirrors cascade;
///   run `compact` afterwards to reclaim disk).
/// - `compact`: reclaim disk space — full `VACUUM` (default) or `incremental`.
#[orca_tool(domain = "db", verb = "update")]
async fn db_update(args: DbUpdateArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<DbUpdateOutput> {
    match args.action {
        DbUpdateAction::Migrate => Ok(DbUpdateOutput::Migrate(run_migrate(
            db::MigrateDirection::Up,
            usize::MAX,
            "up-all",
        )?)),
        DbUpdateAction::Up => Ok(DbUpdateOutput::Migrate(run_migrate(
            db::MigrateDirection::Up,
            1,
            "up",
        )?)),
        DbUpdateAction::Down => Ok(DbUpdateOutput::Migrate(run_migrate(
            db::MigrateDirection::Down,
            1,
            "down",
        )?)),
        DbUpdateAction::Sweep => Ok(DbUpdateOutput::Sweep(db_sweep(args)?)),
        DbUpdateAction::Compact => Ok(DbUpdateOutput::Compact(db_compact(args)?)),
    }
}

fn db_sweep(args: DbUpdateArgs) -> anyhow::Result<DbSweepReport> {
    let table = args
        .table
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("`table` is required for action=sweep"))?;
    let conn = db::open_default()?;
    let rows_removed = match table {
        "session_events" => db::maintenance::sweep_session_events(&conn, args.days)?,
        other => anyhow::bail!("unknown sweep table '{other}' (supported: session_events)"),
    };
    Ok(DbSweepReport {
        table: table.to_string(),
        days: args.days,
        rows_removed,
    })
}

fn db_compact(args: DbUpdateArgs) -> anyhow::Result<DbCompactReport> {
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

// ── stats / sweep / compact payloads (folded into detail{view}/update{action}) ─

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

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DbSweepReport {
    pub table: String,
    pub days: u32,
    pub rows_removed: u64,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DbCompactReport {
    pub mode: String,
    pub bytes_before: i64,
    pub bytes_after: i64,
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

    fn update_args(action: DbUpdateAction) -> DbUpdateArgs {
        DbUpdateArgs {
            action,
            table: None,
            days: default_sweep_days(),
            incremental: false,
            pages: default_compact_pages(),
        }
    }

    #[tokio::test]
    async fn db_sweep_requires_table() {
        let ctx = empty_ctx();
        assert!(
            db_update(update_args(DbUpdateAction::Sweep), &ctx)
                .await
                .is_err()
        );
    }
}
