//! `DbAdminService` — schema migration runner for orca.db.
//!
//! Wraps the existing `db::migrate_*` helpers so the `db.{status,migrate,up,down}`
//! tools can drive migrations from any surface (CLI, web UI, MCP).

use anyhow::Result;
use async_trait::async_trait;

use crate::orca_db::{DbMigrateReport, DbStatusReport};

#[async_trait]
pub trait DbAdminService: Send + Sync {
    async fn status(&self) -> Result<DbStatusReport>;
    async fn migrate(&self) -> Result<DbMigrateReport>;
    async fn up(&self) -> Result<DbMigrateReport>;
    async fn down(&self) -> Result<DbMigrateReport>;
}
