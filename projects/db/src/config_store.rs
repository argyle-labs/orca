//! Config store — typed, system-owned rows that drive the scheduler, services,
//! backups, NFS watches, chown sweeps, and other runtime configuration.
//!
//! Ownership model: every row is owned by a system id (`host_owner`), and only
//! that system may write it. Other systems hold replicas (`is_replica = 1`) for
//! fast local reads; a write to a replica is rejected and belongs to the owner.
//!
//! A row's `id` is its own uuidv7, so ownership and addressing are separate
//! values: restating who owns a row leaves its id alone.
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

/// Setting holding this host's system id — the value that owns its config rows.
/// Stamped at daemon startup, where the id is loaded.
pub const LOCAL_SYSTEM_ID_SETTING: &str = "host.system_id";

/// This host's system id, or empty when startup has not stamped it yet.
pub fn local_system_id(conn: &Connection) -> String {
    crate::settings::get(conn, LOCAL_SYSTEM_ID_SETTING)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Put config rows this system owns back under its current id, and drop replicas
/// whose owner no longer exists. Returns how many rows changed.
///
/// A row is owned by a system id. Rows written under anything else — an earlier
/// identity, or a display name from before ownership was an id — are locally
/// owned (`is_replica = 0`) yet unwritable, since `set` and `delete` compare
/// `host_owner` against the caller's own id while `get` reads them regardless: a
/// value no writer can reach is the one the host enforces. Replicas of a system
/// the roster no longer carries are dropped; a live owner re-gossips its own.
pub fn reconcile_ownership(conn: &Connection, local_id: &str) -> Result<usize> {
    let mut changed = 0usize;

    let mut stmt = conn.prepare(
        "SELECT id, host_owner, noun, name, json, is_replica, updated_at, updated_by
         FROM config_rows WHERE is_replica = 0 AND host_owner <> ?1",
    )?;
    let strays: Vec<ConfigRow> = stmt
        .query_map(params![local_id], row_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    for row in strays {
        // A row already held under this id is authoritative, so a stray gives way.
        if get_owned(conn, local_id, &row.noun, &row.name)?.is_some() {
            conn.execute("DELETE FROM config_rows WHERE id = ?1", params![row.id])?;
        } else {
            conn.execute(
                "UPDATE config_rows SET host_owner = ?2 WHERE id = ?1",
                params![row.id, local_id],
            )?;
        }
        changed += 1;
    }

    changed += conn.execute(
        "DELETE FROM config_rows
          WHERE is_replica = 1 AND host_owner <> ?1
            AND host_owner NOT IN (SELECT peer_id FROM pod_peers)",
        params![local_id],
    )?;

    if changed > 0 {
        crate::replicate::notify_write("config_rows");
    }
    Ok(changed)
}

/// The row for one owner, addressed by the natural key the schema enforces
/// (`UNIQUE (noun, name, host_owner)`).
pub fn get_owned(
    conn: &Connection,
    host_owner: &str,
    noun: &str,
    name: &str,
) -> Result<Option<ConfigRow>> {
    let r = conn
        .query_row(
            "SELECT id, host_owner, noun, name, json, is_replica, updated_at, updated_by
             FROM config_rows WHERE noun = ?1 AND name = ?2 AND host_owner = ?3",
            params![noun, name, host_owner],
            row_from,
        )
        .optional()?;
    Ok(r)
}

/// One row for `(noun, name)`, whoever owns it.
///
/// `UNIQUE (noun, name, host_owner)` allows one row per owner, so several can
/// match. The row this system owns wins over a replica, newest first, so every
/// caller on a host resolves the same one.
pub fn get(conn: &Connection, noun: &str, name: &str) -> Result<Option<ConfigRow>> {
    let r = conn
        .query_row(
            "SELECT id, host_owner, noun, name, json, is_replica, updated_at, updated_by
             FROM config_rows WHERE noun = ?1 AND name = ?2
             ORDER BY is_replica ASC, updated_at DESC, id ASC
             LIMIT 1",
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

    let prior = get_owned(conn, host_owner, noun, name)?;
    if let Some(p) = &prior {
        record_history(conn, &p.id, &p.json, updated_by)?;
    }
    // A row keeps the id it already has; a new one is minted here so the
    // returned id is known without a second read.
    let row_id = prior
        .as_ref()
        .map_or_else(|| utils::id::new().to_string(), |p| p.id.clone());

    conn.execute(
        "INSERT INTO config_rows (id, host_owner, noun, name, json, is_replica, updated_at, updated_by, uuidv7)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), ?6, ?1)
         ON CONFLICT(noun, name, host_owner) DO UPDATE SET
             json       = excluded.json,
             is_replica = 0,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
             updated_by = excluded.updated_by",
        params![row_id, host_owner, noun, name, payload_json, updated_by],
    )?;
    // Re-creating a previously-deleted key: supersede any delete op so the
    // resurrected row is not removed again on the next sync (no-op otherwise).
    crate::replication_ops::note_write(
        conn,
        "config_rows",
        "id",
        &row_id,
        utils::time::now_millis_since_epoch(),
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
    // Incoming passenger v7 id. Empty when the sending peer predates the uuidv7
    // migration; in that case we leave the local value untouched (and the insert
    // trigger mints one for a genuinely new row).
    uuidv7: &str,
) -> Result<bool> {
    serde_json::from_str::<serde::de::IgnoredAny>(payload_json)
        .with_context(|| format!("mesh payload for {noun}/{name} is not valid JSON"))?;
    // An id the owner sent wins; otherwise the row keeps the one it has, and a
    // genuinely new row gets a fresh one.
    let row_id = if uuidv7.is_empty() {
        get_owned(conn, host_owner, noun, name)?
            .map_or_else(|| utils::id::new().to_string(), |p| p.id)
    } else {
        uuidv7.to_string()
    };
    let rep = i64::from(is_replica);
    // Insert carries the incoming uuidv7 (NULL when empty → the AFTER INSERT
    // trigger mints one for a brand-new row). The LWW gate governs the mutable
    // payload columns only.
    let n = conn.execute(
        "INSERT INTO config_rows (id, host_owner, noun, name, json, is_replica, updated_at, updated_by, uuidv7)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?1)
         ON CONFLICT(noun, name, host_owner) DO UPDATE SET
             json       = excluded.json,
             is_replica = excluded.is_replica,
             updated_at = excluded.updated_at,
             updated_by = excluded.updated_by
         WHERE excluded.updated_at > config_rows.updated_at",
        params![row_id, host_owner, noun, name, payload_json, rep, updated_at, updated_by],
    )?;
    // Converge the id independently of the payload LWW gate. All peers adopt
    // MIN(local, incoming): a deterministic, order-independent selection so the
    // fleet settles on ONE id per logical row even where each minted its own
    // (during backfill `updated_at` never changes and LWW is a no-op). The natural
    // key addresses the row here, since the id is the value being converged.
    let mut converged = 0;
    if !uuidv7.is_empty()
        && let Some(local) = get_owned(conn, host_owner, noun, name)?
        && uuidv7 < local.id.as_str()
    {
        // History is addressed by the row's id, so it moves with it.
        conn.execute(
            "UPDATE config_history SET row_id = ?2 WHERE row_id = ?1",
            params![local.id, uuidv7],
        )?;
        converged = conn.execute(
            "UPDATE config_rows SET id = ?2, uuidv7 = ?2 WHERE id = ?1",
            params![local.id, uuidv7],
        )?;
    }
    Ok(n > 0 || converged > 0)
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
        "SELECT host_owner, noun, name, json, updated_at, updated_by, uuidv7
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
                // Passenger v7 id. Carried so peers converge on ONE value per
                // logical row via MIN-selection in replicate_merge.
                "uuidv7":     r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            }))
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(serde_json::Value::Array(rows))
}

#[allow(clippy::disallowed_types)]
fn replicate_merge(conn: &Connection, rows: serde_json::Value) -> Result<usize> {
    // Ownership decides is_replica on the RECEIVER: a row whose host_owner is
    // THIS system is applied as OWNED (is_replica=0) — that's the restore path,
    // where a reinstalled node re-owns its config pulled from a peer. Everything
    // else lands as a replica. An unresolvable local id treats all as replicas
    // (safe; restore needs the id stamped).
    let local = local_system_id(conn);
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
        let uuidv7 = field("uuidv7");
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
            &uuidv7,
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
    let Some(row) = get_owned(conn, host_owner, noun, name)? else {
        return Ok(false);
    };
    let row_id = row.id;
    record_history(conn, &row_id, &row.json, deleted_by)?;
    let n = conn.execute("DELETE FROM config_rows WHERE id = ?1", params![row_id])?;
    if n > 0 {
        // Command-log the removal so it replicates and cannot be resurrected by
        // a peer that still holds the row (see replication_ops).
        crate::replication_ops::note_delete(
            conn,
            "config_rows",
            "id",
            &row_id,
            utils::time::now_millis_since_epoch(),
        )?;
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

/// The row with this id.
pub fn get_by_id(conn: &Connection, row_id: &str) -> Result<Option<ConfigRow>> {
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
        let row_id = get(&conn, "schedule", "host.backup").unwrap().unwrap().id;
        let removed = delete(&conn, LOCAL, LOCAL, "schedule", "host.backup", "test").unwrap();
        assert!(removed);
        assert!(get(&conn, "schedule", "host.backup").unwrap().is_none());

        // History is addressed by the row's id.
        let h = history(&conn, &row_id).unwrap();
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn delete_records_a_replication_op() {
        let conn = test_conn();
        set_local(&conn, "schedule", "host.backup", r#"{"cron":"@daily"}"#).unwrap();
        let row_id = get(&conn, "schedule", "host.backup").unwrap().unwrap().id;
        delete(&conn, LOCAL, LOCAL, "schedule", "host.backup", "test").unwrap();
        let op: String = conn
            .query_row(
                "SELECT op FROM replication_ops WHERE entity='config_rows' AND key_val=?1",
                params![row_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(op, "delete", "delete must leave a durable command-log op");
    }

    #[test]
    fn a_recreated_row_survives_the_earlier_tombstone() {
        // Each row instance carries its own id, so a re-created row and the
        // tombstone for the deleted one address different rows: the pending delete
        // sweeps what it named and leaves the new row standing.
        let conn = test_conn();
        set_local(&conn, "service", "plex", r#"{"v":1}"#).unwrap();
        let gone = get(&conn, "service", "plex").unwrap().unwrap().id;
        delete(&conn, LOCAL, LOCAL, "service", "plex", "test").unwrap();

        set_local(&conn, "service", "plex", r#"{"v":2}"#).unwrap();
        let live = get(&conn, "service", "plex").unwrap().unwrap().id;
        assert_ne!(live, gone, "a new row is a new id");

        let op: String = conn
            .query_row(
                "SELECT op FROM replication_ops WHERE entity='config_rows' AND key_val=?1",
                params![gone],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(op, "delete", "the tombstone names the row that was deleted");

        crate::replication_ops::apply_pending_deletes(&conn).unwrap();
        let row = get(&conn, "service", "plex").unwrap().unwrap();
        assert_eq!(row.id, live, "the re-created row survives");
        assert!(row.json.contains("\"v\":2"));
    }

    #[test]
    fn the_id_is_a_uuidv7_not_a_rebuilt_key() {
        let conn = test_conn();
        set_local(&conn, "host_status", "retention_max_mb", "25").unwrap();
        let row = get(&conn, "host_status", "retention_max_mb")
            .unwrap()
            .unwrap();
        assert!(
            utils::id::is_uuidv7(&row.id),
            "id must be a uuidv7, got {}",
            row.id
        );
        assert!(
            get_by_id(&conn, &row.id).unwrap().is_some(),
            "addressable by id"
        );
    }

    #[test]
    fn an_id_outlives_a_restatement_of_its_owner() {
        // The id stops moving when ownership is restated, which is what let a
        // machine_id owner leave a row no writer could reach.
        let conn = test_conn();
        set_local(&conn, "host_status", "retention_days", "2").unwrap();
        let before = get(&conn, "host_status", "retention_days")
            .unwrap()
            .unwrap()
            .id;
        reconcile_ownership(&conn, LOCAL).unwrap();
        set_local(&conn, "host_status", "retention_days", "3").unwrap();
        let after = get(&conn, "host_status", "retention_days")
            .unwrap()
            .unwrap();
        assert_eq!(after.id, before);
        assert_eq!(after.json, "3");
    }

    /// Insert a locally-owned row under some other owner, bypassing `set`'s owner
    /// check, to reproduce what an earlier identity left behind.
    fn stray_owned_row(conn: &Connection, owner: &str, noun: &str, name: &str, json: &str) {
        conn.execute(
            "INSERT INTO config_rows (id, host_owner, noun, name, json, is_replica, updated_at, updated_by, uuidv7)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, '2026-07-26T20:51:23Z', 'system.retention.set', ?1)",
            params![utils::id::new().to_string(), owner, noun, name, json],
        )
        .unwrap();
    }

    #[test]
    fn a_row_owned_by_a_dead_identity_comes_back_under_this_system() {
        // What mint held: rows owned by a machine_id absent from the roster,
        // locally owned yet refused by `set`.
        let conn = test_conn();
        stray_owned_row(
            &conn,
            "019e710a-b21c-79e0-bb76-2563af169c1c",
            "host_status",
            "retention_max_mb",
            "25",
        );
        assert_eq!(reconcile_ownership(&conn, LOCAL).unwrap(), 1);

        let row = get(&conn, "host_status", "retention_max_mb")
            .unwrap()
            .unwrap();
        assert_eq!(row.host_owner, LOCAL);
        assert_eq!(row.json, "25", "the value carries over");
        // Writable again through the ordinary path.
        set_local(&conn, "host_status", "retention_max_mb", "50").unwrap();
        assert_eq!(
            get(&conn, "host_status", "retention_max_mb")
                .unwrap()
                .unwrap()
                .json,
            "50"
        );
    }

    #[test]
    fn a_stray_row_gives_way_to_the_one_this_system_already_holds() {
        let conn = test_conn();
        set_local(&conn, "host_status", "retention_days", "2").unwrap();
        stray_owned_row(&conn, "dead-id", "host_status", "retention_days", "0.5");

        assert_eq!(reconcile_ownership(&conn, LOCAL).unwrap(), 1);
        let rows = list(&conn, Some("host_status"), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].host_owner, LOCAL);
        assert_eq!(rows[0].json, "2");
    }

    #[test]
    fn a_replica_of_a_system_still_in_the_roster_is_kept() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO pod_peers (peer_id, peer_hostname, peer_port,
                                    ca_cert_pem, first_seen_at, last_seen_at)
             VALUES ('peer-b', 'host-b', 12002, '', 0, 0)",
            [],
        )
        .unwrap();
        merge_remote(
            &conn,
            "peer-b",
            "graphics",
            "prefs",
            "{}",
            "2026-07-05T10:00:00Z",
        );
        merge_remote(
            &conn,
            "gone-id",
            "display",
            "target",
            "{}",
            "2026-07-05T10:00:00Z",
        );

        // The absent system's replica goes; a live system's stays for it to own.
        assert_eq!(reconcile_ownership(&conn, LOCAL).unwrap(), 1);
        assert!(get(&conn, "graphics", "prefs").unwrap().is_some());
        assert!(get(&conn, "display", "target").unwrap().is_none());
    }

    #[test]
    fn reconcile_is_idempotent() {
        let conn = test_conn();
        stray_owned_row(&conn, "dead-id", "host_status", "retention_days", "0.5");
        assert_eq!(reconcile_ownership(&conn, LOCAL).unwrap(), 1);
        assert_eq!(reconcile_ownership(&conn, LOCAL).unwrap(), 0);
    }

    #[test]
    fn get_prefers_the_row_this_system_owns_over_a_replica() {
        // Two rows for one (noun, name) — one owned, one a replica. Without an
        // order the winner is whichever the query happens to return first.
        let conn = test_conn();
        merge_remote(
            &conn,
            "peer-b",
            "host_status",
            "retention_days",
            "9",
            "2027-01-01T00:00:00Z",
        );
        set_local(&conn, "host_status", "retention_days", "2").unwrap();
        let row = get(&conn, "host_status", "retention_days")
            .unwrap()
            .unwrap();
        assert_eq!(row.host_owner, LOCAL, "this system's own row decides");
        assert_eq!(row.json, "2");
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
        // Empty uuidv7 → the insert trigger mints one locally (these tests don't
        // exercise convergence; see uuidv7_converges_to_min_across_peers).
        upsert_mesh_row(conn, owner, noun, name, json, ts, "mesh", true, "").unwrap()
    }

    #[test]
    fn uuidv7_converges_to_min_across_peers() {
        // Two peers independently hold the same logical row with DIFFERENT
        // backfilled uuidv7s (the pre-existing-row case, where updated_at is
        // unchanged so payload LWW is a no-op). After exchanging exports in EITHER
        // order, both must settle on MIN — a deterministic, order-independent id.
        let conn = test_conn();
        let ts = "2026-07-05T10:00:00Z";
        // Local row with the LARGER id; a smaller id arrives from a peer.
        upsert_mesh_row(
            &conn,
            "host-b",
            "display",
            "t",
            r#"{"v":1}"#,
            ts,
            "mesh",
            true,
            "ffff",
        )
        .unwrap();
        assert!(
            upsert_mesh_row(
                &conn,
                "host-b",
                "display",
                "t",
                r#"{"v":1}"#,
                ts,
                "mesh",
                true,
                "0000"
            )
            .unwrap(),
            "adopting a smaller incoming id is a change"
        );
        let got = get_owned(&conn, "host-b", "display", "t")
            .unwrap()
            .unwrap()
            .id;
        assert_eq!(got, "0000", "peer converges DOWN to the smaller id");
        // A LARGER incoming id must NOT displace the settled minimum, regardless
        // of arrival order → convergence is stable.
        assert!(
            !upsert_mesh_row(
                &conn,
                "host-b",
                "display",
                "t",
                r#"{"v":1}"#,
                ts,
                "mesh",
                true,
                "eeee"
            )
            .unwrap(),
            "a larger id must not change the converged minimum"
        );
        let still = get_owned(&conn, "host-b", "display", "t")
            .unwrap()
            .unwrap()
            .id;
        assert_eq!(still, "0000", "minimum is stable");
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
        crate::settings::set(&conn, LOCAL_SYSTEM_ID_SETTING, LOCAL).unwrap();
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
