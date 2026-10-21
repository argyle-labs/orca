//! Command-log delete for LWW mesh replication.
//!
//! The mesh replicates by **state gossip**: each entity exports its full row
//! set and peers merge last-write-wins (see [`crate::replicate`]). That model
//! has no way to express *absence* — a hard `DELETE` simply makes a row stop
//! appearing in our export, so any peer that still holds it re-gossips it back
//! and the row **resurrects**. That was the rc.26 bug.
//!
//! This module adds the missing piece: an **operation log**. A delete is not
//! just a local `DELETE`; it also appends a durable `delete` op that itself
//! replicates. Peers replay the op and physically remove their own copy, and
//! our own merge re-applies pending deletes so a stale peer row cannot
//! resurrect within a single sync. The domain row is **genuinely gone** — no
//! `deleted` column, no read-time filtering, no lingering secrets.
//!
//! Ordering is a single uniform millisecond clock ([`utils::time::now_millis_since_epoch`]),
//! so delete-vs-recreate races resolve without comparing each entity's
//! heterogeneous `lww` column: the op-log is the ordering authority. A
//! re-create supersedes a prior delete by writing a newer `upsert` op (which
//! replicates and out-votes the delete), so "delete then re-add the same key"
//! works across the fleet.
//!
//! The log is **self-describing** — each op carries the entity's table name,
//! its natural-key column, and the key value — so the receiver applies deletes
//! generically without any per-entity code. It is compacted by [`reap`] once
//! ops age past the anti-entropy horizon: a peer offline longer than that
//! re-bootstraps from a full snapshot anyway, so "delete means delete" holds
//! for the data *and*, eventually, for the death record itself.

// The op-log rides the same heterogeneous-JSON bundle boundary as the rest of
// the replication registry (see replicate.rs) — Value is the right tool for the
// export/merge wire shape.
#![allow(clippy::disallowed_types)]

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

/// Replicated-entity name for the op-log itself. Merged before every domain
/// entity (see [`crate::replicate::merge_bundle`]) so pending deletes are known
/// before rows land.
pub const ENTITY: &str = "replication_ops";

const LEGACY_ENTITIES: &[(&str, &str)] = &[
    ("pod_peers", "mesh_peers"),
    ("pod_peer_addresses", "mesh_peer_addresses"),
    ("pod_trust", "mesh_trust"),
    ("pod_self", "mesh_self"),
    ("pod_discovery", "mesh_discovery"),
    ("pod_pending_offers", "mesh_pending_offers"),
];

const LEGACY_KEY_COLS: &[(&str, &str, &str)] = &[
    ("mesh_self", "pod_id", "mesh_id"),
    ("mesh_pending_offers", "pod_id", "mesh_id"),
];

fn canonical_key_col<'a>(entity: &str, key_col: &'a str) -> &'a str {
    LEGACY_KEY_COLS
        .iter()
        .find(|(e, old, _)| *e == entity && *old == key_col)
        .map_or(key_col, |(_, _, new)| new)
}

fn canonical_entity(entity: &str) -> &str {
    LEGACY_ENTITIES
        .iter()
        .find(|(old, _)| *old == entity)
        .map_or(entity, |(_, new)| new)
}

