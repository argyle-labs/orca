//! Plugin-declared **real SQL tables** with safe, additive diff-migration.
//!
//! A plugin declares its config/data tables as full typed schemas (real
//! columns + indexes — NOT JSONB, NOT a generic KV blob). orca materializes
//! each as a real SQL table and, on every re-declaration, **diffs** the
//! declared shape against what exists and applies an **additive** migration
//! (create-if-absent, add new columns, add new indexes) so existing data is
//! preserved end to end.
//!
//! ## Isolation + capability model
//!
//! Orca — not the plugin — owns the connection and performs every operation.
//! The plugin only ever supplies its `namespace` plus a *logical* table name;
//! the **physical** table name is derived here as `plug__<namespace>__<table>`.
//! A plugin therefore cannot name a core table or another plugin's table: the
//! derivation is the isolation boundary, and every identifier is validated
//! against a strict `[a-z_][a-z0-9_]*` allow-list before it touches SQL (no
//! quoting games, no injection). This is "the plugin declares its ability; orca
//! holds the power to act," applied to persistence.
//!
//! ## Why additive-only
//!
//! SQLite can `ADD COLUMN` cheaply but cannot retype/drop a column without a
//! table rebuild. A destructive change is never performed implicitly: a
//! declared column whose type conflicts with the live column is **refused**
//! (loudly) rather than silently rebuilt, and a column that disappears from the
//! declaration is **left in place** rather than dropped. Data safety wins over
//! tidiness; an intentional breaking migration is a separate, explicit step.

use anyhow::{Result, bail};
use rusqlite::Connection;

// The declared-schema descriptors are pure serde types in the ABI contract crate
// so a thin (rusqlite-free) plugin can build them; `db` owns the engine that
// turns them into real SQL tables. Aliased to the names this module already used.
pub use plugin_abi::{
    ColumnDef as ColumnSpec, IndexDef as IndexSpec, SchemaDecl, TableDef as TableSchema,
};

/// What a single [`apply`] did — surfaced so the loader can log exactly which
/// tables/columns/indexes a plugin's registration created or converged.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MigrationReport {
    pub physical_table: String,
    pub created_table: bool,
    pub added_columns: Vec<String>,
    pub created_indexes: Vec<String>,
}

/// Strict identifier allow-list. Anything a plugin contributes that reaches SQL
/// as an identifier (namespace, table, column, index) must match — no spaces,
/// no quotes, no dots, no leading digit. This is the injection boundary.
fn validate_ident(kind: &str, s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_lowercase() || (i > 0 && b.is_ascii_digit()));
    if !ok {
        bail!("invalid {kind} identifier `{s}`: must match [a-z_][a-z0-9_]* (max 64)");
    }
    Ok(())
}

/// Allow-list of column types. Keeps the declared type to SQLite's real storage
/// classes so a plugin can't smuggle a constraint clause through the type field.
fn validate_type(t: &str) -> Result<()> {
    match t.to_ascii_uppercase().as_str() {
        "TEXT" | "INTEGER" | "REAL" | "BLOB" | "NUMERIC" => Ok(()),
        other => bail!("invalid column type `{other}`: allowed TEXT|INTEGER|REAL|BLOB|NUMERIC"),
    }
}

/// Derive the physical table name for a plugin's logical table. The `plug__`
/// prefix + namespace segment is what keeps a plugin's tables in their own
/// space and unable to collide with core orca tables or another plugin's.
pub fn physical_table_name(namespace: &str, table: &str) -> Result<String> {
    validate_ident("namespace", namespace)?;
    validate_ident("table", table)?;
    Ok(format!("plug__{namespace}__{table}"))
}

fn physical_index_name(namespace: &str, table: &str, index: &str) -> Result<String> {
    validate_ident("index", index)?;
    Ok(format!("plug__{namespace}__{table}__{index}"))
}

/// Columns currently present on `physical` (name → declared type), via
/// `PRAGMA table_info`. Empty when the table does not exist.
fn existing_columns(conn: &Connection, physical: &str) -> Result<Vec<(String, String)>> {
    // `physical` is a validated, derived identifier — safe to interpolate.
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{physical}\")"))?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn column_ddl(c: &ColumnSpec) -> Result<String> {
    validate_ident("column", &c.name)?;
    validate_type(&c.sql_type)?;
    let mut ddl = format!("\"{}\" {}", c.name, c.sql_type.to_ascii_uppercase());
    if c.primary_key {
        ddl.push_str(" PRIMARY KEY");
    }
    if c.not_null {
        ddl.push_str(" NOT NULL");
    }
    if let Some(d) = &c.default {
        // Default is raw SQL by design (CURRENT_TIMESTAMP, 0, ''); it is not an
        // identifier. Keep it to a conservative shape so it can't carry a
        // statement terminator or comment.
        if d.contains(';') || d.contains("--") {
            bail!("column `{}` default contains illegal characters", c.name);
        }
        ddl.push_str(&format!(" DEFAULT {d}"));
    }
    Ok(ddl)
}

