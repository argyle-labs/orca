//! Runtime helpers used by `endpoint_resource!`-generated code.
//!
//! These exist as free functions (rather than inline-emitted by the macro)
//! so the macro expansion stays small and the error-mapping logic has one
//! home. Adding behaviour here — better error messages, telemetry hooks,
//! mesh sync hints — automatically benefits every plugin that uses the
//! toolkit, per the "power scales with the macro" rule.

use anyhow::{Result, anyhow, bail};
use rusqlite::Connection;

use std::sync::OnceLock;

use crate::abi::{DbOp, DbReply, DbValue, HostDbOp, HostSecretOp, SecretOp, SecretReply};
use crate::capsink::cap_route;
use abi_stable::std_types::{RResult, RStr};

/// Open the default orca SQLite db. Plugin-generated tools all route
/// through this so a future swap of the storage layer is a single call
/// site change.
pub fn open_db() -> Result<Connection> {
    db::open_default()
}

// The out-of-process capability sink lives in [`crate::capsink`] (dependency-
// light, no rusqlite) so the delegated-HTTP shim can reach it without the `db`
// feature. `db_op`/`secret_op` below route through `cap_route` when a sink is
// installed, else fall back to the FFI / in-core pooled paths.

// ── Host DB service (set by the loader via `PluginMod::set_host`) ─────────────
//
// A plugin NEVER opens its own SQLite connection — a second connection to the
// encrypted db races the daemon's on the WAL/shm index (SQLITE_IOERR_SHMOPEN
// 5898). Instead the loader hands the plugin a `HostDbOp` bound to core's single
// pooled connection, stored here; every generated CRUD call routes through it.

static HOST_DB: OnceLock<HostDbOp> = OnceLock::new();

/// Install core's DB service. Called once by the plugin's exported `__set_host`
/// (which the loader invokes right after the compat gate). First call wins.
pub fn set_host_db(op: HostDbOp) {
    // First install wins; a duplicate call (shouldn't happen) keeps the
    // original binding rather than swapping it mid-flight.
    if HOST_DB.set(op).is_err() {
        debug_assert!(false, "set_host_db called more than once");
    }
}

/// Execute a typed CRUD op through core's connection. This is the ONLY db path
/// generated `endpoint_resource!` code uses.
///
/// Two callers, one destination (core's single pooled connection):
/// * **Loaded cdylib plugin** — the loader installed a [`HostDbOp`] via
///   `set_host`, so we hop the FFI boundary into core's `exec_db_op_pooled`.
/// * **In-core `endpoint_resource!`** (e.g. `managed_mounts`, compiled into the
///   daemon) — no loader ran, so `HOST_DB` is empty. We call the same pooled
///   executor directly. Without this fallback, in-core CRUD failed with
///   "core DB service not installed" even though the daemon owns the connection.
pub fn db_op(op: &DbOp) -> Result<DbReply> {
    // Subprocess mode: route through the capability sink if one is installed.
    if let Some(reply_json) = cap_route("db.op", &serde_json::to_string(op)?) {
        return Ok(serde_json::from_str(&reply_json?)?);
    }
    if let Some(host) = HOST_DB.get() {
        let json = serde_json::to_string(op)?;
        return match (host.func)(RStr::from_str(&json)) {
            RResult::ROk(s) => Ok(serde_json::from_str(s.as_str())?),
            RResult::RErr(e) => bail!("core db_op failed: {e}"),
        };
    }
    #[cfg(feature = "db")]
    {
        db::plugin_tables::exec_db_op_pooled(op)
    }
    #[cfg(not(feature = "db"))]
    Err(anyhow!(
        "core DB service not installed (daemon predates set_host?)"
    ))
}

// ── Host secrets service (set by the loader via `PluginMod::set_secret_op`) ────

static HOST_SECRET: OnceLock<HostSecretOp> = OnceLock::new();

/// Install core's secrets service. Called once by the plugin's exported
/// `__set_secret_op` (which the loader invokes right after `set_host`).
pub fn set_host_secret_op(op: HostSecretOp) {
    if HOST_SECRET.set(op).is_err() {
        debug_assert!(false, "set_host_secret_op called more than once");
    }
}