/// A `delete` op survives at least this long so any peer that was briefly
/// offline still replays it. 30 days: a host down longer than that re-bootstraps
/// from a full snapshot, which already reflects the deletion.
pub const DEFAULT_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// True iff `ident` is a bare SQL identifier (`[A-Za-z_][A-Za-z0-9_]*`). The
/// entity/key-column names come from trusted registration code, never user
/// input, but [`apply_pending_deletes`] interpolates them into SQL (identifiers
/// can't be bound), so we validate defensively before ever building a statement.
fn is_ident(ident: &str) -> bool {
    let mut chars = ident.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn op_target(
    entity: &str,
    key_col: &str,
) -> Result<Option<&'static macro_runtime::ReplicatedRegistration>, &'static str> {
    if entity == crate::mesh::peerdb::PEER_TOMBSTONE_ENTITY {
        return if key_col == crate::mesh::peerdb::PEER_TOMBSTONE_KEY_COL {
            Ok(None)
        } else {
            Err("key column is not a declared key")
        };
    }
    let reg = crate::replicate::registrations()
        .into_iter()
        .find(|r| r.name == entity)
        .ok_or("entity is not replicated")?;
    if reg.keys.contains(&key_col) {
        Ok(Some(reg))
    } else {
        Err("key column is not a declared key")
    }
}

const ROW_DELETE_REFUSED: &str = "delete not permitted for this row";

fn peer_delete_allowed(
    conn: &Connection,
    entity: &str,
    key_col: &str,
    key_val: &str,
) -> Result<(), &'static str> {
    let Some(reg) = op_target(entity, key_col)? else {
        return Ok(());
    };
    match reg.peer_delete {
        Some(allowed) if allowed(conn, key_val) => Ok(()),
        Some(_) => Err(ROW_DELETE_REFUSED),
        None => Err("entity is not peer-deletable"),
    }
}

fn peer_op_allowed(
    conn: &Connection,
    entity: &str,
    key_col: &str,
    key_val: &str,
    op: &str,
) -> Result<(), &'static str> {
    if entity == crate::mesh::peerdb::PEER_TOMBSTONE_ENTITY {
        op_target(entity, key_col)?;
        if op != "delete" {
            return Err("peers may not clear peer tombstones");
        }
        if key_val == crate::config_store::local_system_id(conn) {
            return Err("tombstone targets the local host");
        }
        if macro_runtime::replicate_table::current_merge_peer().as_deref() != Some(key_val) {
            return Err("peer tombstone not sent by the retiring peer");
        }
        return Ok(());
    }
    peer_delete_allowed(conn, entity, key_col, key_val)
}