/// Materialize / converge one plugin-declared table. Idempotent: re-applying an
/// unchanged schema is a no-op; applying an evolved schema adds only what is
/// new. Never drops or retypes — a conflicting retype is refused.
pub fn apply(conn: &Connection, namespace: &str, schema: &TableSchema) -> Result<MigrationReport> {
    let physical = physical_table_name(namespace, &schema.table)?;
    if schema.columns.is_empty() {
        bail!("table `{}` declares no columns", schema.table);
    }

    let mut report = MigrationReport {
        physical_table: physical.clone(),
        ..Default::default()
    };

    let existing = existing_columns(conn, &physical)?;
    if existing.is_empty() {
        // Fresh create.
        let cols = schema
            .columns
            .iter()
            .map(column_ddl)
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        conn.execute_batch(&format!("CREATE TABLE \"{physical}\" ({cols})"))?;
        report.created_table = true;
    } else {
        // Diff against the live table.
        for c in &schema.columns {
            validate_ident("column", &c.name)?;
            if let Some((_, live_type)) = existing.iter().find(|(n, _)| n == &c.name) {
                // Present already — refuse a conflicting retype rather than
                // rebuild and risk data. Same affinity → fine.
                if !live_type.eq_ignore_ascii_case(&c.sql_type) {
                    bail!(
                        "column `{}.{}` is {live_type} on disk but declared {}; \
                         refusing implicit destructive retype",
                        schema.table,
                        c.name,
                        c.sql_type
                    );
                }
                continue;
            }
            // New column → additive ADD COLUMN. A NOT NULL add needs a default.
            if c.not_null && c.default.is_none() {
                bail!(
                    "new column `{}.{}` is NOT NULL but has no default; \
                     a default is required to add it to an existing table",
                    schema.table,
                    c.name
                );
            }
            conn.execute_batch(&format!(
                "ALTER TABLE \"{physical}\" ADD COLUMN {}",
                column_ddl(c)?
            ))?;
            report.added_columns.push(c.name.clone());
        }
    }

    // Indexes — additive, idempotent.
    for idx in &schema.indexes {
        let phys_idx = physical_index_name(namespace, &schema.table, &idx.name)?;
        if idx.columns.is_empty() {
            bail!("index `{}` lists no columns", idx.name);
        }
        for col in &idx.columns {
            validate_ident("column", col)?;
        }
        let cols = idx
            .columns
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let unique = if idx.unique { "UNIQUE " } else { "" };
        conn.execute_batch(&format!(
            "CREATE {unique}INDEX IF NOT EXISTS \"{phys_idx}\" ON \"{physical}\" ({cols})"
        ))?;
        report.created_indexes.push(phys_idx);
    }

    Ok(report)
}

