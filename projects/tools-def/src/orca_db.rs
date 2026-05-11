//! orca.db admin domain — schema status, migrate, up, down.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

// ── Shared outputs ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DbStatusReport {
    /// Currently applied schema version.
    pub current: u32,
    /// Total migrations compiled into this orca binary.
    pub total: u32,
    /// Pending migrations: `total - current`.
    pub pending: u32,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DbMigrateReport {
    pub before: u32,
    pub after: u32,
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

// ── Tool defs ───────────────────────────────────────────────────────────────

pub struct DbStatus;
impl OrcaToolDef for DbStatus {
    const NAME: &'static str = "db.status";
    const DESCRIPTION: &'static str = "Show current schema version and pending-migration count.";
    type Args = DbStatusArgs;
    type Output = DbStatusReport;
}

pub struct DbMigrate;
impl OrcaToolDef for DbMigrate {
    const NAME: &'static str = "db.migrate";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Apply all pending migrations.";
    type Args = DbMigrateArgs;
    type Output = DbMigrateReport;
}

pub struct DbUp;
impl OrcaToolDef for DbUp {
    const NAME: &'static str = "db.up";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Apply the next pending migration (one step).";
    type Args = DbUpArgs;
    type Output = DbMigrateReport;
}

pub struct DbDown;
impl OrcaToolDef for DbDown {
    const NAME: &'static str = "db.down";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Revert the most recently applied migration (one step).";
    type Args = DbDownArgs;
    type Output = DbMigrateReport;
}

// ── Native dispatch ─────────────────────────────────────────────────────────

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::db_admin::DbAdminService;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn DbAdminService>> {
        ctx.service::<Arc<dyn DbAdminService>>()
    }

    #[async_trait]
    impl OrcaTool for DbStatus {
        async fn run(_a: DbStatusArgs, ctx: &ToolCtx) -> Result<DbStatusReport> {
            svc(ctx)?.status().await
        }
    }
    #[async_trait]
    impl OrcaTool for DbMigrate {
        async fn run(_a: DbMigrateArgs, ctx: &ToolCtx) -> Result<DbMigrateReport> {
            svc(ctx)?.migrate().await
        }
    }
    #[async_trait]
    impl OrcaTool for DbUp {
        async fn run(_a: DbUpArgs, ctx: &ToolCtx) -> Result<DbMigrateReport> {
            svc(ctx)?.up().await
        }
    }
    #[async_trait]
    impl OrcaTool for DbDown {
        async fn run(_a: DbDownArgs, ctx: &ToolCtx) -> Result<DbMigrateReport> {
            svc(ctx)?.down().await
        }
    }
}
