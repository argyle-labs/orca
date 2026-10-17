//! Generic, column-list-driven table replication — the runtime behind an
//! `endpoint_resource!(… lww = "…")` table's mesh sync.
//!
//! The `#[derive(Replicated)]` macro emits per-struct `export`/`merge` bodies
//! bound to a known field set. `endpoint_resource!` tables have a *dynamic*
//! column set (the PK, the declared fields, the built-in `addresses`/`enabled`/
//! timestamp columns), so rather than emit all that SQL from the macro, the
//! macro emits two one-line wrappers that call the functions here with the
//! table's column list. The heavy row↔JSON logic lives here as ordinary,
//! unit-tested Rust instead of generated tokens.
//!
//! Semantics match the derive exactly: export is `SELECT <cols> ORDER BY <pk>`;
//! merge is last-write-wins on the `lww` column, upserting on the PK. A row
//! whose `lww` is not strictly newer than the local copy is skipped, so a peer
//! can never regress fresher local state.
//!
//! Gated behind `replication` — it takes a live `rusqlite::Connection`, a
//! core-only concern (a thin plugin links no rusqlite).
#![cfg(feature = "replication")]
// The replication bundle is a heterogeneous registry of per-table rows with
// dynamic columns, so the wire payload is genuinely free-form JSON at this
// boundary — there is no single typed struct to model it against. This is the
// same contract (and the same justified allow) the `Replicated` derive and
// `ReplicatedRegistration` already carry.
#![allow(clippy::disallowed_types)]

use ::std::cell::RefCell;
use ::std::collections::BTreeMap;
use ::std::fmt::Debug;
use ::std::sync::Mutex;

use ::anyhow::{Context, Result};
use ::rusqlite::Connection;
use ::rusqlite::types::{Value as SqlValue, ValueRef};
use ::serde_json::{Map, Value};

pub const MAX_LWW_SKEW_MILLIS: i64 = 300_000;

pub const MAX_BUNDLE_ROWS: usize = 100_000;

const AUDIT_KEY_MAX: usize = 64;

type Pending = BTreeMap<(String, String), (u64, String)>;

thread_local! {
    static MERGE_PEER: RefCell<Option<String>> = const { RefCell::new(None) };
    static PENDING: RefCell<Pending> = const { RefCell::new(BTreeMap::new()) };
}

static REJECTIONS: Mutex<BTreeMap<(String, String, String), u64>> = Mutex::new(BTreeMap::new());

struct PeerGuard {
    prev: Option<String>,
    prev_pending: Pending,
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        let peer = MERGE_PEER.with(|p| p.replace(self.prev.take()));
        let pending = PENDING.with(|p| p.replace(::std::mem::take(&mut self.prev_pending)));
        let peer = peer.unwrap_or_default();
        for ((table, reason), (count, sample)) in pending {
            ::tracing::warn!(target: "audit", peer = %peer, table, reason, count, sample_key = %sample, "replicated rows rejected");
        }
    }
}

pub fn with_merge_peer<R>(peer: &str, f: impl FnOnce() -> R) -> R {
    let _guard = PeerGuard {
        prev: MERGE_PEER.with(|p| p.replace(Some(peer.to_string()))),
        prev_pending: PENDING.with(|p| p.take()),
    };
    f()
}

pub fn current_merge_peer() -> Option<String> {
    MERGE_PEER.with(|p| p.borrow().clone())
}

fn truncated_key(key: &dyn Debug) -> String {
    let mut k = format!("{key:?}");
    if k.len() > AUDIT_KEY_MAX {
        let mut end = AUDIT_KEY_MAX;
        while !k.is_char_boundary(end) {
            end -= 1;
        }
        k.truncate(end);
        k.push('…');
    }
    k
}

pub fn audit_reject(table: &str, key: &dyn Debug, reason: &str) {
    let key = truncated_key(key);
    let peer = current_merge_peer();
    {
        let mut counts = REJECTIONS.lock().unwrap_or_else(|e| e.into_inner());
        *counts
            .entry((
                peer.clone().unwrap_or_else(|| "unknown".into()),
                table.to_string(),
                reason.to_string(),
            ))
            .or_default() += 1;
    }
    if peer.is_none() {
        ::tracing::warn!(target: "audit", peer = "unknown", table, key = %key, reason, "replicated row rejected");
        return;
    }
    PENDING.with(|p| {
        p.borrow_mut()
            .entry((table.to_string(), reason.to_string()))
            .or_insert((0, key))
            .0 += 1;
    });
}

pub fn rejection_counts() -> BTreeMap<(String, String, String), u64> {
    REJECTIONS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn canonical_rfc3339_millis(s: &str) -> Option<i64> {
    let ts = ::utils::time::Timestamp::parse_rfc3339(s).ok()?;
    (ts.to_rfc3339() == s).then(|| ts.unix_millis())
}

fn check_lww_millis(millis: Option<i64>) -> Result<i64, &'static str> {
    match millis {
        None => Err("non-canonical lww"),
        Some(m) if m > ::utils::time::now_millis_since_epoch() + MAX_LWW_SKEW_MILLIS => {
            Err("lww too far in the future")
        }
        Some(m) => Ok(m),
    }
}

pub fn check_incoming_millis(millis: i64) -> Result<i64, &'static str> {
    check_lww_millis(Some(millis))
}

