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

use crate::abi::{
    DbOp, DbReply, DbValue, HostDbOp, HostSecretOp, HttpRequest, HttpResponse, SecretOp,
    SecretReply,
};
use abi_stable::std_types::{RResult, RStr};

/// Open the default orca SQLite db. Plugin-generated tools all route
/// through this so a future swap of the storage layer is a single call
/// site change.
pub fn open_db() -> Result<Connection> {
    db::open_default()
}

// ── Out-of-process capability sink ───────────────────────────────────────────
//
// In the subprocess plugin model there is no `set_host` FFI. Instead the
// toolkit's serve loop installs a capability sink — a closure over the session
// socket — for the duration of each `Invoke`, and `db_op`/`secret_op` route
// their typed op through it as a `db.op` / `secret.op` capability round-trip.
//
// Thread-local because a plugin's serve loop and its `block_on(dispatch)` run on
// ONE thread (a current-thread runtime), serially: the sink is valid exactly
// while a tool is executing, so tool code deep in the call graph reaches the
// socket without threading a channel through every signature. When no sink is
// installed (cdylib load, or in-core `endpoint_resource!`) the existing FFI /
// pooled paths are used.

/// A capability round-trip: `(cap_name, op_json) -> reply_json | error`.
pub type CapSink = Box<dyn FnMut(&str, &str) -> std::result::Result<String, String>>;

thread_local! {
    static CAP_SINK: std::cell::RefCell<Option<CapSink>> =
        const { std::cell::RefCell::new(None) };
}

/// Install `sink` for the current thread, run `body`, then restore the previous
/// sink (even on panic). The serve loop wraps each `Invoke` dispatch in this.
/// Non-reentrant: a nested call replaces the sink for the inner scope.
pub fn with_cap_sink<R>(sink: CapSink, body: impl FnOnce() -> R) -> R {
    struct Restore(Option<CapSink>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CAP_SINK.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let prev = CAP_SINK.with(|c| c.borrow_mut().replace(sink));
    let _restore = Restore(prev);
    body()
}

/// Route one op through the installed capability sink. `Some(_)` in subprocess
/// mode; `None` when no sink is installed (fall through to FFI / in-core).
fn cap_route(cap: &str, op_json: &str) -> Option<Result<String>> {
    CAP_SINK.with(|c| {
        c.borrow_mut()
            .as_mut()
            .map(|sink| sink(cap, op_json).map_err(|e| anyhow!("capability {cap}: {e}")))
    })
}

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

// ── Host HTTP service (the `http.request` capability) ─────────────────────────

/// Perform an HTTP request through orca's runtime instead of a plugin-local
/// client. In subprocess mode this routes over the capability sink as an
/// `http.request` round-trip, so the plugin links **no** reqwest/rustls/hyper —
/// the single largest source of plugin bloat. The daemon executes it on its one
/// HTTP/TLS stack and relays the response for any status.
///
/// Errors if no capability sink is installed (i.e. not running as an orca
/// subprocess): an in-process cdylib still uses its own linked HTTP client, and
/// the delegated path is only meaningful when orca is on the other end of the
/// socket. This is the seam Phase B retargets progenitor-generated clients onto.
pub fn http_request(req: &HttpRequest) -> Result<HttpResponse> {
    match cap_route("http.request", &serde_json::to_string(req)?) {
        Some(reply_json) => Ok(serde_json::from_str(&reply_json?)?),
        None => Err(anyhow!(
            "http.request capability unavailable: this plugin is not running as an orca subprocess"
        )),
    }
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

#[cfg(test)]
mod cap_sink_tests {
    use super::*;

    #[test]
    fn no_sink_falls_through() {
        // With nothing installed, cap_route declines so the FFI/in-core path runs.
        assert!(cap_route("db.op", "{}").is_none());
    }

    #[test]
    fn sink_routes_within_scope_and_restores_after() {
        let out = with_cap_sink(
            Box::new(|cap: &str, json: &str| Ok(format!("reply:{cap}:{json}"))),
            || cap_route("secret.op", "{\"x\":1}"),
        );
        assert_eq!(out.unwrap().unwrap(), "reply:secret.op:{\"x\":1}");
        // Sink cleared once the scope ends.
        assert!(cap_route("secret.op", "{}").is_none());
    }

    #[test]
    fn http_request_routes_through_sink() {
        let req = HttpRequest {
            method: "GET".into(),
            url: "https://example.test/x".into(),
            headers: vec![("accept".into(), "application/json".into())],
            body: Vec::new(),
            timeout_ms: None,
            insecure: false,
        };
        let out = with_cap_sink(
            Box::new(|cap: &str, json: &str| {
                assert_eq!(cap, "http.request");
                // Echo a canned 204 response, asserting the request serialized.
                assert!(json.contains("example.test"));
                Ok(r#"{"status":204,"headers":[],"body":[]}"#.to_string())
            }),
            || http_request(&req),
        );
        assert_eq!(out.unwrap().status, 204);
    }

    #[test]
    fn http_request_without_sink_errors() {
        let req = HttpRequest {
            method: "GET".into(),
            url: "https://example.test".into(),
            headers: vec![],
            body: vec![],
            timeout_ms: None,
            insecure: false,
        };
        let err = http_request(&req).unwrap_err().to_string();
        assert!(
            err.contains("not running as an orca subprocess"),
            "got: {err}"
        );
    }

    #[test]
    fn sink_error_maps_to_anyhow() {
        let r = with_cap_sink(Box::new(|_: &str, _: &str| Err("boom".to_string())), || {
            cap_route("db.op", "{}")
        });
        let err = r.unwrap().unwrap_err().to_string();
        assert!(err.contains("db.op") && err.contains("boom"), "got: {err}");
    }
}
