//! Server-side `DbAdminService` impl — wraps `db::migrate*` helpers.

use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::orca_db::{DbMigrateReport, DbStatusReport};
use orca_tools_def::services::db_admin::DbAdminService;

pub struct ServerDbAdmin;

impl ServerDbAdmin {
    fn migrate(
        direction: db::MigrateDirection,
        steps: usize,
        label: &str,
    ) -> Result<DbMigrateReport> {
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
}

#[async_trait]
impl DbAdminService for ServerDbAdmin {
    async fn status(&self) -> Result<DbStatusReport> {
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

    async fn migrate(&self) -> Result<DbMigrateReport> {
        Self::migrate(db::MigrateDirection::Up, usize::MAX, "up-all")
    }

    async fn up(&self) -> Result<DbMigrateReport> {
        Self::migrate(db::MigrateDirection::Up, 1, "up")
    }

    async fn down(&self) -> Result<DbMigrateReport> {
        Self::migrate(db::MigrateDirection::Down, 1, "down")
    }
}