pub fn check_incoming_lww(lww: &str) -> Result<i64, &'static str> {
    check_lww_millis(canonical_rfc3339_millis(lww))
}

pub fn lww_str_is_newer(incoming: &str, local: &str) -> bool {
    match (
        canonical_rfc3339_millis(incoming),
        ::utils::time::Timestamp::parse_rfc3339(local).map(|t| t.unix_millis()),
    ) {
        (Some(i), Ok(l)) => i > l,
        _ => false,
    }
}

fn check_incoming_lww_json(lww: &Value) -> Result<i64, &'static str> {
    match lww {
        Value::String(s) => check_incoming_lww(s),
        Value::Number(n) => check_lww_millis(n.as_i64()),
        _ => Err("uncomparable lww"),
    }
}

/// `SELECT <columns> FROM <table> ORDER BY <pk> ASC`, returned as a JSON array
/// of `{column: value}` objects — the wire shape the merge side consumes.
pub fn export_table(conn: &Connection, table: &str, columns: &[&str], pk: &str) -> Result<Value> {
    let cols_csv = columns.join(", ");
    let sql = format!("SELECT {cols_csv} FROM {table} ORDER BY {pk} ASC");
    let mut stmt = conn
        .prepare(&sql)
        .with_context(|| format!("prepare export {table}"))?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut obj = Map::with_capacity(columns.len());
        for (i, col) in columns.iter().enumerate() {
            obj.insert((*col).to_string(), sql_to_json(row.get_ref(i)?));
        }
        out.push(Value::Object(obj));
    }
    Ok(Value::Array(out))
}