/// Apply an entire plugin [`SchemaDecl`] — every declared table into the
/// plugin's namespace — in one pass. This is what the loader/installer calls
/// after `module.schemas()`: the plugin declares; orca migrates. Returns one
/// [`MigrationReport`] per table. A declaration with an empty namespace and no
/// tables is a clean no-op (the default for plugins that declare nothing).
pub fn apply_decl(conn: &Connection, decl: &SchemaDecl) -> Result<Vec<MigrationReport>> {
    if decl.tables.is_empty() {
        return Ok(Vec::new());
    }
    if decl.namespace.is_empty() {
        bail!("schema declaration lists tables but no namespace");
    }
    let mut reports = Vec::with_capacity(decl.tables.len());
    for table in &decl.tables {
        reports.push(apply(conn, &decl.namespace, table)?);
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        Connection::open_in_memory().expect("in-memory db")
    }

    fn schema_v1() -> TableSchema {
        TableSchema {
            table: "servers".into(),
            columns: vec![
                ColumnSpec {
                    name: "id".into(),
                    sql_type: "TEXT".into(),
                    not_null: true,
                    primary_key: true,
                    default: None,
                },
                ColumnSpec {
                    name: "url".into(),
                    sql_type: "TEXT".into(),
                    not_null: true,
                    primary_key: false,
                    default: None,
                },
            ],
            indexes: vec![IndexSpec {
                name: "by_url".into(),
                columns: vec!["url".into()],
                unique: false,
            }],
        }
    }

    #[test]
    fn physical_name_is_namespaced_and_isolating() {
        let n = physical_table_name("mcp", "servers").unwrap();
        assert_eq!(n, "plug__mcp__servers");
        // A plugin cannot escape into a core table name.
        assert!(physical_table_name("mcp", "plugins; DROP TABLE x").is_err());
        assert!(physical_table_name("../core", "servers").is_err());
        assert!(physical_table_name("MCP", "servers").is_err()); // uppercase rejected
    }

    #[test]
    fn create_then_additive_migrate_preserves_data() {
        let c = conn();
        let r = apply(&c, "mcp", &schema_v1()).unwrap();
        assert!(r.created_table);
        assert_eq!(r.physical_table, "plug__mcp__servers");

        c.execute(
            "INSERT INTO \"plug__mcp__servers\" (id, url) VALUES ('a', 'http://x')",
            [],
        )
        .unwrap();

        // Re-apply unchanged → no-op (no new columns).
        let again = apply(&c, "mcp", &schema_v1()).unwrap();
        assert!(!again.created_table);
        assert!(again.added_columns.is_empty());

        // Evolve: add a nullable column + a new column with a default.
        let mut v2 = schema_v1();
        v2.columns.push(ColumnSpec {
            name: "label".into(),
            sql_type: "TEXT".into(),
            not_null: false,
            primary_key: false,
            default: None,
        });
        v2.columns.push(ColumnSpec {
            name: "enabled".into(),
            sql_type: "INTEGER".into(),
            not_null: true,
            primary_key: false,
            default: Some("1".into()),
        });
        let mig = apply(&c, "mcp", &v2).unwrap();
        assert_eq!(mig.added_columns, vec!["label", "enabled"]);

        // Existing row survived and the defaulted column backfilled.
        let (url, enabled): (String, i64) = c
            .query_row(
                "SELECT url, enabled FROM \"plug__mcp__servers\" WHERE id='a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(url, "http://x");
        assert_eq!(enabled, 1);
    }

    #[test]
    fn not_null_add_without_default_is_refused() {
        let c = conn();
        apply(&c, "mcp", &schema_v1()).unwrap();
        let mut bad = schema_v1();
        bad.columns.push(ColumnSpec {
            name: "required".into(),
            sql_type: "TEXT".into(),
            not_null: true,
            primary_key: false,
            default: None,
        });
        assert!(apply(&c, "mcp", &bad).is_err());
    }

    #[test]
    fn conflicting_retype_is_refused_not_silently_rebuilt() {
        let c = conn();
        apply(&c, "mcp", &schema_v1()).unwrap();
        let mut retype = schema_v1();
        retype.columns[1].sql_type = "INTEGER".into(); // url TEXT -> INTEGER
        let err = apply(&c, "mcp", &retype).unwrap_err();
        assert!(err.to_string().contains("destructive retype"));
    }

    #[test]
    fn two_plugins_same_logical_table_are_isolated() {
        let c = conn();
        apply(&c, "mcp", &schema_v1()).unwrap();
        apply(&c, "docker", &schema_v1()).unwrap();
        // Distinct physical tables; data does not bleed across.
        c.execute(
            "INSERT INTO \"plug__mcp__servers\" (id, url) VALUES ('m', 'mcp')",
            [],
        )
        .unwrap();
        let docker_count: i64 = c
            .query_row("SELECT COUNT(*) FROM \"plug__docker__servers\"", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            docker_count, 0,
            "mcp's insert must not appear in docker's table"
        );
    }

    #[test]
    fn injection_attempts_are_rejected() {
        let c = conn();
        let mut evil = schema_v1();
        evil.columns[0].name = "id\"); DROP TABLE plugins;--".into();
        assert!(apply(&c, "mcp", &evil).is_err());
        evil = schema_v1();
        evil.columns[0].sql_type = "TEXT); DROP TABLE plugins;--".into();
        assert!(apply(&c, "mcp", &evil).is_err());
    }
}

// ── Runtime CRUD: the plugin's whole DB surface, run on core's connection ─────
//
// `exec_db_op` is what the loader binds into each plugin's `set_host` channel:
// the plugin never opens a connection, it sends a typed [`DbOp`] and core runs
// it here on its single pooled connection. Table + every identifier are
// validated and the table is resolved to `plug__<namespace>__<table>`, so a
// plugin can only ever touch its own namespace. This replaces the old
// per-plugin `runtime::open_db()` second connection that raced the daemon's on
// the WAL/shm index (SQLITE_IOERR_SHMOPEN 5898).

use plugin_abi::{DbOp, DbReply, DbRow, DbValue};

/// Resolve the physical table an op targets. A non-empty `namespace` is the
/// isolated plugin-declared case (`plug__<ns>__<table>`). An EMPTY namespace
/// means a core-migrated registry table the plugin owns by name (e.g.
/// `proxmox_endpoints` from `endpoint_resource!`): the literal name is used,
/// still validated against the strict identifier allow-list so it can't inject.
fn resolve_op_table(namespace: &str, table: &str) -> Result<String> {
    if namespace.is_empty() {
        validate_ident("table", table)?;
        Ok(table.to_string())
    } else {
        physical_table_name(namespace, table)
    }
}

fn to_sql(v: &DbValue) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match v {
        DbValue::Null => Value::Null,
        DbValue::Int(i) => Value::Integer(*i),
        DbValue::Real(f) => Value::Real(*f),
        DbValue::Text(s) => Value::Text(s.clone()),
        DbValue::Bool(b) => Value::Integer(*b as i64),
        DbValue::Blob(b) => Value::Blob(b.clone()),
    }
}

fn from_sql(v: rusqlite::types::ValueRef<'_>) -> DbValue {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => DbValue::Null,
        ValueRef::Integer(i) => DbValue::Int(i),
        ValueRef::Real(f) => DbValue::Real(f),
        ValueRef::Text(t) => DbValue::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => DbValue::Blob(b.to_vec()),
    }
}

/// Run a prepared SELECT and collect every row into a typed [`DbRow`].
fn collect_rows<P: rusqlite::Params>(
    stmt: &mut rusqlite::Statement<'_>,
    params: P,
) -> Result<Vec<DbRow>> {
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let mut map = DbRow::new();
        for (i, name) in cols.iter().enumerate() {
            map.insert(name.clone(), from_sql(r.get_ref(i)?));
        }
        out.push(map);
    }
    Ok(out)
}

