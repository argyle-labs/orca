//! orca.db admin domain — schema status, migrate, up, down.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use orca_macro::orca_tool;

#[cfg(feature = "native")]
fn run_migrate(
    direction: orca_db::MigrateDirection,
    steps: usize,
    label: &str,
) -> anyhow::Result<DbMigrateReport> {
    let conn = orca_db::open_default()?;
    let before_applied = orca_db::applied_count(&conn)?;
    let before = orca_db::schema_version(&conn)?;
    let after = orca_db::migrate(&conn, direction, steps)?;
    let after_applied = orca_db::applied_count(&conn)?;
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
        #[cfg_attr(feature = "cli", derive(clap::Args))]
        #[derive(Serialize, Deserialize, JsonSchema)]
        pub struct $name {}
    };
}
empty_args!(DbStatusArgs);

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DbLifecycleUpdateArgs {
    /// "migrate" | "up" | "down"
    pub action: String,
}

/// Show current schema version and pending-migration count.
#[orca_tool(domain = "system.db", verb = "detail")]
async fn db_detail(
    _args: DbStatusArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DbStatusReport> {
    let conn = orca_db::open_default()?;
    let current = orca_db::schema_version(&conn)?;
    let total = orca_db::migration_count() as u32;
    let applied = orca_db::applied_count(&conn)?;
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
#[orca_tool(domain = "system.db.lifecycle", verb = "update")]
async fn db_lifecycle_update(
    args: DbLifecycleUpdateArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DbMigrateReport> {
    match args.action.as_str() {
        "migrate" => run_migrate(orca_db::MigrateDirection::Up, usize::MAX, "up-all"),
        "up" => run_migrate(orca_db::MigrateDirection::Up, 1, "up"),
        "down" => run_migrate(orca_db::MigrateDirection::Down, 1, "down"),
        other => anyhow::bail!("unknown action '{other}' (expected migrate|up|down)"),
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::test_support::empty_ctx;

    fn migrate_args(action: &str) -> DbLifecycleUpdateArgs {
        DbLifecycleUpdateArgs {
            action: action.into(),
        }
    }

    #[tokio::test]
    async fn db_lifecycle_rejects_unknown_action() {
        let ctx = empty_ctx();
        assert!(
            db_lifecycle_update(migrate_args("bogus"), &ctx)
                .await
                .is_err()
        );
    }
}