/// Merge a peer's exported rows into `table`, last-write-wins on `lww`. Returns
/// the number of rows actually written (newer-than-local). Upserts on `pk`; a
/// row whose `lww` is `<=` the local row's is skipped. Every column except `pk`,
/// `lww` and the `peer_writable` entries keeps its local value; a
/// `column.key` entry lets a peer write only that key of a JSON column.
pub fn merge_table(
    conn: &Connection,
    table: &str,
    columns: &[&str],
    pk: &str,
    lww: &str,
    peer_writable: &[&str],
    rows: Value,
) -> Result<usize> {
    let rows: Vec<Value> = ::serde_json::from_value(rows).context("merge rows not an array")?;
    let cols_csv = columns.join(", ");
    let placeholders = (1..=columns.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let update_set = columns
        .iter()
        .filter(|c| **c != pk)
        .map(|c| format!("{c} = excluded.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let insert_sql = format!(
        "INSERT INTO {table} ({cols_csv}) VALUES ({placeholders}) ON CONFLICT({pk}) DO UPDATE SET {update_set}"
    );
    let lww_sql = format!("SELECT {lww} FROM {table} WHERE {pk} = ?1");
    let protection = Protection::new(columns, &[pk, lww], peer_writable);
    let protected_sql = protection.select(table, &format!("{pk} = ?1"));

    let mut merged = 0usize;
    for row in &rows {
        let mut obj = row.as_object().context("merge row not an object")?.clone();
        let incoming_lww = obj
            .get(lww)
            .context("merge row missing lww column")?
            .clone();
        let pk_json = obj.get(pk).context("merge row missing pk column")?.clone();
        if let Err(reason) = check_incoming_lww_json(&incoming_lww) {
            audit_reject(table, &pk_json, reason);
            continue;
        }
        let pk_val = json_to_sql(&pk_json);

        // Last-write-wins: skip when the local copy is at least as new.
        let local_lww: Option<SqlValue> = conn
            .query_row(&lww_sql, [&pk_val], |r| r.get::<_, SqlValue>(0))
            .ok();
        if let Some(local) = local_lww
            && !json_is_newer_than_sql(&incoming_lww, &local)
        {
            continue;
        }
        if !protection.apply(
            conn,
            table,
            protected_sql.as_deref(),
            std::slice::from_ref(&pk_val),
            &pk_json,
            &mut obj,
        )? {
            continue;
        }

        let vals: Vec<SqlValue> = columns
            .iter()
            .map(|c| json_to_sql(obj.get(*c).unwrap_or(&Value::Null)))
            .collect();
        if let Err(e) = conn.execute(&insert_sql, ::rusqlite::params_from_iter(vals.iter())) {
            ::tracing::debug!("[replicate] {table} row write failed: {e}");
            audit_reject(table, &pk_json, "row write failed");
            continue;
        }
        merged += 1;
    }
    Ok(merged)
}

/// Like [`merge_table`], but reconciles on a NATURAL KEY (one or more columns)
/// instead of the PK, with LWW on `lww`. For each incoming row: find the local
/// row matching the natural key; skip if its `lww` is not strictly newer than
/// local; otherwise DELETE any local row matching the natural key OR the incoming
/// PK value (so neither the PK nor a `UNIQUE(natural_key)` index can collide),
/// then INSERT the incoming row. A row whose PK and natural key match different
/// local rows is skipped. Use when the table's real identity is a natural
/// key but the PK is an independently-minted surrogate (e.g. mounts: PK=uuidv7 id,
/// natural key=(host,name), each host minted its own id). Only `lww` and the
/// `peer_writable` columns take a peer's value, so the PK converges only when
/// listed there.
///
/// `preserve` lists columns that are host-LOCAL and must NOT be overwritten by a
/// peer's merge — per-tick runtime values (e.g. mounts' `health`/`active_route`)
/// that the exporter deliberately omits ([[data-classification-config-syncs-history-local]]).
/// They are not in `columns`, so the DELETE+INSERT would reset them to their
/// column defaults; to keep local ownership, the local row's `preserve` values are
/// read BEFORE the delete and re-applied AFTER the insert. A row with no prior
/// local copy keeps the CREATE defaults (this host has not yet measured it).
// The generic column-list replicator is inherently parameterised by table,
// column set, PK, natural key, LWW clock, and preserved columns — each is an
// independent axis of the merge, so they are passed positionally rather than
// bundled into a single-use struct.
#[allow(clippy::too_many_arguments)]
pub fn merge_table_natural(
    conn: &Connection,
    table: &str,
    columns: &[&str],
    pk: &str,
    natural_key: &[&str],
    lww: &str,
    preserve: &[&str],
    peer_writable: &[&str],
    rows: Value,
) -> Result<usize> {
    let rows: Vec<Value> = ::serde_json::from_value(rows).context("merge rows not an array")?;
    let cols_csv = columns.join(", ");
    let placeholders = (1..=columns.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let nk_conj = natural_key
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c} = ?{}", i + 1))
        .collect::<Vec<_>>()
        .join(" AND ");
    let target_where = format!("({nk_conj}) OR {pk} = ?{}", natural_key.len() + 1);
    let target_sql = format!("SELECT {pk}, {lww} FROM {table} WHERE {target_where}");
    let protection = Protection::new(columns, &[lww], peer_writable);
    let protected_sql = protection.select(table, &target_where);
    let preserve_select_sql = (!preserve.is_empty()).then(|| {
        format!(
            "SELECT {} FROM {table} WHERE {target_where}",
            preserve.join(", ")
        )
    });
    let preserve_update_sql = (!preserve.is_empty()).then(|| {
        let set = preserve
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c} = ?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "UPDATE {table} SET {set} WHERE {pk} = ?{}",
            preserve.len() + 1
        )
    });
    let delete_sql = format!("DELETE FROM {table} WHERE {target_where}");
    let insert_sql = format!("INSERT INTO {table} ({cols_csv}) VALUES ({placeholders})");

    let mut merged = 0usize;
    for row in &rows {
        let mut obj = row.as_object().context("merge row not an object")?.clone();
        let incoming_lww = obj
            .get(lww)
            .context("merge row missing lww column")?
            .clone();
        let pk_json = obj.get(pk).context("merge row missing pk column")?.clone();
        let nk_vals: Vec<SqlValue> = natural_key
            .iter()
            .map(|c| {
                Ok(json_to_sql(
                    obj.get(*c)
                        .context("merge row missing natural-key column")?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        if let Err(reason) = check_incoming_lww_json(&incoming_lww) {
            audit_reject(table, &nk_vals, reason);
            continue;
        }
        let mut target_params = nk_vals.clone();
        target_params.push(json_to_sql(&pk_json));

        let targets: Vec<(SqlValue, SqlValue)> = conn
            .prepare(&target_sql)?
            .query_map(::rusqlite::params_from_iter(target_params.iter()), |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<::rusqlite::Result<_>>()?;
        if targets.len() > 1 {
            audit_reject(table, &nk_vals, "pk and natural key match different rows");
            continue;
        }

        // Last-write-wins: skip when the local copy is at least as new.
        if let Some((_, local)) = targets.first()
            && !json_is_newer_than_sql(&incoming_lww, local)
        {
            continue;
        }
        if !protection.apply(
            conn,
            table,
            protected_sql.as_deref(),
            &target_params,
            &nk_vals,
            &mut obj,
        )? {
            continue;
        }

        let preserved: Option<Vec<SqlValue>> = preserve_select_sql.as_ref().and_then(|sql| {
            conn.query_row(
                sql,
                ::rusqlite::params_from_iter(target_params.iter()),
                |r| {
                    (0..preserve.len())
                        .map(|i| r.get::<_, SqlValue>(i))
                        .collect()
                },
            )
            .ok()
        });
        let vals: Vec<SqlValue> = columns
            .iter()
            .map(|c| json_to_sql(obj.get(*c).unwrap_or(&Value::Null)))
            .collect();
        let final_pk = json_to_sql(obj.get(pk).unwrap_or(&Value::Null));

        conn.execute_batch("SAVEPOINT merge_row")?;
        let written = (|| -> ::rusqlite::Result<()> {
            conn.execute(
                &delete_sql,
                ::rusqlite::params_from_iter(target_params.iter()),
            )?;
            conn.execute(&insert_sql, ::rusqlite::params_from_iter(vals.iter()))?;
            if let (Some(sql), Some(vals)) = (preserve_update_sql.as_ref(), preserved) {
                let params: Vec<SqlValue> = vals.into_iter().chain([final_pk]).collect();
                conn.execute(sql, ::rusqlite::params_from_iter(params.iter()))?;
            }
            Ok(())
        })();
        match written {
            Ok(()) => {
                conn.execute_batch("RELEASE merge_row")?;
                merged += 1;
            }
            Err(e) => {
                conn.execute_batch("ROLLBACK TO merge_row; RELEASE merge_row")?;
                ::tracing::debug!("[replicate] {table} row write failed: {e}");
                audit_reject(table, &nk_vals, "row write failed");
            }
        }
    }
    Ok(merged)
}

struct Protection<'a> {
    cols: Vec<&'a str>,
    json_writable: Vec<(&'a str, &'a str)>,
}

impl<'a> Protection<'a> {
    fn new(columns: &[&'a str], exempt: &[&str], peer_writable: &[&'a str]) -> Self {
        let cols: Vec<&str> = columns
            .iter()
            .copied()
            .filter(|c| !exempt.contains(c) && !peer_writable.contains(c))
            .collect();
        let json_writable = peer_writable
            .iter()
            .filter_map(|w| w.split_once('.'))
            .filter(|(col, _)| cols.contains(col))
            .collect();
        Self {
            cols,
            json_writable,
        }
    }

    fn select(&self, table: &str, where_clause: &str) -> Option<String> {
        (!self.cols.is_empty()).then(|| {
            format!(
                "SELECT {} FROM {table} WHERE {where_clause}",
                self.cols.join(", ")
            )
        })
    }

    fn apply(
        &self,
        conn: &Connection,
        table: &str,
        sql: Option<&str>,
        params: &[SqlValue],
        key: &dyn Debug,
        obj: &mut Map<String, Value>,
    ) -> Result<bool> {
        let Some(sql) = sql else {
            return Ok(true);
        };
        let local: Option<Vec<Value>> = {
            let mut stmt = conn.prepare(sql)?;
            let mut rows = stmt.query(::rusqlite::params_from_iter(params.iter()))?;
            match rows.next()? {
                None => None,
                Some(r) => Some(
                    (0..self.cols.len())
                        .map(|i| r.get_ref(i).map(sql_to_json))
                        .collect::<::rusqlite::Result<_>>()?,
                ),
            }
        };
        let Some(local) = local else {
            audit_reject(table, key, "new row with protected fields from a peer");
            return Ok(false);
        };
        let mut changed = false;
        for (col, lv) in self.cols.iter().zip(local) {
            let keys: Vec<&str> = self
                .json_writable
                .iter()
                .filter(|(c, _)| c == col)
                .map(|(_, k)| *k)
                .collect();
            let merged = if keys.is_empty() {
                lv
            } else {
                merge_json_keys(&lv, obj.get(*col), &keys)
            };
            if obj.get(*col) != Some(&merged) {
                changed = true;
                obj.insert((*col).to_string(), merged);
            }
        }
        if changed {
            audit_reject(table, key, "peer change to protected fields dropped");
        }
        Ok(true)
    }
}

fn merge_json_keys(local: &Value, incoming: Option<&Value>, keys: &[&str]) -> Value {
    let parse = |v: Option<&Value>| match v {
        Some(Value::String(s)) => ::serde_json::from_str::<Map<String, Value>>(s).ok(),
        _ => None,
    };
    let Some(mut out) = parse(Some(local)) else {
        return local.clone();
    };
    let inc = parse(incoming).unwrap_or_default();
    let mut changed = false;
    for k in keys {
        if out.get(*k) != inc.get(*k) {
            changed = true;
            match inc.get(*k) {
                Some(v) => out.insert((*k).to_string(), v.clone()),
                None => out.remove(*k),
            };
        }
    }
    if changed {
        Value::String(Value::Object(out).to_string())
    } else {
        local.clone()
    }
}

/// Read a SQLite cell as JSON. Endpoint tables are TEXT/INTEGER only; REAL/NULL
/// are handled for completeness and BLOB is carried losslessly as a byte array.
fn sql_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Value::from(f),
        ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::from(b.to_vec()),
    }
}

/// Bind a JSON value back to a SQLite value for upsert. Inverse of [`sql_to_json`]
/// for the cases endpoint tables use; a nested object/array (e.g. the
/// `addresses` JSON) round-trips as its compact string form.
fn json_to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) if n.is_i64() => SqlValue::Integer(n.as_i64().unwrap()),
        Value::Number(n) if n.is_u64() => SqlValue::Integer(n.as_u64().unwrap() as i64),
        Value::Number(n) => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Array(a)
            if a.iter()
                .all(|e| e.as_u64().map(|n| n <= 255).unwrap_or(false)) =>
        {
            SqlValue::Blob(a.iter().map(|e| e.as_u64().unwrap() as u8).collect())
        }
        other => SqlValue::Text(other.to_string()),
    }
}

