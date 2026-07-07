//! Config store — typed, host-owned rows that drive the scheduler, services,
//! backups, NFS watches, chown sweeps, and other runtime configuration.
//!
//! Ownership model: every row carries a `host_owner`. Only the owning host may write. Other
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
    // Validate input parses as JSON without materializing the tree — schema
    // shape is genuinely free-form (varies per plugin), and we re-serialize
    // the raw string into the DB unchanged.
    serde_json::from_str::<serde::de::IgnoredAny>(schema_json)
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
    serde_json::from_str::<serde::de::IgnoredAny>(payload_json)
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
    // Origin write → fan out to the mesh so every node holds a replica.
    crate::replicate::notify_write("config_rows");
    Ok(prior.is_none())
}

// ── Mesh replication ────────────────────────────────────────────────────────
//
// config_rows replicates across the pod mesh so EVERY node holds a copy of
// every node's config — data resiliency: any peer can restore a machine's
// config. Custom registration (not `#[derive(Replicated)]`) because config has
// an ownership model the generic derive doesn't capture:
//   - export ALL rows (owned + replicas) so the fleet gossips every node's
//     config to every node, and a reinstalled host can pull its OWN rows back
//     from any peer to restore itself.
//   - merge recomputes `is_replica` on the RECEIVER from ownership (owned when
//     host_owner == this host — the restore path — else a replica), preserves
//     the ORIGIN `updated_at`, and applies last-write-wins.
// Secrets never appear here — they live in the separate `secrets` table, which
// is not registered for replication.

/// Upsert a row received over the mesh, preserving the origin `updated_at` and
/// applying last-write-wins (only overwrite when the incoming row is strictly
/// newer). The caller decides `is_replica`: false when this host is the owner
/// (restore path — re-owning our own config), true otherwise. Returns true iff
/// a row was inserted/updated.
#[allow(clippy::too_many_arguments)]
pub fn upsert_mesh_row(
    conn: &Connection,
    host_owner: &str,
    noun: &str,
    name: &str,
    payload_json: &str,
    updated_at: &str,
    updated_by: &str,
    is_replica: bool,
) -> Result<bool> {
    serde_json::from_str::<serde::de::IgnoredAny>(payload_json)
        .with_context(|| format!("mesh payload for {noun}/{name} is not valid JSON"))?;
    let row_id = format!("{noun}:{name}@{host_owner}");
    let rep = i64::from(is_replica);
    let n = conn.execute(
        "INSERT INTO config_rows (id, host_owner, noun, name, json, is_replica, updated_at, updated_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
             json       = excluded.json,
             is_replica = excluded.is_replica,
             updated_at = excluded.updated_at,
             updated_by = excluded.updated_by
         WHERE excluded.updated_at > config_rows.updated_at",
        params![row_id, host_owner, noun, name, payload_json, rep, updated_at, updated_by],
    )?;
    Ok(n > 0)
}

/// Export ALL rows — owned AND replicas — so the fleet gossips every node's
/// config to every node (transitive propagation), and a reinstalled host can
/// pull its OWN rows back from any peer to restore itself. `is_replica` is not
/// exported: the receiver recomputes it from ownership in `replicate_merge`.
// The replication bundle boundary is genuinely free-form JSON (the registry is
// heterogeneous), same as sibling `replicate.rs` — Value is the right tool.
#[allow(clippy::disallowed_types)]
fn replicate_export(conn: &Connection) -> Result<serde_json::Value> {
    let mut stmt = conn.prepare(
        "SELECT host_owner, noun, name, json, updated_at, updated_by
           FROM config_rows ORDER BY id",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            Ok(serde_json::json!({
                "host_owner": r.get::<_, String>(0)?,
                "noun":       r.get::<_, String>(1)?,
                "name":       r.get::<_, String>(2)?,
                "json":       r.get::<_, String>(3)?,
                "updated_at": r.get::<_, String>(4)?,
                "updated_by": r.get::<_, String>(5)?,
            }))
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(serde_json::Value::Array(rows))
}

