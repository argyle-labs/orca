//! Config store — typed, host-owned rows that drive the scheduler, services,
//! backups, NFS watches, chown sweeps, and other runtime configuration.
//!
//! Ownership model (see `docs/planned/orca-v1-scope.md` §3.1):
//! every row carries a `host_owner`. Only the owning host may write. Other
//! hosts may hold replicas (`is_replica = 1`) for fast local reads, but
//! attempts to mutate a replica directly are rejected — the write must be
//! routed to the owner.
//!
//! Each row's payload is JSON validated against the schema registered for
//! its `noun`. v1 enforces only that the payload parses as JSON; full
//! JSON-Schema validation lands in a follow-up (will use the schema_json
//! column already stored here).

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigRow {
    pub id: String,
    pub host_owner: String,
    pub noun: String,
    pub name: String,
    /// JSON payload as stored. Always a valid JSON document.
    pub json: String,
    pub is_replica: bool,
    pub updated_at: String,
    pub updated_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigSchema {
    pub noun: String,
    pub schema_json: String,
    /// JSON array of dotted field paths considered sensitive — never
    /// serialized to git, never replicated over the mesh.
    pub sensitive_fields: String,
    pub registered_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigHistoryEntry {
    pub id: i64,
    pub row_id: String,
    pub prior_json: String,
    pub changed_at: String,
    pub changed_by: String,
}

// ── Schema registry ──────────────────────────────────────────────────────────

pub fn register_schema(
    conn: &Connection,
    noun: &str,
    schema_json: &str,
    sensitive_fields: &[&str],
) -> Result<()> {
    // Validate inputs parse as JSON.
    serde_json::from_str::<serde_json::Value>(schema_json)
        .with_context(|| format!("schema_json for noun {noun} is not valid JSON"))?;
    let sensitive = serde_json::to_string(sensitive_fields)?;
    conn.execute(
        "INSERT INTO config_schemas (noun, schema_json, sensitive_fields)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(noun) DO UPDATE SET
             schema_json      = excluded.schema_json,
             sensitive_fields = excluded.sensitive_fields,
             registered_at    = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        params![noun, schema_json, sensitive],
    )?;
    Ok(())
}

pub fn get_schema(conn: &Connection, noun: &str) -> Result<Option<ConfigSchema>> {
    let r = conn
        .query_row(
            "SELECT noun, schema_json, sensitive_fields, registered_at
             FROM config_schemas WHERE noun = ?1",
            params![noun],
            |r| {
                Ok(ConfigSchema {
                    noun: r.get(0)?,
                    schema_json: r.get(1)?,
                    sensitive_fields: r.get(2)?,
                    registered_at: r.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(r)
}

pub fn list_schemas(conn: &Connection) -> Result<Vec<ConfigSchema>> {
    let mut stmt = conn.prepare(
        "SELECT noun, schema_json, sensitive_fields, registered_at
         FROM config_schemas ORDER BY noun",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ConfigSchema {
            noun: r.get(0)?,
            schema_json: r.get(1)?,
            sensitive_fields: r.get(2)?,
            registered_at: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ── Row CRUD ─────────────────────────────────────────────────────────────────

/// List rows, optionally filtered by `noun` and/or `host_owner`.
pub fn list(
    conn: &Connection,
    noun: Option<&str>,
    host_owner: Option<&str>,
) -> Result<Vec<ConfigRow>> {
    let mut sql = String::from(
        "SELECT id, host_owner, noun, name, json, is_replica, updated_at, updated_by
         FROM config_rows WHERE 1=1",
    );
    let mut args: Vec<String> = Vec::new();
    if let Some(n) = noun {
        sql.push_str(" AND noun = ?");
        args.push(n.to_string());
    }
    if let Some(h) = host_owner {
        sql.push_str(" AND host_owner = ?");
        args.push(h.to_string());
    }
    sql.push_str(" ORDER BY noun, name, host_owner");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), row_from)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn get(conn: &Connection, noun: &str, name: &str) -> Result<Option<ConfigRow>> {
    let r = conn
        .query_row(
            "SELECT id, host_owner, noun, name, json, is_replica, updated_at, updated_by
             FROM config_rows WHERE noun = ?1 AND name = ?2",
            params![noun, name],
            row_from,
        )
        .optional()?;
    Ok(r)
}

/// Upsert a row owned by `host_owner`. Refuses to write if the caller's
/// `local_host` does not match `host_owner` — cross-host writes must be
/// routed via mesh (§3.3). Returns true if a new row was created.
///
/// `payload_json` must be a valid JSON document. v1 does not yet enforce
/// the registered schema's shape — that lands as a follow-up.
pub fn set(
    conn: &Connection,
    local_host: &str,
    host_owner: &str,
    noun: &str,
    name: &str,
    payload_json: &str,
    updated_by: &str,
) -> Result<bool> {
    if host_owner != local_host {
        bail!(
            "refusing to write config row owned by '{host_owner}' from host '{local_host}' \
             — route via mesh once peer dispatch lands (§3.3)"
        );
    }
    serde_json::from_str::<serde_json::Value>(payload_json)
        .with_context(|| format!("payload for {noun}/{name} is not valid JSON"))?;

    let row_id = format!("{noun}:{name}@{host_owner}");
    let prior = get_by_id(conn, &row_id)?;

    if let Some(p) = &prior {
        record_history(conn, &p.id, &p.json, updated_by)?;
    }

    conn.execute(
        "INSERT INTO config_rows (id, host_owner, noun, name, json, is_replica, updated_at, updated_by)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), ?6)
         ON CONFLICT(id) DO UPDATE SET
             json       = excluded.json,
             is_replica = 0,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
             updated_by = excluded.updated_by",
        params![row_id, host_owner, noun, name, payload_json, updated_by],
    )?;
    Ok(prior.is_none())
}

/// Apply a replica row received from the owner via mesh. Sets `is_replica = 1`.
/// Skips the local-host ownership check; the caller (mesh handler) is
/// responsible for verifying the peer authority.
pub fn apply_replica(
    conn: &Connection,
    host_owner: &str,
    noun: &str,
    name: &str,
    payload_json: &str,
    updated_by: &str,
) -> Result<()> {
    serde_json::from_str::<serde_json::Value>(payload_json)
        .with_context(|| format!("replica payload for {noun}/{name} is not valid JSON"))?;
    let row_id = format!("{noun}:{name}@{host_owner}");
    conn.execute(
        "INSERT INTO config_rows (id, host_owner, noun, name, json, is_replica, updated_at, updated_by)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), ?6)
         ON CONFLICT(id) DO UPDATE SET
             json       = excluded.json,
             is_replica = 1,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
             updated_by = excluded.updated_by",
        params![row_id, host_owner, noun, name, payload_json, updated_by],
    )?;
    Ok(())
}

pub fn delete(
    conn: &Connection,
    local_host: &str,
    host_owner: &str,
    noun: &str,
    name: &str,
    deleted_by: &str,
) -> Result<bool> {
    if host_owner != local_host {
        bail!(
            "refusing to delete config row owned by '{host_owner}' from host '{local_host}' \
             — route via mesh once peer dispatch lands (§3.3)"
        );
    }
    let row_id = format!("{noun}:{name}@{host_owner}");
    if let Some(p) = get_by_id(conn, &row_id)? {
        record_history(conn, &p.id, &p.json, deleted_by)?;
    }
    let n = conn.execute("DELETE FROM config_rows WHERE id = ?1", params![row_id])?;
    Ok(n > 0)
}

// ── History ──────────────────────────────────────────────────────────────────

pub fn history(conn: &Connection, row_id: &str) -> Result<Vec<ConfigHistoryEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, row_id, prior_json, changed_at, changed_by
         FROM config_history WHERE row_id = ?1 ORDER BY id DESC",
    )?;
    let rows = stmt.query_map(params![row_id], |r| {
        Ok(ConfigHistoryEntry {
            id: r.get(0)?,
            row_id: r.get(1)?,
            prior_json: r.get(2)?,
            changed_at: r.get(3)?,
            changed_by: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn record_history(
    conn: &Connection,
    row_id: &str,
    prior_json: &str,
    changed_by: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO config_history (row_id, prior_json, changed_by)
         VALUES (?1, ?2, ?3)",
        params![row_id, prior_json, changed_by],
    )?;
    Ok(())
}

fn get_by_id(conn: &Connection, row_id: &str) -> Result<Option<ConfigRow>> {
    let r = conn
        .query_row(
            "SELECT id, host_owner, noun, name, json, is_replica, updated_at, updated_by
             FROM config_rows WHERE id = ?1",
            params![row_id],
            row_from,
        )
        .optional()?;
    Ok(r)
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<ConfigRow> {
    Ok(ConfigRow {
        id: r.get(0)?,
        host_owner: r.get(1)?,
        noun: r.get(2)?,
        name: r.get(3)?,
        json: r.get(4)?,
        is_replica: r.get::<_, i64>(5)? != 0,
        updated_at: r.get(6)?,
        updated_by: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    const LOCAL: &str = "thor";

    fn set_local(conn: &Connection, noun: &str, name: &str, json: &str) -> Result<bool> {
        set(conn, LOCAL, LOCAL, noun, name, json, "test")
    }

    #[test]
    fn set_get_round_trip() {
        let conn = test_conn();
        let created = set_local(&conn, "service", "plex", r#"{"runtime":"lxc:110"}"#).unwrap();
        assert!(created);

        let r = get(&conn, "service", "plex").unwrap().unwrap();
        assert_eq!(r.noun, "service");
        assert_eq!(r.name, "plex");
        assert_eq!(r.host_owner, "thor");
        assert!(!r.is_replica);
        let v: serde_json::Value = serde_json::from_str(&r.json).unwrap();
        assert_eq!(v["runtime"], "lxc:110");
    }

    #[test]
    fn set_records_history_on_update() {
        let conn = test_conn();
        set_local(&conn, "service", "plex", r#"{"v":1}"#).unwrap();
        let created = set_local(&conn, "service", "plex", r#"{"v":2}"#).unwrap();
        assert!(!created, "second set should be an update");

        let row = get(&conn, "service", "plex").unwrap().unwrap();
        let h = history(&conn, &row.id).unwrap();
        assert_eq!(h.len(), 1);
        assert!(h[0].prior_json.contains("\"v\":1"));
    }

    #[test]
    fn cross_host_write_refused() {
        let conn = test_conn();
        let err = set(&conn, "thor", "frigg", "service", "jellyfin", "{}", "test").unwrap_err();
        assert!(err.to_string().contains("refusing to write"), "got: {err}");
    }

    #[test]
    fn apply_replica_marks_row_as_replica() {
        let conn = test_conn();
        apply_replica(&conn, "frigg", "service", "jellyfin", r#"{"v":1}"#, "mesh").unwrap();
        let r = get(&conn, "service", "jellyfin").unwrap().unwrap();
        assert!(r.is_replica);
        assert_eq!(r.host_owner, "frigg");
    }

    #[test]
    fn delete_records_history_and_removes() {
        let conn = test_conn();
        set_local(&conn, "schedule", "host.backup", r#"{"cron":"0 * * * *"}"#).unwrap();
        let removed = delete(&conn, LOCAL, LOCAL, "schedule", "host.backup", "test").unwrap();
        assert!(removed);
        assert!(get(&conn, "schedule", "host.backup").unwrap().is_none());

        let row_id = "schedule:host.backup@thor";
        let h = history(&conn, row_id).unwrap();
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn invalid_json_rejected() {
        let conn = test_conn();
        let err = set_local(&conn, "service", "plex", "not-json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "got: {err}");
    }

    #[test]
    fn list_filters_by_noun_and_owner() {
        let conn = test_conn();
        set_local(&conn, "service", "plex", "{}").unwrap();
        set_local(&conn, "service", "immich", "{}").unwrap();
        set_local(&conn, "schedule", "host.backup", "{}").unwrap();

        let services = list(&conn, Some("service"), None).unwrap();
        assert_eq!(services.len(), 2);

        let all_thor = list(&conn, None, Some("thor")).unwrap();
        assert_eq!(all_thor.len(), 3);

        let none = list(&conn, None, Some("frigg")).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn schema_register_and_get() {
        let conn = test_conn();
        register_schema(
            &conn,
            "service",
            r#"{"type":"object","properties":{"runtime":{"type":"string"}}}"#,
            &["api_key", "password"],
        )
        .unwrap();
        let s = get_schema(&conn, "service").unwrap().unwrap();
        assert_eq!(s.noun, "service");
        let sensitive: Vec<String> = serde_json::from_str(&s.sensitive_fields).unwrap();
        assert_eq!(sensitive, vec!["api_key", "password"]);
    }
}
