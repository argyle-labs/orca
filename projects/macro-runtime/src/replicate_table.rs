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

use ::anyhow::{Context, Result};
use ::rusqlite::Connection;
use ::rusqlite::types::{Value as SqlValue, ValueRef};
use ::serde_json::{Map, Value};

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
/// row whose `lww` is `<=` the local row's is skipped.
pub fn merge_table(
    conn: &Connection,
    table: &str,
    columns: &[&str],
    pk: &str,
    lww: &str,
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

    let mut merged = 0usize;
    for row in &rows {
        let obj = row.as_object().context("merge row not an object")?;
        let incoming_lww = obj.get(lww).context("merge row missing lww column")?;
        let pk_val = json_to_sql(obj.get(pk).context("merge row missing pk column")?);

        // Last-write-wins: skip when the local copy is at least as new.
        let local_lww: Option<SqlValue> = conn
            .query_row(&lww_sql, [&pk_val], |r| r.get::<_, SqlValue>(0))
            .ok();
        if let Some(local) = local_lww
            && !json_is_newer_than_sql(incoming_lww, &local)
        {
            continue;
        }

        let vals: Vec<SqlValue> = columns
            .iter()
            .map(|c| json_to_sql(obj.get(*c).unwrap_or(&Value::Null)))
            .collect();
        let params = ::rusqlite::params_from_iter(vals.iter());
        conn.execute(&insert_sql, params)
            .with_context(|| format!("merge upsert {table}"))?;
        merged += 1;
    }
    Ok(merged)
}

/// Read a SQLite cell as JSON. Endpoint tables are TEXT/INTEGER only; REAL/NULL
/// are handled for completeness and BLOB is carried losslessly as a byte array.
fn sql_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Value::from(f),
        ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::from(b.iter().copied().collect::<Vec<u8>>()),
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
fn json_is_newer_than_sql(incoming: &Value, local: &SqlValue) -> bool {
    match (incoming, local) {
        (Value::String(i), SqlValue::Text(l)) => i.as_str() > l.as_str(),
        (Value::Number(i), SqlValue::Integer(l)) => i.as_i64().unwrap_or(i64::MIN) > *l,
        (Value::Number(i), SqlValue::Real(l)) => i.as_f64().unwrap_or(f64::MIN) > *l,
        // Mixed/uncomparable: treat incoming as newer so a real change isn't lost.
        _ => true,
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
        insert(&c, "b", "two", "2026-01-02");
        insert(&c, "a", "one", "2026-01-01");
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
        insert(&dst, "a", "old", "2026-01-01");
        insert(&dst, "keep", "fresh", "2026-06-01");

        // Peer: 'a' is newer (update), 'new' is unseen (insert), 'keep' is
        // STALE (must be ignored — local is fresher).
        let bundle = ::serde_json::json!([
            {"name":"a","val":"new","enabled":0,"updated_at":"2026-05-01"},
            {"name":"new","val":"n","enabled":1,"updated_at":"2026-05-01"},
            {"name":"keep","val":"STALE","enabled":1,"updated_at":"2026-02-01"},
        ]);
        let n = merge_table(&dst, "t", COLS, "name", "updated_at", bundle).unwrap();
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
    fn merge_is_idempotent_on_equal_lww() {
        let dst = setup();
        insert(&dst, "a", "v", "2026-01-01");
        let bundle =
            ::serde_json::json!([{"name":"a","val":"v2","enabled":1,"updated_at":"2026-01-01"}]);
        let n = merge_table(&dst, "t", COLS, "name", "updated_at", bundle).unwrap();
        assert_eq!(n, 0, "equal lww is not newer — skipped");
        let v: String = dst
            .query_row("SELECT val FROM t WHERE name='a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "v", "unchanged");
    }
}