/// Is the incoming JSON `lww` strictly newer than the local SQLite `lww`?
/// Compares as strings when both are text (ISO timestamps sort lexically) and
/// numerically when both are integers — the two shapes `lww` columns take.
/// Uncomparable shapes are never newer.
fn json_is_newer_than_sql(incoming: &Value, local: &SqlValue) -> bool {
    match (incoming, local) {
        (Value::String(i), SqlValue::Text(l)) => lww_str_is_newer(i, l),
        (Value::Number(i), SqlValue::Integer(l)) => i.as_i64().unwrap_or(i64::MIN) > *l,
        (Value::Number(i), SqlValue::Real(l)) => i.as_f64().unwrap_or(f64::MIN) > *l,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE t (name TEXT PRIMARY KEY, val TEXT, enabled INTEGER, updated_at TEXT);",
        )
        .unwrap();
        c
    }

    fn insert(c: &Connection, name: &str, val: &str, updated: &str) {
        c.execute(
            "INSERT INTO t (name, val, enabled, updated_at) VALUES (?1,?2,1,?3)",
            ::rusqlite::params![name, val, updated],
        )
        .unwrap();
    }

    const COLS: &[&str] = &["name", "val", "enabled", "updated_at"];

    #[test]
    fn export_round_trips_rows_ordered_by_pk() {
        let c = setup();
        insert(&c, "b", "two", "2026-01-02T00:00:00Z");
        insert(&c, "a", "one", "2026-01-01T00:00:00Z");
        let v = export_table(&c, "t", COLS, "name").unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "a"); // ordered by pk
        assert_eq!(arr[1]["val"], "two");
        assert_eq!(arr[0]["enabled"], 1);
    }

    #[test]
    fn merge_inserts_new_and_lww_updates_but_never_regresses() {
        let dst = setup();
        insert(&dst, "a", "old", "2026-01-01T00:00:00Z");
        insert(&dst, "keep", "fresh", "2026-06-01T00:00:00Z");

        // Peer: 'a' is newer (update), 'new' is unseen (insert), 'keep' is
        // STALE (must be ignored — local is fresher).
        let bundle = ::serde_json::json!([
            {"name":"a","val":"new","enabled":0,"updated_at":"2026-05-01T00:00:00Z"},
            {"name":"new","val":"n","enabled":1,"updated_at":"2026-05-01T00:00:00Z"},
            {"name":"keep","val":"STALE","enabled":1,"updated_at":"2026-02-01T00:00:00Z"},
        ]);
        let n = merge_table(
            &dst,
            "t",
            COLS,
            "name",
            "updated_at",
            &["val", "enabled"],
            bundle,
        )
        .unwrap();
        assert_eq!(n, 2, "only 'a' (newer) and 'new' (unseen) written");

        let a: String = dst
            .query_row("SELECT val FROM t WHERE name='a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(a, "new", "newer peer row applied");
        let keep: String = dst
            .query_row("SELECT val FROM t WHERE name='keep'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            keep, "fresh",
            "fresher local row not regressed by stale peer"
        );
        let new_val: String = dst
            .query_row("SELECT val FROM t WHERE name='new'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(new_val, "n", "unseen peer row inserted");
    }

    #[test]
    fn merge_propagates_tombstone_and_resurrection_by_lww() {
        // A `deleted` flag is just another replicated column — LWW carries it, so
        // a fleet-wide DELETE (tombstone) and a later re-create (resurrection)
        // both converge by the clock. This is the guarantee that makes any
        // replicated row removable everywhere.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE t (name TEXT PRIMARY KEY, val TEXT, deleted INTEGER, updated_at TEXT);",
        )
        .unwrap();
        c.execute(
            "INSERT INTO t (name,val,deleted,updated_at) VALUES ('a','live',0,'2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        const COLS: &[&str] = &["name", "val", "deleted", "updated_at"];

        // Peer deletes 'a' (tombstone, newer clock) → local converges to deleted.
        let tomb = ::serde_json::json!([{"name":"a","val":"live","deleted":1,"updated_at":"2026-02-01T00:00:00Z"}]);
        assert_eq!(
            merge_table(
                &c,
                "t",
                COLS,
                "name",
                "updated_at",
                &["val", "deleted"],
                tomb
            )
            .unwrap(),
            1
        );
        let d: i64 = c
            .query_row("SELECT deleted FROM t WHERE name='a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(d, 1, "tombstone propagated — row is deleted locally");

        // A later re-create (deleted=0, newer clock still) resurrects it.
        let res = ::serde_json::json!([{"name":"a","val":"back","deleted":0,"updated_at":"2026-03-01T00:00:00Z"}]);
        assert_eq!(
            merge_table(
                &c,
                "t",
                COLS,
                "name",
                "updated_at",
                &["val", "deleted"],
                res
            )
            .unwrap(),
            1
        );
        let (d2, v): (i64, String) = c
            .query_row("SELECT deleted, val FROM t WHERE name='a'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((d2, v.as_str()), (0, "back"), "newer re-create resurrects");

        // A STALE delete (older clock) must NOT regress a resurrected row.
        let stale = ::serde_json::json!([{"name":"a","val":"live","deleted":1,"updated_at":"2026-02-15T00:00:00Z"}]);
        assert_eq!(
            merge_table(
                &c,
                "t",
                COLS,
                "name",
                "updated_at",
                &["val", "deleted"],
                stale
            )
            .unwrap(),
            0
        );
        let d3: i64 = c
            .query_row("SELECT deleted FROM t WHERE name='a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            d3, 0,
            "stale tombstone ignored — no resurrection-then-death"
        );
    }

    fn setup_natural() -> Connection {
        // Mirrors the mounts table shape: surrogate uuidv7 PK, natural key
        // (host, name) with a UNIQUE index, unix-millis LWW clock.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE m (id TEXT PRIMARY KEY, host TEXT, name TEXT, updated_at INTEGER, UNIQUE(host, name));",
        )
        .unwrap();
        c
    }

    fn insert_m(c: &Connection, id: &str, host: &str, name: &str, updated: i64) {
        c.execute(
            "INSERT INTO m (id, host, name, updated_at) VALUES (?1,?2,?3,?4)",
            ::rusqlite::params![id, host, name, updated],
        )
        .unwrap();
    }

    const MCOLS: &[&str] = &["id", "host", "name", "updated_at"];
    const NK: &[&str] = &["host", "name"];

    #[test]
    fn natural_merge_replaces_row_with_different_id_no_unique_violation() {
        let c = setup_natural();
        // Local row: (baldur, data) with this host's own minted id.
        insert_m(&c, "id-local", "baldur", "data", 100);
        // Peer sent the SAME (host, name) with a DIFFERENT id and a newer clock.
        let bundle = ::serde_json::json!([
            {"id":"id-peer","host":"baldur","name":"data","updated_at":200},
        ]);
        let n = merge_table_natural(&c, "m", MCOLS, "id", NK, "updated_at", &[], MCOLS, bundle)
            .unwrap();
        assert_eq!(n, 1, "newer peer row applied");
        // Exactly one row for (baldur, data), and its id converged to the peer's.
        let (id, cnt): (String, i64) = c
            .query_row(
                "SELECT id, (SELECT COUNT(*) FROM m WHERE host='baldur' AND name='data') FROM m WHERE host='baldur' AND name='data'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cnt, 1, "no duplicate — natural key converged to one row");
        assert_eq!(id, "id-peer", "id converged to the incoming/newer row");
    }

    #[test]
    fn natural_merge_lww_protects_fresher_local_row() {
        let c = setup_natural();
        insert_m(&c, "id-local", "baldur", "data", 300);
        // Older incoming (different id) must be skipped — local is fresher.
        let bundle = ::serde_json::json!([
            {"id":"id-peer","host":"baldur","name":"data","updated_at":200},
        ]);
        let n = merge_table_natural(&c, "m", MCOLS, "id", NK, "updated_at", &[], MCOLS, bundle)
            .unwrap();
        assert_eq!(n, 0, "stale incoming skipped");
        let id: String = c
            .query_row(
                "SELECT id FROM m WHERE host='baldur' AND name='data'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(id, "id-local", "fresher local row untouched");
    }

    #[test]
    fn natural_merge_is_idempotent_on_equal_lww() {
        let c = setup_natural();
        insert_m(&c, "id-local", "baldur", "data", 200);
        let bundle = ::serde_json::json!([
            {"id":"id-peer","host":"baldur","name":"data","updated_at":200},
        ]);
        let n = merge_table_natural(&c, "m", MCOLS, "id", NK, "updated_at", &[], MCOLS, bundle)
            .unwrap();
        assert_eq!(n, 0, "equal lww is not newer — skipped");
        let id: String = c
            .query_row(
                "SELECT id FROM m WHERE host='baldur' AND name='data'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(id, "id-local", "unchanged on equal lww");
    }

    #[test]
    fn natural_merge_preserves_host_local_columns_but_updates_config() {
        // Mounts shape with host-LOCAL runtime columns (`health`, `active_route`)
        // that must never be replicated: a peer's merge updates CONFIG
        // (`remount_policy`, `routes`, `enabled`) but must not clobber this host's
        // freshly-measured liveness — even though the merge is a DELETE+INSERT.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE m (\
                id TEXT PRIMARY KEY, host TEXT, name TEXT, remount_policy TEXT, routes TEXT, \
                enabled INTEGER, health TEXT NOT NULL DEFAULT 'missing', active_route TEXT, \
                updated_at INTEGER, UNIQUE(host, name));",
        )
        .unwrap();
        c.execute(
            "INSERT INTO m (id, host, name, remount_policy, routes, enabled, health, active_route, updated_at) \
             VALUES ('id-local','baldur','data','{\"old\":1}','[]',1,'ok','10.0.0.1:/e',100)",
            [],
        )
        .unwrap();

        // Replicated columns exclude the host-local runtime pair; `preserve` names
        // them so they carry across the merge.
        const REPL: &[&str] = &[
            "id",
            "host",
            "name",
            "remount_policy",
            "routes",
            "enabled",
            "updated_at",
        ];
        const PRESERVE: &[&str] = &["health", "active_route"];
        // The peer's export never carries health/active_route (exporter omits them),
        // and it advances config with a newer clock.
        let bundle = ::serde_json::json!([
            {"id":"id-peer","host":"baldur","name":"data","remount_policy":"{\"new\":2}",
             "routes":"[{\"kind\":\"lan_v4\"}]","enabled":0,"updated_at":200},
        ]);
        let n = merge_table_natural(
            &c,
            "m",
            REPL,
            "id",
            NK,
            "updated_at",
            PRESERVE,
            REPL,
            bundle,
        )
        .unwrap();
        assert_eq!(n, 1, "newer peer config applied");

        let (rp, routes, enabled, health, active): (String, String, i64, String, Option<String>) =
            c.query_row(
                "SELECT remount_policy, routes, enabled, health, active_route FROM m \
                 WHERE host='baldur' AND name='data'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        // CONFIG converged to the peer's values …
        assert_eq!(rp, "{\"new\":2}", "remount_policy replicated");
        assert!(routes.contains("lan_v4"), "routes replicated");
        assert_eq!(enabled, 0, "enabled replicated");
        // … but the host-local runtime pair is untouched.
        assert_eq!(health, "ok", "local health preserved across merge");
        assert_eq!(
            active.as_deref(),
            Some("10.0.0.1:/e"),
            "local active_route preserved across merge"
        );
    }

    #[test]
    fn merge_is_idempotent_on_equal_lww() {
        let dst = setup();
        insert(&dst, "a", "v", "2026-01-01T00:00:00Z");
        let bundle = ::serde_json::json!([{"name":"a","val":"v2","enabled":1,"updated_at":"2026-01-01T00:00:00Z"}]);
        let n = merge_table(
            &dst,
            "t",
            COLS,
            "name",
            "updated_at",
            &["val", "enabled"],
            bundle,
        )
        .unwrap();
        assert_eq!(n, 0, "equal lww is not newer — skipped");
        let v: String = dst
            .query_row("SELECT val FROM t WHERE name='a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "v", "unchanged");
    }

    #[test]
    fn merge_rejects_future_and_uncomparable_lww() {
        let dst = setup();
        insert(&dst, "a", "old", "2026-01-01T00:00:00Z");
        let bundle = ::serde_json::json!([
            {"name":"a","val":"future","enabled":1,"updated_at":"9999-01-01T00:00:00Z"},
            {"name":"b","val":"garbage","enabled":1,"updated_at":"not-a-time"},
            {"name":"c","val":"bool","enabled":1,"updated_at":true},
        ]);
        let n = merge_table(
            &dst,
            "t",
            COLS,
            "name",
            "updated_at",
            &["val", "enabled"],
            bundle,
        )
        .unwrap();
        assert_eq!(n, 0);
        let a: String = dst
            .query_row("SELECT val FROM t WHERE name='a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(a, "old");
    }

    #[test]
    fn natural_merge_rejects_future_lww() {
        let c = setup_natural();
        insert_m(&c, "id-local", "baldur", "data", 100);
        let future = ::utils::time::now_millis_since_epoch() + MAX_LWW_SKEW_MILLIS + 60_000;
        let bundle = ::serde_json::json!([
            {"id":"id-peer","host":"baldur","name":"data","updated_at":future},
        ]);
        let n = merge_table_natural(&c, "m", MCOLS, "id", NK, "updated_at", &[], MCOLS, bundle)
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn uncomparable_local_is_not_newer() {
        assert!(!json_is_newer_than_sql(
            &Value::from("2026-01-01T00:00:00Z"),
            &SqlValue::Integer(1)
        ));
    }

    fn rfc3339_in(secs: i64) -> String {
        ::utils::time::now()
            .plus(::std::time::Duration::from_secs(secs as u64))
            .to_rfc3339()
    }

    #[test]
    fn skew_boundary_300_accepted_301_rejected() {
        assert!(check_incoming_lww(&rfc3339_in(300)).is_ok());
        assert_eq!(
            check_incoming_lww(&rfc3339_in(301)),
            Err("lww too far in the future")
        );
        let now = ::utils::time::now_millis_since_epoch();
        assert!(check_incoming_lww_json(&Value::from(now + 300_000)).is_ok());
        assert!(check_incoming_lww_json(&Value::from(now + 301_000)).is_err());
    }

    #[test]
    fn non_canonical_lww_rejected() {
        for bad in [
            "9",
            "",
            "2026-01-01",
            "2026-01-01t00:00:00z",
            "2026-01-01T00:00:00+14:00",
            "2026-01-01T00:00:00.5Z",
            "2026-01-01T00:00:00+00:00",
        ] {
            assert!(check_incoming_lww(bad).is_err(), "{bad} must be rejected");
        }
        assert!(check_incoming_lww("2026-01-01T00:00:00Z").is_ok());
    }

    #[test]
    fn lww_compares_parsed_instants() {
        assert!(!lww_str_is_newer("9", "2026-01-01T00:00:00Z"));
        assert!(!lww_str_is_newer(
            "2026-01-01T00:00:00Z",
            "2026-01-01T01:00:00+00:00"
        ));
        assert!(lww_str_is_newer(
            "2026-01-02T00:00:00Z",
            "2026-01-01T00:00:00Z"
        ));
    }

    #[test]
    fn merge_table_protects_columns_and_json_keys() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE e (id TEXT PRIMARY KEY, routes TEXT, config TEXT, note TEXT, updated_at TEXT);
             INSERT INTO e VALUES ('a','[\"10.0.0.1\"]','{\"url\":\"https://good\",\"tz\":\"utc\"}','n','2026-01-01T00:00:00Z');",
        )
        .unwrap();
        const ECOLS: &[&str] = &["id", "routes", "config", "note", "updated_at"];
        const WRITABLE: &[&str] = &["note", "config.tz"];
        let bundle = ::serde_json::json!([
            {"id":"a","routes":"[\"10.0.0.66\"]","config":"{\"url\":\"https://evil\",\"tz\":\"est\"}","note":"m","updated_at":"2026-02-01T00:00:00Z"},
            {"id":"b","routes":"[]","config":"{}","note":"x","updated_at":"2026-02-01T00:00:00Z"},
        ]);
        let n = with_merge_peer("peer-prot", || {
            merge_table(&c, "e", ECOLS, "id", "updated_at", WRITABLE, bundle).unwrap()
        });
        assert_eq!(n, 1);
        let (routes, config, note): (String, String, String) = c
            .query_row("SELECT routes, config, note FROM e WHERE id='a'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(routes, "[\"10.0.0.1\"]");
        let cfg: Value = ::serde_json::from_str(&config).unwrap();
        assert_eq!(cfg["url"], "https://good");
        assert_eq!(cfg["tz"], "est");
        assert_eq!(note, "m");
        let b: i64 = c
            .query_row("SELECT COUNT(*) FROM e WHERE id='b'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(b, 0, "new row with protected fields rejected");
        let counts = rejection_counts();
        assert!(counts.contains_key(&(
            "peer-prot".into(),
            "e".into(),
            "new row with protected fields from a peer".into()
        )));
        assert!(counts.contains_key(&(
            "peer-prot".into(),
            "e".into(),
            "peer change to protected fields dropped".into()
        )));
    }

    #[test]
    fn natural_merge_skips_pk_and_key_matching_different_rows() {
        let c = setup_natural();
        insert_m(&c, "id-1", "baldur", "data", 100);
        insert_m(&c, "id-2", "baldur", "other", 100);
        let bundle = ::serde_json::json!([
            {"id":"id-2","host":"baldur","name":"data","updated_at":200},
        ]);
        let n = merge_table_natural(&c, "m", MCOLS, "id", NK, "updated_at", &[], MCOLS, bundle)
            .unwrap();
        assert_eq!(n, 0);
        let cnt: i64 = c
            .query_row("SELECT COUNT(*) FROM m", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 2, "neither row deleted");
    }

    #[test]
    fn natural_merge_protects_columns() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE m (id TEXT PRIMARY KEY, host TEXT, name TEXT, target TEXT, note TEXT, updated_at INTEGER, UNIQUE(host, name));
             INSERT INTO m VALUES ('id-1','baldur','data','/mnt/data','a',100);",
        )
        .unwrap();
        const C: &[&str] = &["id", "host", "name", "target", "note", "updated_at"];
        let bundle = ::serde_json::json!([
            {"id":"id-1","host":"baldur","name":"data","target":"/etc","note":"b","updated_at":200},
            {"id":"id-9","host":"baldur","name":"new","target":"/","note":"b","updated_at":200},
        ]);
        let n = merge_table_natural(&c, "m", C, "id", NK, "updated_at", &[], &["note"], bundle)
            .unwrap();
        assert_eq!(n, 1);
        let (target, note): (String, String) = c
            .query_row("SELECT target, note FROM m WHERE id='id-1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((target.as_str(), note.as_str()), ("/mnt/data", "b"));
        let cnt: i64 = c
            .query_row("SELECT COUNT(*) FROM m", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 1);
    }

    #[test]
    fn merge_peer_restored_after_panic() {
        let r = ::std::panic::catch_unwind(|| with_merge_peer("p", || panic!("boom")));
        assert!(r.is_err());
        assert!(MERGE_PEER.with(|p| p.borrow().is_none()));
    }

    #[test]
    fn json_column_is_whole_protected_unless_keys_listed() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE e (id TEXT PRIMARY KEY, config TEXT, updated_at TEXT);
             INSERT INTO e VALUES ('a','{\"url\":\"https://good\"}','2026-01-01T00:00:00Z');",
        )
        .unwrap();
        let bundle = ::serde_json::json!([
            {"id":"a","config":"{\"url\":\"https://evil\",\"x\":1}","updated_at":"2026-02-01T00:00:00Z"},
        ]);
        merge_table(
            &c,
            "e",
            &["id", "config", "updated_at"],
            "id",
            "updated_at",
            &[],
            bundle,
        )
        .unwrap();
        let config: String = c
            .query_row("SELECT config FROM e WHERE id='a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(config, "{\"url\":\"https://good\"}");
    }

    #[test]
    fn natural_merge_protects_id_by_default() {
        let c = setup_natural();
        insert_m(&c, "id-local", "baldur", "data", 100);
        let bundle = ::serde_json::json!([
            {"id":"id-peer","host":"baldur","name":"data","updated_at":200},
        ]);
        let n =
            merge_table_natural(&c, "m", MCOLS, "id", NK, "updated_at", &[], &[], bundle).unwrap();
        assert_eq!(n, 1);
        let id: String = c.query_row("SELECT id FROM m", [], |r| r.get(0)).unwrap();
        assert_eq!(id, "id-local");
    }

    #[test]
    fn row_write_error_is_audited_and_merge_continues() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE u (id TEXT PRIMARY KEY, label TEXT UNIQUE, updated_at TEXT);
             INSERT INTO u VALUES ('a','taken','2026-01-01T00:00:00Z');",
        )
        .unwrap();
        let bundle = ::serde_json::json!([
            {"id":"b","label":"taken","updated_at":"2026-02-01T00:00:00Z"},
            {"id":"c","label":"free","updated_at":"2026-02-01T00:00:00Z"},
        ]);
        let n = with_merge_peer("peer-err", || {
            merge_table(
                &c,
                "u",
                &["id", "label", "updated_at"],
                "id",
                "updated_at",
                &["label"],
                bundle,
            )
            .unwrap()
        });
        assert_eq!(n, 1);
        assert!(rejection_counts().contains_key(&(
            "peer-err".into(),
            "u".into(),
            "row write failed".into()
        )));
    }

    #[test]
    fn audit_aggregates_per_merge_and_truncates_keys() {
        with_merge_peer("peer-agg", || {
            for _ in 0..3 {
                audit_reject("t", &"k".repeat(500), "r");
            }
            let pending = PENDING.with(|p| p.borrow().clone());
            let (count, sample) = &pending[&("t".to_string(), "r".to_string())];
            assert_eq!(*count, 3);
            assert!(sample.chars().count() <= AUDIT_KEY_MAX + 1);
        });
        assert!(PENDING.with(|p| p.borrow().is_empty()));
        assert_eq!(current_merge_peer(), None);
    }
}
