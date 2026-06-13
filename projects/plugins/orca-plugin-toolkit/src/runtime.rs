//! Runtime helpers used by `endpoint_resource!`-generated code.
//!
//! These exist as free functions (rather than inline-emitted by the macro)
//! so the macro expansion stays small and the error-mapping logic has one
//! home. Adding behaviour here — better error messages, telemetry hooks,
//! mesh sync hints — automatically benefits every plugin that uses the
//! toolkit, per the "power scales with the macro" rule.

use anyhow::{Result, anyhow};
use rusqlite::Connection;

/// Open the default orca SQLite db. Plugin-generated tools all route
/// through this so a future swap of the storage layer is a single call
/// site change.
pub fn open_db() -> Result<Connection> {
    db::open_default()
}

/// Translate a SQLite UNIQUE / PRIMARY KEY constraint error from `insert`
/// into the user-facing "name already exists; use <plugin>.update"
/// message. Falls through unchanged for any other error so genuine I/O
/// failures aren't masked.
pub fn map_insert_conflict(err: anyhow::Error, plugin: &str, name: &str) -> anyhow::Error {
    let msg = format!("{err:#}");
    if msg.contains("UNIQUE") || msg.contains("PRIMARY") {
        anyhow!("{plugin} endpoint '{name}' already exists; use {plugin}.update")
    } else {
        err
    }
}

/// Build the "not registered; use <plugin>.create" error used by `.update`
/// and `.delete` when the row isn't found.
pub fn missing_row_error(plugin: &str, name: &str) -> anyhow::Error {
    anyhow!("{plugin} endpoint '{name}' not registered; use {plugin}.create")
}