/// Run a secrets op through core's connection — the only secrets path plugin
/// code uses. Errors if the host never installed the service.
pub fn secret_op(op: &SecretOp) -> Result<SecretReply> {
    // Subprocess mode: route through the capability sink if one is installed.
    if let Some(reply_json) = cap_route("secret.op", &serde_json::to_string(op)?) {
        return Ok(serde_json::from_str(&reply_json?)?);
    }
    if let Some(host) = HOST_SECRET.get() {
        let json = serde_json::to_string(op)?;
        return match (host.func)(RStr::from_str(&json)) {
            RResult::ROk(s) => Ok(serde_json::from_str(s.as_str())?),
            RResult::RErr(e) => bail!("core secret_op failed: {e}"),
        };
    }
    // In-core fallback: same pooled connection the loader would have handed a
    // plugin. See `db_op` for the full rationale.
    #[cfg(feature = "db")]
    {
        db::secrets::exec_secret_op_pooled(op)
    }
    #[cfg(not(feature = "db"))]
    Err(anyhow!(
        "core secrets service not installed (daemon predates set_secret_op?)"
    ))
}

// ── Typed cell conversion for generated CRUD ─────────────────────────────────
//
// The `endpoint_resource!` macro maps each row field to/from a [`DbValue`] via
// these traits so it never has to reason about SQLite storage classes — the
// same job rusqlite's `ToSql`/`FromSql` did before, over the typed FFI cell.

/// Convert a typed field into a [`DbValue`] for a write op.
pub trait ToDbValue {
    fn to_dbvalue(&self) -> DbValue;
}

/// Read a typed field back out of a [`DbValue`] from a read op.
pub trait FromDbValue: Sized {
    fn from_dbvalue(v: &DbValue) -> Result<Self>;
}

macro_rules! int_dbvalue {
    ($($t:ty),*) => { $(
        impl ToDbValue for $t {
            fn to_dbvalue(&self) -> DbValue { DbValue::Int(*self as i64) }
        }
        impl FromDbValue for $t {
            fn from_dbvalue(v: &DbValue) -> Result<Self> {
                match v {
                    DbValue::Int(n) => Ok(*n as $t),
                    DbValue::Bool(b) => Ok(*b as i64 as $t),
                    other => bail!(concat!("expected integer for ", stringify!($t), ", got {:?}"), other),
                }
            }
        }
    )* };
}
int_dbvalue!(i64, i32, i16, i8, u64, u32, u16, u8);

impl ToDbValue for String {
    fn to_dbvalue(&self) -> DbValue {
        DbValue::Text(self.clone())
    }
}
impl FromDbValue for String {
    fn from_dbvalue(v: &DbValue) -> Result<Self> {
        match v {
            DbValue::Text(s) => Ok(s.clone()),
            other => bail!("expected text, got {other:?}"),
        }
    }
}

impl ToDbValue for bool {
    fn to_dbvalue(&self) -> DbValue {
        DbValue::Bool(*self)
    }
}
impl FromDbValue for bool {
    fn from_dbvalue(v: &DbValue) -> Result<Self> {
        match v {
            DbValue::Bool(b) => Ok(*b),
            DbValue::Int(n) => Ok(*n != 0),
            other => bail!("expected bool, got {other:?}"),
        }
    }
}

impl ToDbValue for f64 {
    fn to_dbvalue(&self) -> DbValue {
        DbValue::Real(*self)
    }
}
impl FromDbValue for f64 {
    fn from_dbvalue(v: &DbValue) -> Result<Self> {
        match v {
            DbValue::Real(f) => Ok(*f),
            DbValue::Int(n) => Ok(*n as f64),
            other => bail!("expected real, got {other:?}"),
        }
    }
}

impl<T: ToDbValue> ToDbValue for Option<T> {
    fn to_dbvalue(&self) -> DbValue {
        match self {
            Some(x) => x.to_dbvalue(),
            None => DbValue::Null,
        }
    }
}
impl<T: FromDbValue> FromDbValue for Option<T> {
    fn from_dbvalue(v: &DbValue) -> Result<Self> {
        match v {
            DbValue::Null => Ok(None),
            other => Ok(Some(T::from_dbvalue(other)?)),
        }
    }
}

/// Pull column `col` out of a returned [`DbRow`] as a typed value (absent →
/// treated as `Null`). Used by generated CRUD read paths.
pub fn field_from_row<T: FromDbValue>(row: &crate::abi::DbRow, col: &str) -> Result<T> {
    T::from_dbvalue(row.get(col).unwrap_or(&DbValue::Null))
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