/// This host's name, used as an op's `origin`. Best-effort — empty if the host
/// hasn't been named yet (ops still work; origin is informational + a merge
/// tiebreak input only via op_id).
fn origin(conn: &Connection) -> String {
    crate::settings::get(conn, "host.display_name")
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Record that `(entity, key_val)` was **deleted** at `stamp_ms`. The caller has
/// already hard-deleted the domain row; this makes the removal durable and
/// replicable. Last-write-wins on `(stamp_ms, op_id)` against any existing op
/// for the same key, so a delete overwrites a stale `upsert` and vice-versa.
pub fn note_delete(
    conn: &Connection,
    entity: &str,
    key_col: &str,
    key_val: &str,
    stamp_ms: i64,
) -> Result<()> {
    put(conn, entity, key_col, key_val, "delete", stamp_ms)
}

/// Record a **write** (create/update) of `(entity, key_val)`. This only matters
/// as a *resurrection*: if a `delete` op exists for the key, a newer `upsert`
/// op supersedes it so the re-created row is not deleted again on the next sync
/// (locally or on any peer). When no delete op exists this is a cheap no-op —
/// live rows do not need an op-log entry, keeping the log sparse.
pub fn note_write(
    conn: &Connection,
    entity: &str,
    key_col: &str,
    key_val: &str,
    stamp_ms: i64,
) -> Result<()> {
    // Fast path: only supersede when a delete tombstone is actually present.
    let has_delete: bool = conn
        .query_row(
            "SELECT 1 FROM replication_ops WHERE entity = ?1 AND key_val = ?2 AND op = 'delete'",
            params![entity, key_val],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_delete {
        return Ok(());
    }
    put(conn, entity, key_col, key_val, "upsert", stamp_ms)
}

/// True iff an *active* `delete` tombstone exists for `(entity, key_val)`: an
/// op with `op = 'delete'` whose `stamp_ms` is within the TTL window
/// (`stamp_ms >= now_ms - ttl_ms`). A superseding `upsert` op flips `op` away
/// from `delete`, so this returns false once a key is re-created. Callers use
/// it to suppress *resurrection* on write paths that bypass the bundle merge
/// (e.g. roster ingest, which upserts peers pulled from a live peer's roster
/// rather than from a replicated snapshot).
pub fn is_deleted(
    conn: &Connection,
    entity: &str,
    key_val: &str,
    now_ms: i64,
    ttl_ms: i64,
) -> Result<bool> {
    let cutoff = now_ms - ttl_ms;
    let found = conn
        .query_row(
            "SELECT 1 FROM replication_ops
              WHERE entity = ?1 AND key_val = ?2 AND op = 'delete' AND stamp_ms >= ?3",
            params![entity, key_val, cutoff],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(found)
}

/// Upsert one op, LWW on `(stamp_ms, op_id)` keyed by `(entity, key_val)`.
fn put(
    conn: &Connection,
    entity: &str,
    key_col: &str,
    key_val: &str,
    op: &str,
    stamp_ms: i64,
) -> Result<()> {
    let op_id = utils::id::new();
    let origin = origin(conn);
    conn.execute(
        "INSERT INTO replication_ops (op_id, entity, key_col, key_val, op, origin, stamp_ms, local_origin)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)
         ON CONFLICT(entity, key_val) DO UPDATE SET
             op_id    = excluded.op_id,
             key_col  = excluded.key_col,
             op       = excluded.op,
             origin   = excluded.origin,
             stamp_ms = excluded.stamp_ms,
             local_origin = 1
         WHERE excluded.stamp_ms > replication_ops.stamp_ms
            OR (excluded.stamp_ms = replication_ops.stamp_ms
                AND excluded.op_id > replication_ops.op_id)",
        params![op_id, entity, key_col, key_val, op, origin, stamp_ms],
    )?;
    crate::replicate::notify_write(ENTITY);
    Ok(())
}

/// Physically remove every domain row that the op-log says is deleted. Called
/// after a bundle merge (see [`crate::replicate::merge_bundle`]): the op-log has
/// already merged, so `op = 'delete'` reflects the fleet-latest transition for
/// each key (a newer `upsert` op would have flipped it). This is what stops a
/// stale peer row — freshly re-imported by a domain merge in the same tick —
/// from resurrecting. Idempotent and generic: no per-entity code.
pub fn apply_pending_deletes(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT entity, key_col, key_val, local_origin FROM replication_ops WHERE op = 'delete'",
    )?;
    let pending: Vec<(String, String, String, bool)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut removed = 0usize;
    for (entity, key_col, key_val, local_origin) in pending {
        let check = if local_origin {
            op_target(&entity, &key_col).map(drop)
        } else {
            peer_delete_allowed(conn, &entity, &key_col, &key_val)
        };
        if let Err(reason) = check {
            tracing::debug!("[replication_ops] not applying delete {entity}.{key_col}: {reason}");
            if reason == ROW_DELETE_REFUSED {
                conn.execute(
                    "UPDATE replication_ops SET op = 'refused'
                      WHERE entity = ?1 AND key_val = ?2 AND op = 'delete' AND local_origin = 0",
                    params![entity, key_val],
                )?;
            }
            continue;
        }
        // Defensive: entity/key_col originate from registration code, but they
        // are interpolated as identifiers — skip anything that isn't a bare
        // identifier rather than build unsafe SQL.
        if !is_ident(&entity) || !is_ident(&key_col) {
            tracing::warn!(
                "[replication_ops] skipping delete with non-identifier entity/col: {entity}/{key_col}"
            );
            continue;
        }
        let sql = format!("DELETE FROM {entity} WHERE {key_col} = ?1");
        match conn.execute(&sql, params![key_val]) {
            Ok(n) => removed += n,
            Err(e) => tracing::warn!(
                "[replication_ops] apply delete {entity}.{key_col}={key_val} failed: {e}"
            ),
        }
    }
    Ok(removed)
}

/// Drop ops whose `stamp_ms` is older than `now_ms - ttl_ms`. Compaction: past
/// the horizon every online peer has applied the delete and a longer-offline
/// peer re-bootstraps from a snapshot, so the op is no longer load-bearing.
pub fn reap(conn: &Connection, now_ms: i64, ttl_ms: i64) -> Result<usize> {
    let cutoff = now_ms - ttl_ms;
    let n = conn.execute(
        "DELETE FROM replication_ops WHERE stamp_ms < ?1",
        params![cutoff],
    )?;
    // Reaping changes this entity's rows, so the memoized content-root is now
    // stale. Invalidate it (local GC only — do NOT notify_write / push: every
    // host reaps the same horizon independently). Without this the divergence
    // check could read a stale root and mis-report in_sync.
    if n > 0 {
        crate::replicate::invalidate_root(ENTITY);
    }
    Ok(n)
}

// ── Mesh replication of the op-log itself ────────────────────────────────────
//
// The op-log rides the SAME bundle transport as every other entity: it exports
// all ops and merges them LWW on (stamp_ms, op_id). That is how a delete
// authored on host A reaches host B — B merges the op, then apply_pending_deletes
// removes B's copy of the row. Registered via inventory like config_rows.

// The bundle boundary is heterogeneous JSON across entities (see replicate.rs).
#[allow(clippy::disallowed_types)]
fn replicate_export(conn: &Connection) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT op_id, entity, key_col, key_val, op, origin, stamp_ms
           FROM replication_ops WHERE op != 'refused' ORDER BY op_id",
    )?;
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(serde_json::json!({
                "op_id":    r.get::<_, String>(0)?,
                "entity":   r.get::<_, String>(1)?,
                "key_col":  r.get::<_, String>(2)?,
                "key_val":  r.get::<_, String>(3)?,
                "op":       r.get::<_, String>(4)?,
                "origin":   r.get::<_, String>(5)?,
                "stamp_ms": r.get::<_, i64>(6)?,
            }))
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(Value::Array(rows))
}