/// SQLite storage class inferred from a value — the basis for the core-owned,
/// auto-materialized column type. `Null` can't be typed from data, so it falls
/// back to `TEXT` (a later non-null write of the same column keeps the affinity;
/// SQLite is dynamically typed so this is never lossy).
fn sql_type_for(v: &DbValue) -> &'static str {
    match v {
        DbValue::Int(_) | DbValue::Bool(_) => "INTEGER",
        DbValue::Real(_) => "REAL",
        DbValue::Blob(_) => "BLOB",
        DbValue::Text(_) | DbValue::Null => "TEXT",
    }
}

/// Auto-materialize (and additively evolve) the physical table for a write, so a
/// plugin never declares, creates, or SQLs a table — core owns all DDL. On the
/// first write the table is created with one column per row key (storage class
/// inferred from the value); on later writes any new row key is added as a
/// nullable column (additive only — never drops or retypes).
///
/// The primary key is the conventional `name` column when present (every
/// `endpoint_resource!` registry keys on it, and `INSERT OR REPLACE` upserts
/// need a PK to converge), else the first column. Every identifier is validated
/// before it reaches the derived DDL, so this can never carry injected SQL — the
/// plugin supplies typed data, not schema.
fn ensure_table_for_row(conn: &Connection, physical: &str, row: &DbRow) -> Result<()> {
    let existing = existing_columns(conn, physical)?;
    if existing.is_empty() {
        let pk = if row.contains_key("name") {
            "name"
        } else {
            // Non-empty: write_row bails on an empty row before calling this.
            row.keys().next().expect("non-empty row").as_str()
        };
        let mut defs = Vec::with_capacity(row.len());
        for (k, v) in row {
            validate_ident("column", k)?;
            let ty = sql_type_for(v);
            if k == pk {
                defs.push(format!("\"{k}\" {ty} PRIMARY KEY"));
            } else {
                defs.push(format!("\"{k}\" {ty}"));
            }
        }
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS \"{physical}\" ({});",
            defs.join(", ")
        ))?;
    } else {
        let have: std::collections::HashSet<&str> =
            existing.iter().map(|(n, _)| n.as_str()).collect();
        for (k, v) in row {
            if !have.contains(k.as_str()) {
                validate_ident("column", k)?;
                let ty = sql_type_for(v);
                conn.execute_batch(&format!(
                    "ALTER TABLE \"{physical}\" ADD COLUMN \"{k}\" {ty};"
                ))?;
            }
        }
    }
    Ok(())
}

fn write_row(
    conn: &Connection,
    namespace: &str,
    table: &str,
    row: &DbRow,
    replace: bool,
) -> Result<DbReply> {
    let physical = resolve_op_table(namespace, table)?;
    if row.is_empty() {
        bail!("write to `{table}` has no columns");
    }
    // Core owns every table. A plugin never ships DDL — it just writes rows, and
    // core materializes (and additively evolves) the backing table from them.
    ensure_table_for_row(conn, &physical, row)?;
    let mut cols = Vec::new();
    let mut placeholders = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    for (i, (k, v)) in row.iter().enumerate() {
        validate_ident("column", k)?;
        cols.push(format!("\"{k}\""));
        placeholders.push(format!("?{}", i + 1));
        vals.push(to_sql(v));
    }
    let verb = if replace {
        "INSERT OR REPLACE"
    } else {
        "INSERT"
    };
    let sql = format!(
        "{verb} INTO \"{physical}\" ({}) VALUES ({})",
        cols.join(", "),
        placeholders.join(", ")
    );
    let n = conn.execute(&sql, rusqlite::params_from_iter(vals.iter()))?;
    // Re-creating a previously-deleted key on a replicated endpoint table:
    // supersede any command-log delete op so the row is not swept on the next
    // sync (no-op when no tombstone op exists — keeps the log sparse).
    if n > 0
        && crate::replicate::is_registered(&physical)
        && let Some(pk_col) = table_pk_col(conn, &physical)
        && let Some(key) = row.get(&pk_col).and_then(dbvalue_to_key)
        && let Err(e) = crate::replication_ops::note_write(
            conn,
            &physical,
            &pk_col,
            &key,
            utils::time::now_millis_since_epoch(),
        )
    {
        tracing::warn!("[replication_ops] note_write {physical} failed: {e}");
    }
    // Origin write to a replicated table: invalidate the content-root memo for
    // this entity and wake push-on-write. Without this, the memoized root stays
    // stale and the divergence check reports in_sync while the change never
    // propagates (silent missed-sync). The op-log `note_write` above only
    // touches the `replication_ops` entity, not this endpoint's own root.
    if n > 0
        && let Some(entity) = crate::replicate::registered_entity(&physical)
    {
        crate::replicate::notify_write(entity);
    }
    Ok(DbReply {
        rows: Vec::new(),
        affected: n as u64,
    })
}

