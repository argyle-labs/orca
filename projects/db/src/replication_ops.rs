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
        "INSERT INTO replication_ops (op_id, entity, key_col, key_val, op, origin, stamp_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(entity, key_val) DO UPDATE SET
             op_id    = excluded.op_id,
             key_col  = excluded.key_col,
             op       = excluded.op,
             origin   = excluded.origin,
             stamp_ms = excluded.stamp_ms
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
    let mut stmt =
        conn.prepare("SELECT entity, key_col, key_val FROM replication_ops WHERE op = 'delete'")?;
    let pending: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut removed = 0usize;
    for (entity, key_col, key_val) in pending {
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
           FROM replication_ops ORDER BY op_id",
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
        let entity = s("entity");
        let key_col = s("key_col");
        let key_val = s("key_val");
        let op = s("op");
        let origin = s("origin");
        let stamp_ms = row.get("stamp_ms").and_then(|v| v.as_i64()).unwrap_or(0);
        if op_id.is_empty() || entity.is_empty() || key_val.is_empty() || op.is_empty() {
            continue;
        }
        let n = conn.execute(
            "INSERT INTO replication_ops (op_id, entity, key_col, key_val, op, origin, stamp_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(entity, key_val) DO UPDATE SET
                 op_id    = excluded.op_id,
                 key_col  = excluded.key_col,
                 op       = excluded.op,
                 origin   = excluded.origin,
                 stamp_ms = excluded.stamp_ms
             WHERE excluded.stamp_ms > replication_ops.stamp_ms
                OR (excluded.stamp_ms = replication_ops.stamp_ms
                    AND excluded.op_id > replication_ops.op_id)",
            params![op_id, entity, key_col, key_val, op, origin, stamp_ms],
        )?;
        merged += n;
    }
    Ok(merged)
}

inventory::submit! {
    macro_runtime::ReplicatedRegistration {
        name: ENTITY,
        export: replicate_export,
        merge: replicate_merge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn note_delete_then_apply_removes_row() {
        let conn = test_conn();
        crate::users::insert(&conn, "u1", "scott", "h", "admin", "2026-01-01T00:00:00Z").unwrap();
        note_delete(&conn, "users", "username_lower", "scott", 1000).unwrap();
        // Simulate a stale peer re-inserting the row, then apply pending deletes.
        conn.execute(
            "INSERT OR IGNORE INTO users
                (id, username, username_lower, password_hash, role, created_at, password_updated_at, updated_at)
             VALUES ('u1','scott','scott','h','admin','t','t','t')",
            [],
        )
        .ok();
        let removed = apply_pending_deletes(&conn).unwrap();
        assert_eq!(removed, 1, "the deleted user must be physically removed");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
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
        crate::users::insert(&conn, "u2", "scott", "h", "admin", "t").unwrap();
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
        note_delete(&src, "users", "username_lower", "bob", 9000).unwrap();
        let dst = test_conn();
        note_write(&dst, "users", "username_lower", "bob", 1000).unwrap(); // no-op (no delete)
        let bundle = replicate_export(&src).unwrap();
        let n = replicate_merge(&dst, bundle).unwrap();
        assert_eq!(n, 1);
        let op: String = dst
            .query_row(
                "SELECT op FROM replication_ops WHERE entity='users' AND key_val='bob'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(op, "delete", "newer peer delete propagates");
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
