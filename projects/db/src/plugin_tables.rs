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

fn write_row(
    conn: &Connection,
    namespace: &str,
    table: &str,
    row: &DbRow,
    replace: bool,
) -> Result<DbReply> {
    let physical = physical_table_name(namespace, table)?;
    if row.is_empty() {
        bail!("write to `{table}` has no columns");
    }
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
    let physical = physical_table_name(namespace, table)?;
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
    Ok(DbReply {
        rows: Vec::new(),
        affected: n as u64,
    })
}

/// Execute one typed plugin CRUD op on `conn` (core's single pooled
/// connection). The whole DB capability a plugin has — every identifier is
/// validated and every table resolved into the plugin's `plug__<ns>__` space.
pub fn exec_db_op(conn: &Connection, op: &DbOp) -> Result<DbReply> {
    match op {
        DbOp::List { namespace, table } => {
            let physical = physical_table_name(namespace, table)?;
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
            let physical = physical_table_name(namespace, table)?;
            validate_ident("column", key_col)?;
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
            let physical = physical_table_name(namespace, table)?;
            validate_ident("column", key_col)?;
            let n = conn.execute(
                &format!("DELETE FROM \"{physical}\" WHERE \"{key_col}\" = ?1"),
                rusqlite::params![key],
            )?;
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