fn update_row(
    conn: &Connection,
    namespace: &str,
    table: &str,
    key_col: &str,
    row: &DbRow,
) -> Result<DbReply> {
    let physical = resolve_op_table(namespace, table)?;
    validate_ident("column", key_col)?;
    let key_val = row
        .get(key_col)
        .ok_or_else(|| anyhow::anyhow!("update of `{table}` missing key column `{key_col}`"))?;
    let mut sets = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    let mut idx = 1;
    for (k, v) in row.iter() {
        if k == key_col {
            continue;
        }
        validate_ident("column", k)?;
        sets.push(format!("\"{k}\" = ?{idx}"));
        vals.push(to_sql(v));
        idx += 1;
    }
    if sets.is_empty() {
        bail!("update of `{table}` sets no columns");
    }
    vals.push(to_sql(key_val));
    let sql = format!(
        "UPDATE \"{physical}\" SET {} WHERE \"{key_col}\" = ?{idx}",
        sets.join(", ")
    );
    let n = conn.execute(&sql, rusqlite::params_from_iter(vals.iter()))?;
    // Origin update to a replicated table — invalidate the content-root memo +
    // wake push-on-write (this path previously did neither, the most severe of
    // the memo-staleness gaps).
    if n > 0
        && let Some(entity) = crate::replicate::registered_entity(&physical)
    {
        crate::replicate::notify_write(entity);
    }
    Ok(DbReply {
        rows: Vec::new(),
        affected: n as u64,
    })
}

/// A `DbValue` rendered as the natural-key string the command-log stores.
/// Endpoint PKs are TEXT; the numeric/bool cases keep it total. Null/Blob keys
/// are not meaningful natural keys, so they yield `None` (op not recorded).
fn dbvalue_to_key(v: &DbValue) -> Option<String> {
    match v {
        DbValue::Text(s) => Some(s.clone()),
        DbValue::Int(i) => Some(i.to_string()),
        DbValue::Real(f) => Some(f.to_string()),
        DbValue::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
        DbValue::Null | DbValue::Blob(_) => None,
    }
}

