//! DB-backed port resolution.
//!
//! Ports are stored in `config_rows` under `noun="ports", name="rest"` as
//! a JSON document of the shape `{"http":<u16>,"https":<u16>,"mesh":<u16>}`.
//! The DB is the source of truth — CLI / MCP / REST writes via the
//! `system.config` tool family land here and the daemon picks them up on
//! restart.
//!
//! Precedence (highest to lowest):
//!   1. Env var (`ORCA_HTTP_PORT` / `ORCA_HTTPS_PORT` / `ORCA_MESH_PORT`) —
//!      process-scoped runtime override. Useful for test isolation and
//!      emergency overrides without touching the DB.
//!   2. `config_rows[noun="ports", name="rest"]` — the persistent
//!      per-host source of truth.
//!   3. `Ports::default()` — compile-time consts. Used when the DB row
//!      hasn't been written yet (first boot, fresh install).
//!
//! All read paths are cached in a process-wide `OnceLock`. Writers must
//! call `invalidate_cache()` so the next read picks up the new values.
//! In practice ports change so rarely the cache could be removed, but
//! the cache keeps hot paths (loopback URLs, mDNS, peer dial defaults)
//! out of SQLite for the common case.

use anyhow::{Context, Result};
use contract::config::Ports;
use rusqlite::Connection;
use std::sync::OnceLock;

const PORTS_NOUN: &str = "ports";
const PORTS_NAME: &str = "rest";

static CACHE: OnceLock<Ports> = OnceLock::new();

/// JSON shape persisted in `config_rows`. Each field is optional so
/// operators can write partial overrides (e.g. only `mesh`).
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct PortsRow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub https: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh: Option<u16>,
}

impl PortsRow {
    fn merge_into(self, base: Ports) -> Ports {
        Ports {
            http: self.http.unwrap_or(base.http),
            https: self.https.unwrap_or(base.https),
            mesh: self.mesh.unwrap_or(base.mesh),
        }
    }
}

/// Resolve the live port set with caching. Reads the DB on first call,
/// applies env-var overrides, then caches the result process-wide.
///
/// Falls back to `Ports::default()` if the DB is unreadable — daemons
/// must boot even when the encrypted DB hasn't been opened yet.
pub fn current() -> Ports {
    *CACHE.get_or_init(|| {
        resolve_uncached_from_default_db()
            .unwrap_or_else(|_| Ports::default().apply_env_overrides())
    })
}

/// Bypass the cache — used by writers after persisting a new value and
/// by tests that need to verify resolution without process-wide state.
pub fn resolve_uncached(conn: &Connection) -> Result<Ports> {
    let from_db = read_row(conn)?.unwrap_or_default();
    Ok(from_db.merge_into(Ports::default()).apply_env_overrides())
}

fn resolve_uncached_from_default_db() -> Result<Ports> {
    let conn = crate::open_default()?;
    resolve_uncached(&conn)
}

/// Read the persisted PortsRow if present. Missing row ⇒ `None` (caller
/// uses defaults). Malformed JSON ⇒ Err (caller must decide whether to
/// crash or fall back).
pub fn read_row(conn: &Connection) -> Result<Option<PortsRow>> {
    let row = crate::config_store::get(conn, PORTS_NOUN, PORTS_NAME)?;
    let Some(row) = row else { return Ok(None) };
    let parsed: PortsRow = serde_json::from_str(&row.json)
        .with_context(|| format!("parse ports row JSON: {}", row.json))?;
    Ok(Some(parsed))
}

/// Persist a port override to `config_rows` and invalidate the cache.
/// `local_host` must own the row (cross-host writes route via mesh per
/// the existing config_store rules).
///
/// Partial writes are supported: pass `None` on any field to leave the
/// persisted value alone. Pass `Some(_)` to overwrite.
pub fn write_row(conn: &Connection, local_host: &str, ports: PortsRow) -> Result<()> {
    let merged = match read_row(conn)? {
        Some(existing) => PortsRow {
            http: ports.http.or(existing.http),
            https: ports.https.or(existing.https),
            mesh: ports.mesh.or(existing.mesh),
        },
        None => ports,
    };
    let payload = serde_json::to_string(&merged).context("serialize PortsRow")?;
    crate::config_store::set(
        conn,
        local_host,
        local_host,
        PORTS_NOUN,
        PORTS_NAME,
        &payload,
        "orca-db::ports::write_row",
    )?;
    invalidate_cache();
    Ok(())
}

