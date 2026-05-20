//! orca.db admin domain — schema status, migrate, up, down.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

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

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::services::db_admin::DbAdminService;
    use crate::test_support::empty_ctx;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Stub {
        status_calls: AtomicUsize,
        migrate_calls: AtomicUsize,
        up_calls: AtomicUsize,
        down_calls: AtomicUsize,
    }
    impl Stub {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                status_calls: AtomicUsize::new(0),
                migrate_calls: AtomicUsize::new(0),
                up_calls: AtomicUsize::new(0),
                down_calls: AtomicUsize::new(0),
            })
        }
    }
    fn rep(dir: &str, before: i64, after: i64) -> DbMigrateReport {
        DbMigrateReport {
            before,
            after,
            applied: (after - before).unsigned_abs() as u32,
            direction: dir.into(),
        }
    }
    #[async_trait]
    impl DbAdminService for Stub {
        async fn status(&self) -> Result<DbStatusReport> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            Ok(DbStatusReport {
                current: 42,
                total: 50,
                pending: 8,
            })
        }
        async fn migrate(&self) -> Result<DbMigrateReport> {
            self.migrate_calls.fetch_add(1, Ordering::SeqCst);
            Ok(rep("up", 42, 50))
        }
        async fn up(&self) -> Result<DbMigrateReport> {
            self.up_calls.fetch_add(1, Ordering::SeqCst);
            Ok(rep("up", 42, 43))
        }
        async fn down(&self) -> Result<DbMigrateReport> {
            self.down_calls.fetch_add(1, Ordering::SeqCst);
            Ok(rep("down", 42, 41))
        }
    }

    fn ctx_with_stub() -> (orca_utils::tool::ToolCtx, Arc<Stub>) {
        let stub = Stub::new();
        let svc: Arc<dyn DbAdminService> = stub.clone();
        let mut ctx = empty_ctx();
        ctx.register_service(svc);
        (ctx, stub)
    }

    #[tokio::test]
    async fn db_status_forwards_to_service() {
        let (ctx, stub) = ctx_with_stub();
        let r = db_status(DbStatusArgs {}, &ctx).await.unwrap();
        assert_eq!(r.current, 42);
        assert_eq!(r.total, 50);
        assert_eq!(r.pending, 8);
        assert_eq!(stub.status_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn db_migrate_forwards_and_returns_delta() {
        let (ctx, stub) = ctx_with_stub();
        let r = db_migrate(DbMigrateArgs {}, &ctx).await.unwrap();
        assert_eq!(r.before, 42);
        assert_eq!(r.after, 50);
        assert_eq!(r.applied, 8);
        assert_eq!(r.direction, "up");
        assert_eq!(stub.migrate_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn db_up_forwards_single_step() {
        let (ctx, stub) = ctx_with_stub();
        let r = db_up(DbUpArgs {}, &ctx).await.unwrap();
        assert_eq!(r.after - r.before, 1);
        assert_eq!(stub.up_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn db_down_forwards_single_step_back() {
        let (ctx, stub) = ctx_with_stub();
        let r = db_down(DbDownArgs {}, &ctx).await.unwrap();
        assert_eq!(r.direction, "down");
        assert_eq!(r.before - r.after, 1);
        assert_eq!(stub.down_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn db_tools_error_when_service_missing() {
        let ctx = empty_ctx();
        assert!(db_status(DbStatusArgs {}, &ctx).await.is_err());
        assert!(db_migrate(DbMigrateArgs {}, &ctx).await.is_err());
        assert!(db_up(DbUpArgs {}, &ctx).await.is_err());
        assert!(db_down(DbDownArgs {}, &ctx).await.is_err());
    }
}