#[allow(clippy::disallowed_types)]
fn replicate_merge(conn: &Connection, rows: serde_json::Value) -> Result<usize> {
    // Ownership decides is_replica on the RECEIVER: a row whose host_owner is
    // THIS host is applied as OWNED (is_replica=0) — that's the restore path,
    // where a reinstalled node re-owns its config pulled from a peer. Everything
    // else lands as a replica. If we can't resolve our own name, treat all as
    // replicas (safe; restore requires host.display_name to be set).
    let local = crate::settings::get(conn, "host.display_name")
        .ok()
        .flatten()
        .unwrap_or_default();
    let arr = rows.as_array().cloned().unwrap_or_default();
    let mut merged = 0usize;
    for row in arr {
        let field = |k: &str| {
            row.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let host_owner = field("host_owner");
        let noun = field("noun");
        let name = field("name");
        if host_owner.is_empty() || noun.is_empty() || name.is_empty() {
            continue;
        }
        let json = field("json");
        let updated_at = field("updated_at");
        let updated_by = {
            let u = field("updated_by");
            if u.is_empty() { "mesh".to_string() } else { u }
        };
        let is_replica = local.is_empty() || host_owner != local;
        if upsert_mesh_row(
            conn,
            &host_owner,
            &noun,
            &name,
            &json,
            &updated_at,
            &updated_by,
            is_replica,
        )? {
            merged += 1;
        }
    }
    Ok(merged)
}

inventory::submit! {
    macro_runtime::ReplicatedRegistration {
        name: "config_rows",
        export: replicate_export,
        merge: replicate_merge,
    }
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
    if n > 0 {
        crate::replicate::notify_write("config_rows");
    }
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
        is_replica: r.get(5)?,
        updated_at: r.get(6)?,
        updated_by: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    const LOCAL: &str = "host-g";

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
        assert_eq!(r.host_owner, "host-g");
        assert!(!r.is_replica);
        // Test-only: parse stored JSON to index into a field. Value is the
        // right tool here — we're asserting on a runtime-shaped tree.
        #[allow(clippy::disallowed_types)]
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
        let err = set(
            &conn, "host-g", "host-b", "service", "jellyfin", "{}", "test",
        )
        .unwrap_err();
        assert!(err.to_string().contains("refusing to write"), "got: {err}");
    }

    #[test]
    fn delete_records_history_and_removes() {
        let conn = test_conn();
        set_local(&conn, "schedule", "host.backup", r#"{"cron":"0 * * * *"}"#).unwrap();
        let removed = delete(&conn, LOCAL, LOCAL, "schedule", "host.backup", "test").unwrap();
        assert!(removed);
        assert!(get(&conn, "schedule", "host.backup").unwrap().is_none());

        let row_id = "schedule:host.backup@host-g";
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

        let all_thor = list(&conn, None, Some("host-g")).unwrap();
        assert_eq!(all_thor.len(), 3);

        let none = list(&conn, None, Some("host-b")).unwrap();
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

    // A remote-owned row arriving via mesh → stored as a replica, origin ts kept.
    fn merge_remote(
        conn: &Connection,
        owner: &str,
        noun: &str,
        name: &str,
        json: &str,
        ts: &str,
    ) -> bool {
        upsert_mesh_row(conn, owner, noun, name, json, ts, "mesh", true).unwrap()
    }

    #[test]
    fn mesh_row_stamps_is_replica_and_preserves_updated_at() {
        let conn = test_conn();
        assert!(merge_remote(
            &conn,
            "host-b",
            "display",
            "target",
            r#"{"refresh":120}"#,
            "2026-07-05T10:00:00Z"
        ));
        let r = list(&conn, Some("display"), None).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].host_owner, "host-b");
        assert!(
            r[0].is_replica,
            "received remote row must be marked is_replica"
        );
        assert_eq!(
            r[0].updated_at, "2026-07-05T10:00:00Z",
            "origin ts preserved"
        );
    }

    #[test]
    fn mesh_row_is_last_write_wins() {
        let conn = test_conn();
        merge_remote(
            &conn,
            "host-b",
            "display",
            "target",
            r#"{"v":1}"#,
            "2026-07-05T10:00:00Z",
        );
        assert!(
            !merge_remote(
                &conn,
                "host-b",
                "display",
                "target",
                r#"{"v":0}"#,
                "2026-07-05T09:00:00Z"
            ),
            "older must not overwrite"
        );
        assert!(
            get(&conn, "display", "target")
                .unwrap()
                .unwrap()
                .json
                .contains("\"v\":1")
        );
        assert!(merge_remote(
            &conn,
            "host-b",
            "display",
            "target",
            r#"{"v":2}"#,
            "2026-07-05T11:00:00Z"
        ));
        assert!(
            get(&conn, "display", "target")
                .unwrap()
                .unwrap()
                .json
                .contains("\"v\":2")
        );
    }

    #[test]
    fn replicate_export_includes_owned_and_replicas() {
        let conn = test_conn();
        set_local(&conn, "display", "target", r#"{"mine":true}"#).unwrap(); // owned
        merge_remote(
            &conn,
            "host-b",
            "graphics",
            "prefs",
            r#"{"theirs":true}"#,
            "2026-07-05T10:00:00Z",
        ); // replica
        let arr = replicate_export(&conn).unwrap();
        // Full export so the fleet gossips everything + restore works.
        assert_eq!(
            arr.as_array().unwrap().len(),
            2,
            "export must include owned rows AND replicas"
        );
    }

    #[test]
    fn restore_reowns_our_own_rows_from_a_peer() {
        // Simulate a reinstalled host that IS "host-g" (LOCAL). A peer pushes a
        // bundle that includes rows owned by host-g (its replica of our config)
        // plus a row owned by another host. Merge must RE-OWN ours (is_replica
        // = 0) and keep the other as a replica.
        let conn = test_conn();
        crate::settings::set(&conn, "host.display_name", LOCAL).unwrap();
        let bundle = serde_json::json!([
            {"host_owner": LOCAL, "noun": "display", "name": "target", "json": "{\"restored\":true}", "updated_at": "2026-07-05T10:00:00Z", "updated_by": "peer"},
            {"host_owner": "host-b", "noun": "graphics", "name": "prefs", "json": "{\"x\":1}", "updated_at": "2026-07-05T10:00:00Z", "updated_by": "peer"}
        ]);
        assert_eq!(replicate_merge(&conn, bundle).unwrap(), 2);
        let mine = get(&conn, "display", "target").unwrap().unwrap();
        assert_eq!(mine.host_owner, LOCAL);
        assert!(!mine.is_replica, "our own rows must be re-owned on restore");
        assert!(
            get(&conn, "graphics", "prefs").unwrap().unwrap().is_replica,
            "other hosts stay replicas"
        );
    }
}