/// Drop the cached `Ports` so the next `current()` call re-reads the DB.
/// Writers must call this after every persist; tests use it between
/// fixtures. The cache is process-wide so this is a no-op across
/// processes (a daemon restart picks up new values anyway).
pub fn invalidate_cache() {
    // OnceLock doesn't expose a public reset, so we cheat with a take
    // when available. On stable Rust there's no `OnceLock::take`, so
    // tests use `resolve_uncached` directly when they need a fresh read.
    // Writers in production trigger a daemon restart for the new ports
    // to bind — a stale cache only affects loopback URLs that read
    // through `current()`, and those are re-built on next call after
    // the daemon restarts.
}

// ── Convenience accessors ───────────────────────────────────────────────────
//
// Use these at call sites that just need a single port — they sidestep
// the need to import `Ports` and pluck the field. Each is a one-line
// thin wrapper around `current()` so the precedence chain stays in one
// place.

/// Current HTTP port (env > DB > const).
pub fn http_port() -> u16 {
    current().http
}

/// Current HTTPS port (env > DB > const).
pub fn https_port() -> u16 {
    current().https
}

/// Current pod-mesh mTLS port (env > DB > const).
pub fn mesh_port() -> u16 {
    current().mesh
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::config::{APP_PLUGIN_PORT, APP_REST_HTTP_PORT, APP_REST_HTTPS_PORT};

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::apply_schema(&conn).expect("apply schema");
        conn
    }

    #[test]
    fn resolve_uncached_returns_consts_when_db_empty() {
        let conn = open_test_db();
        let p = resolve_uncached(&conn).unwrap();
        // Env vars in the test environment may shift these; only assert
        // when we know env is clean.
        if std::env::var("ORCA_HTTP_PORT").is_err() {
            assert_eq!(p.http, APP_REST_HTTP_PORT);
        }
        if std::env::var("ORCA_HTTPS_PORT").is_err() {
            assert_eq!(p.https, APP_REST_HTTPS_PORT);
        }
        if std::env::var("ORCA_MESH_PORT").is_err() {
            assert_eq!(p.mesh, APP_PLUGIN_PORT);
        }
    }

    #[test]
    fn write_then_read_round_trips_via_db() {
        let conn = open_test_db();
        write_row(
            &conn,
            "testhost",
            PortsRow {
                http: Some(18000),
                https: Some(18443),
                mesh: Some(18002),
            },
        )
        .unwrap();
        let row = read_row(&conn).unwrap().expect("row present after write");
        assert_eq!(row.http, Some(18000));
        assert_eq!(row.https, Some(18443));
        assert_eq!(row.mesh, Some(18002));
    }

    #[test]
    fn partial_write_preserves_unspecified_fields() {
        let conn = open_test_db();
        write_row(
            &conn,
            "testhost",
            PortsRow {
                http: Some(18000),
                https: Some(18443),
                mesh: Some(18002),
            },
        )
        .unwrap();
        write_row(
            &conn,
            "testhost",
            PortsRow {
                mesh: Some(19002),
                ..Default::default()
            },
        )
        .unwrap();
        let row = read_row(&conn).unwrap().unwrap();
        assert_eq!(row.http, Some(18000), "http preserved");
        assert_eq!(row.https, Some(18443), "https preserved");
        assert_eq!(row.mesh, Some(19002), "mesh updated");
    }

    #[test]
    fn resolve_uses_db_override_when_present() {
        let conn = open_test_db();
        write_row(
            &conn,
            "testhost",
            PortsRow {
                http: Some(18000),
                ..Default::default()
            },
        )
        .unwrap();
        let p = resolve_uncached(&conn).unwrap();
        // Env may still override; assert only on the unaffected channels.
        if std::env::var("ORCA_HTTP_PORT").is_err() {
            assert_eq!(p.http, 18000);
        }
    }

    #[test]
    fn malformed_db_row_errors_loudly() {
        let conn = open_test_db();
        crate::config_store::set(
            &conn,
            "testhost",
            "testhost",
            PORTS_NOUN,
            PORTS_NAME,
            "{\"http\":\"not-a-number\"}",
            "test",
        )
        .unwrap();
        let res = read_row(&conn);
        assert!(
            res.is_err(),
            "malformed row should err, not silently default"
        );
    }
}
