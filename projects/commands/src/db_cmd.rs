use anyhow::Result;
use brain_utils::consts::APP_NAME;
use brain_utils::db::{self, MigrateDirection};
use clap::Subcommand;

/// Subcommands for `orca db`.
#[derive(Subcommand)]
pub enum DbAction {
    /// Apply the next pending migration (one step up).
    Up,
    /// Revert the most recently applied migration (one step down).
    Down,
    /// Apply all pending migrations (default: same as `orca db migrate`).
    Migrate,
    /// Show current schema version and pending migration count.
    Status,
}

/// Dispatch `orca db <action>`.
pub fn cmd_db(action: DbAction) -> Result<()> {
    let conn = db::open_default()?;
    let current = db::schema_version(&conn)?;
    let total = brain_utils::db::migration_count();

    match action {
        DbAction::Up => {
            println!("Migrating up one step (current: v{current})…");
            let new = db::migrate(&conn, MigrateDirection::Up, 1)?;
            println!("Schema now at v{new}.");
        }
        DbAction::Down => {
            println!("Rolling back one step (current: v{current})…");
            let new = db::migrate(&conn, MigrateDirection::Down, 1)?;
            println!("Schema now at v{new}.");
        }
        DbAction::Migrate => {
            println!("Applying all pending migrations (current: v{current})…");
            let new = db::migrate(&conn, MigrateDirection::Up, usize::MAX)?;
            println!("Schema now at v{new}.");
        }
        DbAction::Status => {
            let pending = total.saturating_sub(current as usize);
            println!("Schema version : v{current}");
            println!("Total defined  : {total}");
            println!("Pending        : {pending}");
            if pending == 0 {
                println!("Status         : up to date");
            } else {
                println!("Status         : {pending} migration(s) pending — run `{APP_NAME} db migrate`");
            }
        }
    }
    Ok(())
}
