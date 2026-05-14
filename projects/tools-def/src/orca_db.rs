//! orca.db admin domain — schema status, migrate, up, down.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

// ── Shared outputs ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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
        #[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
        #[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
        #[cfg_attr(feature = "cli", derive(clap::Args))]
        #[derive(Serialize, Deserialize, JsonSchema)]
        pub struct $name {}
    };
}
empty_args!(DbStatusArgs);
empty_args!(DbMigrateArgs);
empty_args!(DbUpArgs);
empty_args!(DbDownArgs);

#[cfg(feature = "native")]
fn db_svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::db_admin::DbAdminService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::db_admin::DbAdminService>>()
}

/// Show current schema version and pending-migration count.
#[orca_tool(domain = "db", verb = "status")]
async fn db_status(
    _args: DbStatusArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DbStatusReport> {
    db_svc(ctx)?.status().await
}

/// [MUTATES STATE] Apply all pending migrations.
#[orca_tool(domain = "db", verb = "migrate")]
async fn db_migrate(
    _args: DbMigrateArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DbMigrateReport> {
    db_svc(ctx)?.migrate().await
}

/// [MUTATES STATE] Apply the next pending migration (one step).
#[orca_tool(domain = "db", verb = "up")]
async fn db_up(
    _args: DbUpArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DbMigrateReport> {
    db_svc(ctx)?.up().await
}

/// [MUTATES STATE] Revert the most recently applied migration (one step).
#[orca_tool(domain = "db", verb = "down")]
async fn db_down(
    _args: DbDownArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DbMigrateReport> {
    db_svc(ctx)?.down().await
}