/// The single-column primary key of `physical`, if it has one. Used to key a
/// command-log op for a replicated endpoint write. `physical` is already a
/// validated identifier (via [`resolve_op_table`]).
fn table_pk_col(conn: &Connection, physical: &str) -> Option<String> {
    conn.query_row(
        &format!("SELECT name FROM pragma_table_info('{physical}') WHERE pk = 1 LIMIT 1"),
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

/// Execute one typed plugin CRUD op on `conn` (core's single pooled
/// connection). The whole DB capability a plugin has — every identifier is
/// validated and every table resolved into the plugin's `plug__<ns>__` space.
pub fn exec_db_op(conn: &Connection, op: &DbOp) -> Result<DbReply> {
    match op {
        DbOp::List { namespace, table } => {
            let physical = resolve_op_table(namespace, table)?;
            // Core owns every table and materializes it on first write. Reading a
            // table that has never been written yet yields [], consistent with the
            // write-side auto-materialize.
            if existing_columns(conn, &physical)?.is_empty() {
                return Ok(DbReply {
                    rows: Vec::new(),
                    affected: 0,
                });
            }
            let mut stmt = conn.prepare(&format!("SELECT * FROM \"{physical}\""))?;
            let rows = collect_rows(&mut stmt, [])?;
            Ok(DbReply { rows, affected: 0 })
        }
        DbOp::Get {
            namespace,
            table,
            key_col,
            key,
        } => {
            let physical = resolve_op_table(namespace, table)?;
            validate_ident("column", key_col)?;
            if existing_columns(conn, &physical)?.is_empty() {
                return Ok(DbReply {
                    rows: Vec::new(),
                    affected: 0,
                });
            }
            let mut stmt = conn.prepare(&format!(
                "SELECT * FROM \"{physical}\" WHERE \"{key_col}\" = ?1"
            ))?;
            let rows = collect_rows(&mut stmt, rusqlite::params![key])?;
            Ok(DbReply { rows, affected: 0 })
        }
        DbOp::Insert {
            namespace,
            table,
            row,
        } => write_row(conn, namespace, table, row, false),
        DbOp::Upsert {
            namespace,
            table,
            row,
        } => write_row(conn, namespace, table, row, true),
        DbOp::Update {
            namespace,
            table,
            key_col,
            row,
        } => update_row(conn, namespace, table, key_col, row),
        DbOp::Delete {
            namespace,
            table,
            key_col,
            key,
        } => {
            let physical = resolve_op_table(namespace, table)?;
            validate_ident("column", key_col)?;
            let n = conn.execute(
                &format!("DELETE FROM \"{physical}\" WHERE \"{key_col}\" = ?1"),
                rusqlite::params![key],
            )?;
            // Command-log the deletion for replicated endpoint tables so it
            // propagates and cannot be resurrected by a peer (see replication_ops).
            if n > 0
                && crate::replicate::is_registered(&physical)
                && let Err(e) = crate::replication_ops::note_delete(
                    conn,
                    &physical,
                    key_col,
                    key,
                    utils::time::now_millis_since_epoch(),
                )
            {
                tracing::warn!("[replication_ops] note_delete {physical} failed: {e}");
            }
            // Origin delete on a replicated table — invalidate the content-root
            // memo + wake push-on-write for the entity itself. `note_delete`
            // above only propagates the tombstone via the `replication_ops`
            // entity; the endpoint's own root would otherwise stay stale.
            if n > 0
                && let Some(entity) = crate::replicate::registered_entity(&physical)
            {
                crate::replicate::notify_write(entity);
            }
            Ok(DbReply {
                rows: Vec::new(),
                affected: n as u64,
            })
        }
    }
}

/// Run a plugin CRUD op on core's **single shared pooled connection** — the
/// entry point the loader binds into each plugin's `set_host` channel. Using
/// the one pooled connection (never a fresh `open_default`) is what removes the
/// SHMOPEN 5898 race entirely.
pub fn exec_db_op_pooled(op: &DbOp) -> Result<DbReply> {
    crate::pool::with_pooled_or_open(|conn| exec_db_op(conn, op))
}

#[cfg(test)]
mod exec_db_op_tests {
    use super::*;
    use plugin_abi::{DbOp, DbValue};

    // A registry-style table like `endpoint_resource!` creates (empty namespace
    // = literal table name, the core-migrated case).
    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE proxmox_endpoints (
                name TEXT PRIMARY KEY,
                base_url TEXT,
                insecure INTEGER NOT NULL DEFAULT 0,
                routes TEXT NOT NULL DEFAULT '[]',
                enabled INTEGER NOT NULL DEFAULT 1
            )",
        )
        .unwrap();
        conn
    }

    fn row(name: &str, url: &str, insecure: bool) -> DbRow {
        let mut m = DbRow::new();
        m.insert("name".into(), DbValue::Text(name.into()));
        m.insert("base_url".into(), DbValue::Text(url.into()));
        m.insert("insecure".into(), DbValue::Bool(insecure));
        m.insert("routes".into(), DbValue::Text("[]".into()));
        m.insert("enabled".into(), DbValue::Bool(true));
        m
    }

    // The shared, core-migrated `endpoints` table (mirrors apply_schema). The
    // derive's shared mode writes provider-tagged rows here and scopes reads to
    // its own provider client-side.
    fn setup_endpoints() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE endpoints (
                id             TEXT PRIMARY KEY,
                provider       TEXT NOT NULL,
                name           TEXT NOT NULL,
                routes         TEXT NOT NULL DEFAULT '[]',
                enabled        INTEGER NOT NULL DEFAULT 1,
                auth_principal TEXT,
                insecure       INTEGER,
                created_at     TEXT,
                updated_at     INTEGER,
                UNIQUE(provider, name)
            )",
        )
        .unwrap();
        conn
    }

    fn endpoints_row(id: &str, provider: &str, name: &str) -> DbRow {
        let mut m = DbRow::new();
        m.insert("id".into(), DbValue::Text(id.into()));
        m.insert("provider".into(), DbValue::Text(provider.into()));
        m.insert("name".into(), DbValue::Text(name.into()));
        m.insert("routes".into(), DbValue::Text("[]".into()));
        m.insert("enabled".into(), DbValue::Bool(true));
        m.insert("updated_at".into(), DbValue::Int(1));
        m
    }

    // Shared-mode insert lands a provider-tagged row in `endpoints` keyed by its
    // minted id, and it round-trips on List.
    #[test]
    fn shared_endpoints_insert_lands_provider_tagged_row() {
        let conn = setup_endpoints();
        exec_db_op(
            &conn,
            &DbOp::Insert {
                namespace: String::new(),
                table: "endpoints".into(),
                row: endpoints_row("id-prox-1", "proxmox", "frigg"),
            },
        )
        .unwrap();
        let l = exec_db_op(
            &conn,
            &DbOp::List {
                namespace: String::new(),
                table: "endpoints".into(),
            },
        )
        .unwrap();
        assert_eq!(l.rows.len(), 1);
        assert_eq!(
            l.rows[0].get("provider"),
            Some(&DbValue::Text("proxmox".into()))
        );
        assert_eq!(
            l.rows[0].get("id"),
            Some(&DbValue::Text("id-prox-1".into()))
        );
    }

    // Two providers with the SAME endpoint name coexist as distinct rows — the
    // whole point of the (provider, name) uniqueness + shared table.
    #[test]
    fn shared_endpoints_two_providers_same_name_coexist() {
        let conn = setup_endpoints();
        exec_db_op(
            &conn,
            &DbOp::Insert {
                namespace: String::new(),
                table: "endpoints".into(),
                row: endpoints_row("id-a", "proxmox", "frigg"),
            },
        )
        .unwrap();
        exec_db_op(
            &conn,
            &DbOp::Insert {
                namespace: String::new(),
                table: "endpoints".into(),
                row: endpoints_row("id-b", "docker", "frigg"),
            },
        )
        .unwrap();
        let l = exec_db_op(
            &conn,
            &DbOp::List {
                namespace: String::new(),
                table: "endpoints".into(),
            },
        )
        .unwrap();
        assert_eq!(l.rows.len(), 2, "same name under two providers = two rows");
    }

    // Update and Delete key by the minted `id` PK (not name), which is how the
    // derive's shared-mode CRUD resolves rows.
    #[test]
    fn shared_endpoints_update_and_delete_by_id() {
        let conn = setup_endpoints();
        exec_db_op(
            &conn,
            &DbOp::Insert {
                namespace: String::new(),
                table: "endpoints".into(),
                row: endpoints_row("id-x", "proxmox", "thor"),
            },
        )
        .unwrap();

        let mut upd = endpoints_row("id-x", "proxmox", "thor");
        upd.insert("enabled".into(), DbValue::Bool(false));
        let u = exec_db_op(
            &conn,
            &DbOp::Update {
                namespace: String::new(),
                table: "endpoints".into(),
                key_col: "id".into(),
                row: upd,
            },
        )
        .unwrap();
        assert_eq!(u.affected, 1);
        let g = exec_db_op(
            &conn,
            &DbOp::Get {
                namespace: String::new(),
                table: "endpoints".into(),
                key_col: "id".into(),
                key: "id-x".into(),
            },
        )
        .unwrap();
        assert_eq!(g.rows[0].get("enabled"), Some(&DbValue::Int(0)));

        let d = exec_db_op(
            &conn,
            &DbOp::Delete {
                namespace: String::new(),
                table: "endpoints".into(),
                key_col: "id".into(),
                key: "id-x".into(),
            },
        )
        .unwrap();
        assert_eq!(d.affected, 1);
    }

    // Core owns table creation: a plugin (e.g. proxmox's endpoint_resource!)
    // never ships DDL — the first Insert materializes the table, and Upsert
    // converges on the conventional `name` primary key. This is the whole fix
    // for "no such table: proxmox_endpoints".
    #[test]
    fn insert_auto_materializes_table_and_upsert_converges_on_name_pk() {
        // Fresh db, table NOT pre-created (unlike `setup`).
        let conn = Connection::open_in_memory().unwrap();
        let table = "proxmox_endpoints".to_string();

        // First insert creates the table from the row shape.
        let r = exec_db_op(
            &conn,
            &DbOp::Insert {
                namespace: String::new(),
                table: table.clone(),
                row: row("frigg", "https://10.10.10.7:8006", true),
            },
        )
        .unwrap();
        assert_eq!(r.affected, 1);

        // `name` was made the primary key → INSERT OR REPLACE upsert converges
        // in place instead of duplicating.
        exec_db_op(
            &conn,
            &DbOp::Upsert {
                namespace: String::new(),
                table: table.clone(),
                row: row("frigg", "https://10.10.10.7:8006", false),
            },
        )
        .unwrap();
        let l = exec_db_op(
            &conn,
            &DbOp::List {
                namespace: String::new(),
                table: table.clone(),
            },
        )
        .unwrap();
        assert_eq!(l.rows.len(), 1, "upsert must converge on the name PK");
        assert_eq!(l.rows[0].get("insecure"), Some(&DbValue::Int(0)));

        // A later write carrying a new column additively evolves the table.
        let mut evolved = row("thor", "https://10.10.10.8:8006", true);
        evolved.insert("token_id".into(), DbValue::Text("root@pam!orca".into()));
        exec_db_op(
            &conn,
            &DbOp::Insert {
                namespace: String::new(),
                table: table.clone(),
                row: evolved,
            },
        )
        .unwrap();
        let got = exec_db_op(
            &conn,
            &DbOp::Get {
                namespace: String::new(),
                table,
                key_col: "name".into(),
                key: "thor".into(),
            },
        )
        .unwrap();
        assert_eq!(
            got.rows[0].get("token_id"),
            Some(&DbValue::Text("root@pam!orca".into()))
        );
    }

    // Read-side mirror of the auto-materialize contract: List/Get against a
    // table that has never been written must return [] with no error (core owns
    // the table; a plugin may read an endpoint table before its first write).
    #[test]
    fn list_and_get_on_missing_table_return_empty() {
        // Fresh db, table NOT pre-created (unlike `setup`).
        let conn = Connection::open_in_memory().unwrap();
        let table = "proxmox_endpoints".to_string();

        let l = exec_db_op(
            &conn,
            &DbOp::List {
                namespace: String::new(),
                table: table.clone(),
            },
        )
        .expect("List on a missing table must not error");
        assert!(l.rows.is_empty());
        assert_eq!(l.affected, 0);

        let g = exec_db_op(
            &conn,
            &DbOp::Get {
                namespace: String::new(),
                table,
                key_col: "name".into(),
                key: "frigg".into(),
            },
        )
        .expect("Get on a missing table must not error");
        assert!(g.rows.is_empty());
        assert_eq!(g.affected, 0);
    }

    #[test]
    fn insert_get_list_update_delete_roundtrip() {
        let conn = setup();
        let ns = String::new();
        let table = "proxmox_endpoints".to_string();

        // Insert
        let r = exec_db_op(
            &conn,
            &DbOp::Insert {
                namespace: ns.clone(),
                table: table.clone(),
                row: row("host-c", "https://10.0.0.7:8006", true),
            },
        )
        .unwrap();
        assert_eq!(r.affected, 1);

        // Get → one row, values round-trip (bool stored as int comes back Int)
        let g = exec_db_op(
            &conn,
            &DbOp::Get {
                namespace: ns.clone(),
                table: table.clone(),
                key_col: "name".into(),
                key: "host-c".into(),
            },
        )
        .unwrap();
        assert_eq!(g.rows.len(), 1);
        assert_eq!(
            g.rows[0].get("base_url"),
            Some(&DbValue::Text("https://10.0.0.7:8006".into()))
        );
        assert_eq!(g.rows[0].get("insecure"), Some(&DbValue::Int(1)));

        // Insert a second, List returns both
        exec_db_op(
            &conn,
            &DbOp::Insert {
                namespace: ns.clone(),
                table: table.clone(),
                row: row("host-b", "https://10.0.0.9:8006", false),
            },
        )
        .unwrap();
        let l = exec_db_op(
            &conn,
            &DbOp::List {
                namespace: ns.clone(),
                table: table.clone(),
            },
        )
        .unwrap();
        assert_eq!(l.rows.len(), 2);

        // Update host-c's url
        let u = exec_db_op(
            &conn,
            &DbOp::Update {
                namespace: ns.clone(),
                table: table.clone(),
                key_col: "name".into(),
                row: row("host-c", "https://new:8006", true),
            },
        )
        .unwrap();
        assert_eq!(u.affected, 1);
        let g2 = exec_db_op(
            &conn,
            &DbOp::Get {
                namespace: ns.clone(),
                table: table.clone(),
                key_col: "name".into(),
                key: "host-c".into(),
            },
        )
        .unwrap();
        assert_eq!(
            g2.rows[0].get("base_url"),
            Some(&DbValue::Text("https://new:8006".into()))
        );

        // Delete host-b
        let d = exec_db_op(
            &conn,
            &DbOp::Delete {
                namespace: ns.clone(),
                table: table.clone(),
                key_col: "name".into(),
                key: "host-b".into(),
            },
        )
        .unwrap();
        assert_eq!(d.affected, 1);
        let l2 = exec_db_op(
            &conn,
            &DbOp::List {
                namespace: ns.clone(),
                table: table.clone(),
            },
        )
        .unwrap();
        assert_eq!(l2.rows.len(), 1);
    }

    // Regression guard: a write to a mesh-replicated table via the generic CRUD
    // path MUST fire notify_write for that entity, so the content-root memo is
    // invalidated and push-on-write wakes. Before this fix the endpoint write
    // path notified nothing → the memo went stale → silent missed-sync.
    #[test]
    fn write_to_registered_table_notifies_its_entity() {
        let mut rx = crate::replicate::subscribe();
        let conn = Connection::open_in_memory().unwrap();
        // "endpoints" is a registered replicated entity (is_registered is a
        // registry check, independent of schema — the row auto-materializes).
        let mut r = DbRow::new();
        r.insert("id".into(), DbValue::Text("e1".into()));
        r.insert("provider".into(), DbValue::Text("p".into()));
        r.insert("name".into(), DbValue::Text("n".into()));
        exec_db_op(
            &conn,
            &DbOp::Insert {
                namespace: String::new(),
                table: "endpoints".into(),
                row: r,
            },
        )
        .unwrap();
        let mut saw_endpoints = false;
        while let Ok(ent) = rx.try_recv() {
            if ent == "endpoints" {
                saw_endpoints = true;
            }
        }
        assert!(
            saw_endpoints,
            "write to a registered table must notify_write its entity"
        );
    }

    // The converse: a write to a NON-replicated table must NOT notify (it would
    // wake push-on-write for nothing and churn the mesh).
    #[test]
    fn write_to_unregistered_table_does_not_notify() {
        let mut rx = crate::replicate::subscribe();
        let conn = Connection::open_in_memory().unwrap();
        exec_db_op(
            &conn,
            &DbOp::Insert {
                namespace: String::new(),
                table: "plug__adversarial__unreplicated".into(),
                row: row("frigg", "https://x", true),
            },
        )
        .unwrap();
        while let Ok(ent) = rx.try_recv() {
            assert_ne!(
                ent, "plug__adversarial__unreplicated",
                "unregistered table must not notify"
            );
        }
    }

    #[test]
    fn rejects_injection_and_bad_identifiers() {
        let conn = setup();
        // A table name that isn't a plain identifier must be refused, not run.
        let bad = exec_db_op(
            &conn,
            &DbOp::List {
                namespace: String::new(),
                table: "proxmox_endpoints; DROP TABLE proxmox_endpoints".into(),
            },
        );
        assert!(bad.is_err());
    }

    #[test]
    fn namespaced_table_resolves_to_plug_prefix() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE plug__myplugin__data (name TEXT PRIMARY KEY, v TEXT)")
            .unwrap();
        let mut r = DbRow::new();
        r.insert("name".into(), DbValue::Text("k".into()));
        r.insert("v".into(), DbValue::Text("hello".into()));
        exec_db_op(
            &conn,
            &DbOp::Insert {
                namespace: "myplugin".into(),
                table: "data".into(),
                row: r,
            },
        )
        .unwrap();
        let g = exec_db_op(
            &conn,
            &DbOp::Get {
                namespace: "myplugin".into(),
                table: "data".into(),
                key_col: "name".into(),
                key: "k".into(),
            },
        )
        .unwrap();
        assert_eq!(g.rows[0].get("v"), Some(&DbValue::Text("hello".into())));
    }
}