#[allow(clippy::disallowed_types)]
fn replicate_merge(conn: &Connection, rows: Value) -> Result<usize> {
    let arr = rows.as_array().cloned().unwrap_or_default();
    let mut merged = 0usize;
    for row in arr {
        let s = |k: &str| {
            row.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let op_id = s("op_id");
        let entity = canonical_entity(&s("entity")).to_string();
        let key_col = canonical_key_col(&entity, &s("key_col")).to_string();
        let key_val = s("key_val");
        let mut op = s("op");
        let origin = s("origin");
        let stamp_ms = row.get("stamp_ms").and_then(|v| v.as_i64()).unwrap_or(0);
        if op_id.is_empty() || entity.is_empty() || key_val.is_empty() || op.is_empty() {
            continue;
        }
        if op == "refused" {
            continue;
        }
        if let Err(reason) = macro_runtime::replicate_table::check_incoming_millis(stamp_ms)
            .map(drop)
            .and_then(|()| peer_op_allowed(conn, &entity, &key_col, &key_val, &op))
        {
            macro_runtime::replicate_table::audit_reject(ENTITY, &(&entity, &key_col), reason);
            if reason != ROW_DELETE_REFUSED || op != "delete" {
                continue;
            }
            op = "refused".to_string();
        }
        let n = conn.execute(
            "INSERT INTO replication_ops (op_id, entity, key_col, key_val, op, origin, stamp_ms, local_origin)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)
             ON CONFLICT(entity, key_val) DO UPDATE SET
                 op_id    = excluded.op_id,
                 key_col  = excluded.key_col,
                 op       = excluded.op,
                 origin   = excluded.origin,
                 stamp_ms = excluded.stamp_ms,
                 local_origin = 0
             WHERE excluded.stamp_ms > replication_ops.stamp_ms
                OR (excluded.stamp_ms = replication_ops.stamp_ms
                    AND excluded.op_id > replication_ops.op_id)",
            params![op_id, entity, key_col, key_val, op, origin, stamp_ms],
        )?;
        if op != "refused" {
            merged += n;
        }
    }
    Ok(merged)
}

inventory::submit! {
    macro_runtime::ReplicatedRegistration {
        name: ENTITY,
        export: replicate_export,
        merge: replicate_merge,
        keys: &[],
        peer_delete: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    fn insert_op(
        conn: &Connection,
        op_id: &str,
        entity: &str,
        key_val: &str,
        op: &str,
        stamp_ms: i64,
    ) {
        conn.execute(
            "INSERT INTO replication_ops (op_id, entity, key_col, key_val, op, origin, stamp_ms, local_origin)
             VALUES (?1, ?2, 'peer_id', ?3, ?4, 'o', ?5, 0)",
            params![op_id, entity, key_val, op, stamp_ms],
        )
        .unwrap();
    }

    fn ops(conn: &Connection) -> Vec<(String, String, String, i64)> {
        let mut stmt = conn
            .prepare("SELECT entity, key_val, op, stamp_ms FROM replication_ops ORDER BY entity, key_val")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn migration_renames_pod_entities_keeping_the_newest_op() {
        let conn = test_conn();
        insert_op(&conn, "a", "pod_peers", "p1", "delete", 100);
        insert_op(&conn, "b", "pod_peers", "p2", "delete", 300);
        insert_op(&conn, "c", "mesh_peers", "p2", "upsert", 200);
        insert_op(&conn, "d", "pod_peer_addresses", "p3", "delete", 100);
        insert_op(&conn, "e", "mesh_peers", "p4", "upsert", 500);
        insert_op(&conn, "f", "pod_peers", "p4", "delete", 400);
        insert_op(&conn, "g", "config_rows", "k", "delete", 100);
        conn.execute_batch(include_str!(
            "../migrations/20261020000000__replication_ops_mesh_entities.up.sql"
        ))
        .unwrap();
        assert_eq!(
            ops(&conn),
            vec![
                ("config_rows".into(), "k".into(), "delete".into(), 100),
                (
                    "mesh_peer_addresses".into(),
                    "p3".into(),
                    "delete".into(),
                    100
                ),
                ("mesh_peers".into(), "p1".into(), "delete".into(), 100),
                ("mesh_peers".into(), "p2".into(), "delete".into(), 300),
                ("mesh_peers".into(), "p4".into(), "upsert".into(), 500),
            ]
        );
    }

    #[test]
    fn ops_from_older_peers_land_under_the_mesh_entity() {
        assert_eq!(canonical_entity("pod_peers"), "mesh_peers");
        assert_eq!(canonical_entity("pod_discovery"), "mesh_discovery");
        assert_eq!(canonical_entity("mesh_peers"), "mesh_peers");
        assert_eq!(canonical_entity("pod_unknown"), "pod_unknown");
        let conn = test_conn();
        let n = macro_runtime::replicate_table::with_merge_peer("p9", || {
            replicate_merge(
                &conn,
                serde_json::json!([{
                    "op_id": "z", "entity": "pod_peers", "key_col": "peer_id",
                    "key_val": "p9", "op": "delete", "origin": "o",
                    "stamp_ms": utils::time::now().unix_millis(),
                }]),
            )
        })
        .unwrap();
        let rows = ops(&conn);
        assert_eq!(n, 1, "{rows:?}");
        assert!(
            rows.iter()
                .any(|r| r.0 == "mesh_peers" && r.1 == "p9" && r.2 == "delete"),
            "{rows:?}"
        );
        assert!(rows.iter().all(|r| r.0 != "pod_peers"), "{rows:?}");
    }

    #[test]
    fn legacy_pod_id_key_col_maps_to_mesh_id() {
        assert_eq!(canonical_key_col("mesh_self", "pod_id"), "mesh_id");
        assert_eq!(
            canonical_key_col("mesh_pending_offers", "pod_id"),
            "mesh_id"
        );
        assert_eq!(canonical_key_col("mesh_peers", "pod_id"), "pod_id");
        let conn = test_conn();
        conn.execute(
            "INSERT INTO replication_ops (op_id, entity, key_col, key_val, op, origin, stamp_ms, local_origin)
             VALUES ('k', 'pod_self', 'pod_id', 'm1', 'delete', 'o', 100, 0)",
            [],
        )
        .unwrap();
        conn.execute_batch(include_str!(
            "../migrations/20261020000000__replication_ops_mesh_entities.up.sql"
        ))
        .unwrap();
        conn.execute_batch(include_str!(
            "../migrations/20261021000000__replication_ops_mesh_key_col.up.sql"
        ))
        .unwrap();
        let migrated: (String, String) = conn
            .query_row(
                "SELECT entity, key_col FROM replication_ops WHERE key_val = 'm1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(migrated, ("mesh_self".into(), "mesh_id".into()));
    }

    #[test]
    fn legacy_bundle_delete_removes_the_mesh_peers_row() {
        let conn = test_conn();
        insert_op(&conn, "old", "pod_peers", "p7", "upsert", 100);
        conn.execute_batch(include_str!(
            "../migrations/20261020000000__replication_ops_mesh_entities.up.sql"
        ))
        .unwrap();
        conn.execute(
            "INSERT INTO mesh_peers (peer_id, peer_hostname, ca_cert_pem, first_seen_at, last_seen_at)
             VALUES ('p7', 'h7', 'pem', 1, 1)",
            [],
        )
        .unwrap();
        macro_runtime::replicate_table::with_merge_peer("p7", || {
            replicate_merge(
                &conn,
                serde_json::json!([{
                    "op_id": "z", "entity": "pod_peers", "key_col": "peer_id",
                    "key_val": "p7", "op": "delete", "origin": "o",
                    "stamp_ms": utils::time::now().unix_millis(),
                }]),
            )
        })
        .unwrap();
        apply_pending_deletes(&conn).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mesh_peers WHERE peer_id = 'p7'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn note_delete_then_apply_removes_row() {
        let conn = test_conn();
        note_delete(&conn, "replica_fixture", "name_lower", "scott", 1000).unwrap();
        conn.execute(
            "INSERT INTO replica_fixture (id, name, name_lower, payload, updated_at)
             VALUES ('u1','scott','scott','p','2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        let removed = apply_pending_deletes(&conn).unwrap();
        assert_eq!(removed, 1, "the deleted row must be physically removed");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM replica_fixture", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn note_write_supersedes_a_delete_so_recreate_survives() {
        let conn = test_conn();
        note_delete(&conn, "users", "username_lower", "scott", 1000).unwrap();
        // Re-create at a later stamp → upsert op supersedes the delete.
        note_write(&conn, "users", "username_lower", "scott", 2000).unwrap();
        let op: String = conn
            .query_row(
                "SELECT op FROM replication_ops WHERE entity='users' AND key_val='scott'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(op, "upsert", "re-create must flip the tombstone to upsert");
        // A live row for the key is now NOT pending deletion.
        crate::testing::insert_user(&conn, "u2", "scott", "h", "admin", "t");
        apply_pending_deletes(&conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "resurrected row must survive apply_pending_deletes");
    }

    #[test]
    fn note_write_is_noop_without_an_existing_delete() {
        let conn = test_conn();
        note_write(&conn, "users", "username_lower", "alice", 1000).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM replication_ops", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "live writes must not bloat the op-log");
    }

    #[test]
    fn merge_is_lww_on_stamp() {
        let src = test_conn();
        note_delete(&src, "config_rows", "id", "a@h", 5000).unwrap();
        let dst = test_conn();
        // Newer local upsert must not be regressed by an older delete op.
        note_delete(&dst, "config_rows", "id", "a@h", 1000).unwrap();
        note_write(&dst, "config_rows", "id", "a@h", 9000).unwrap(); // supersede → upsert@9000
        let bundle = replicate_export(&src).unwrap();
        replicate_merge(&dst, bundle).unwrap();
        let (op, stamp): (String, i64) = dst
            .query_row(
                "SELECT op, stamp_ms FROM replication_ops WHERE entity='config_rows' AND key_val='a@h'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            op, "upsert",
            "newer local upsert wins over older peer delete"
        );
        assert_eq!(stamp, 9000);
    }

    #[test]
    fn merge_applies_newer_peer_delete() {
        let src = test_conn();
        note_delete(&src, "replica_fixture", "name_lower", "bob", 9000).unwrap();
        let dst = test_conn();
        let bundle = replicate_export(&src).unwrap();
        let n = replicate_merge(&dst, bundle).unwrap();
        assert_eq!(n, 1);
        let op: String = dst
            .query_row(
                "SELECT op FROM replication_ops WHERE entity='replica_fixture' AND key_val='bob'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(op, "delete", "newer peer delete propagates");
    }

    fn peer_op(entity: &str, key_col: &str, key_val: &str, stamp_ms: i64) -> Value {
        serde_json::json!([{
            "op_id": utils::id::new().to_string(), "entity": entity, "key_col": key_col,
            "key_val": key_val, "op": "delete", "origin": "peer", "stamp_ms": stamp_ms,
        }])
    }

    #[test]
    fn merge_rejects_ops_outside_declared_entities_and_keys() {
        let dst = test_conn();
        let now = utils::time::now_millis_since_epoch();
        let cases = [
            ("settings", "key", "entity is not replicated"),
            (
                "replica_fixture",
                "payload",
                "key column is not a declared key",
            ),
            ("endpoints", "id", "entity is not peer-deletable"),
            (
                "replication_ops",
                "op_id",
                "key column is not a declared key",
            ),
        ];
        for (entity, col, reason) in cases {
            let n = macro_runtime::replicate_table::with_merge_peer("peer-ops", || {
                replicate_merge(&dst, peer_op(entity, col, "x", now)).unwrap()
            });
            assert_eq!(n, 0, "{entity}.{col}");
            assert!(
                macro_runtime::replicate_table::rejection_counts().contains_key(&(
                    "peer-ops".into(),
                    ENTITY.into(),
                    reason.into()
                )),
                "{reason}"
            );
        }
    }

    #[test]
    fn merge_rejects_config_delete_for_local_only_noun() {
        let dst = test_conn();
        crate::config_store::set(&dst, "h", "h", "backup", "targets", "{}", "t").unwrap();
        let id = crate::config_store::get(&dst, "backup", "targets")
            .unwrap()
            .unwrap()
            .id;
        let now = utils::time::now_millis_since_epoch();
        assert_eq!(
            replicate_merge(&dst, peer_op("config_rows", "id", &id, now)).unwrap(),
            0
        );
        crate::config_store::set(&dst, "h", "h", "display", "target", "{}", "t").unwrap();
        let id = crate::config_store::get(&dst, "display", "target")
            .unwrap()
            .unwrap()
            .id;
        assert_eq!(
            replicate_merge(&dst, peer_op("config_rows", "id", &id, now)).unwrap(),
            0,
            "no sender: fail closed"
        );
        let from = |sender: &str| {
            macro_runtime::replicate_table::with_merge_peer(sender, || {
                replicate_merge(&dst, peer_op("config_rows", "id", &id, now)).unwrap()
            })
        };
        assert_eq!(from("other"), 0, "sender must own the row");
        assert_eq!(from("h"), 1);
    }

    #[test]
    fn merge_bounds_op_stamp_skew() {
        let dst = test_conn();
        let now = utils::time::now_millis_since_epoch();
        let ok = peer_op("replica_fixture", "name_lower", "a", now + 300_000);
        assert_eq!(replicate_merge(&dst, ok).unwrap(), 1);
        let late = peer_op("replica_fixture", "name_lower", "b", now + 301_000);
        assert_eq!(replicate_merge(&dst, late).unwrap(), 0);
    }

    fn peer_mesh_op(op: &str, key_val: &str) -> Value {
        serde_json::json!([{
            "op_id": utils::id::new().to_string(), "entity": "mesh_peers", "key_col": "peer_id",
            "key_val": key_val, "op": op, "origin": "peer",
            "stamp_ms": utils::time::now_millis_since_epoch(),
        }])
    }

    #[test]
    fn mesh_peer_tombstone_only_from_the_retiring_peer() {
        let dst = test_conn();
        crate::settings::set(&dst, crate::config_store::LOCAL_SYSTEM_ID_SETTING, "me").unwrap();
        let merge = |sender: &str, rows: Value| {
            macro_runtime::replicate_table::with_merge_peer(sender, || {
                replicate_merge(&dst, rows).unwrap()
            })
        };
        assert_eq!(merge("p2", peer_mesh_op("delete", "p1")), 0);
        assert_eq!(merge("p1", peer_mesh_op("upsert", "p1")), 0);
        assert_eq!(merge("me", peer_mesh_op("delete", "me")), 0);
        assert_eq!(merge("p1", peer_mesh_op("delete", "p1")), 1);
    }

    #[test]
    fn apply_skips_peer_ops_for_non_deletable_entities_but_not_local_ones() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO endpoints (id, provider, name) VALUES ('e1','p','a'), ('e2','p','b')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO replication_ops (op_id, entity, key_col, key_val, op, origin, stamp_ms, local_origin)
             VALUES ('o1','endpoints','id','e1','delete','peer',1,0)",
            [],
        )
        .unwrap();
        note_delete(&conn, "endpoints", "id", "e2", 1).unwrap();
        assert_eq!(apply_pending_deletes(&conn).unwrap(), 1);
        let left: String = conn
            .query_row("SELECT id FROM endpoints", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, "e1");
    }

    #[test]
    fn local_origin_migration_purges_every_unproven_op_except_config_rows() {
        let conn = test_conn();
        conn.execute_batch(
            "INSERT INTO replication_ops (op_id, entity, key_col, key_val, op, origin, stamp_ms) VALUES
               ('a','users','username_lower','x','delete','evil',1),
               ('b','mesh_peers','peer_id','y','delete','me',1),
               ('c','config_rows','id','z','delete','evil',1);",
        )
        .unwrap();
        let purge =
            include_str!("../migrations/20261017000001__replication_ops_purge_foreign.up.sql");
        conn.execute_batch(purge).unwrap();
        conn.execute_batch(purge).unwrap();
        let ids: Vec<String> = conn
            .prepare("SELECT op_id FROM replication_ops ORDER BY op_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec!["c".to_string()]);
    }

    #[test]
    fn is_deleted_honors_ttl_and_supersession() {
        let conn = test_conn();
        note_delete(&conn, "mesh_peers", "peer_id", "p1", 1_000).unwrap();
        // Active within the window.
        assert!(is_deleted(&conn, "mesh_peers", "p1", 1_000, 500).unwrap());
        // Past the TTL horizon → no longer suppressed.
        assert!(!is_deleted(&conn, "mesh_peers", "p1", 2_000, 500).unwrap());
        // A superseding upsert flips the op away from `delete`.
        note_write(&conn, "mesh_peers", "peer_id", "p1", 3_000).unwrap();
        assert!(!is_deleted(&conn, "mesh_peers", "p1", 3_000, 1_000_000).unwrap());
        // Unknown key is never deleted.
        assert!(!is_deleted(&conn, "mesh_peers", "other", 1_000, 500).unwrap());
    }

    #[test]
    fn reap_drops_only_ops_past_horizon() {
        let conn = test_conn();
        note_delete(&conn, "users", "username_lower", "old", 1_000).unwrap();
        note_delete(&conn, "users", "username_lower", "new", 1_000_000).unwrap();
        let dropped = reap(&conn, 1_000_000, 500_000).unwrap();
        assert_eq!(dropped, 1, "only the op older than now-ttl is reaped");
        let survivor: String = conn
            .query_row("SELECT key_val FROM replication_ops", [], |r| r.get(0))
            .unwrap();
        assert_eq!(survivor, "new");
    }

    #[test]
    fn apply_skips_non_identifier_entity() {
        let conn = test_conn();
        // Hand-craft a hostile op row; apply must skip it, not build unsafe SQL.
        conn.execute(
            "INSERT INTO replication_ops (op_id, entity, key_col, key_val, op, origin, stamp_ms)
             VALUES ('op1', 'users; DROP TABLE users', 'id', 'x', 'delete', 'h', 1)",
            [],
        )
        .unwrap();
        // Must not error or drop the table.
        apply_pending_deletes(&conn).unwrap();
        let ok: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, 0, "users table still exists (query succeeds)");
    }
}
