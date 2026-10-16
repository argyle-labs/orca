use anyhow::{Result, bail};
use db::plugin_tables::{physical_table_name, validate_ident};
use plugin_abi::SchemaDecl;
use rusqlite::Connection;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SecretColumnMigration {
    pub moved: usize,
    pub already_stored: usize,
    pub names: Vec<String>,
}

pub fn migrate_secret_columns(
    conn: &Connection,
    decl: &SchemaDecl,
    dry_run: bool,
) -> Result<SecretColumnMigration> {
    let mut report = SecretColumnMigration::default();
    let tx = conn.unchecked_transaction()?;
    for table in &decl.tables {
        let secret_cols: Vec<_> = table.columns.iter().filter(|c| c.secret).collect();
        if secret_cols.is_empty() {
            continue;
        }
        let Some(pk) = table.columns.iter().find(|c| c.primary_key) else {
            bail!(
                "table `{}` declares secret columns but no primary key",
                table.table
            );
        };
        validate_ident("column", &pk.name)?;
        let physical = physical_table_name(&decl.namespace, &table.table)?;
        for col in secret_cols {
            validate_ident("column", &col.name)?;
            let rows: Vec<(String, String)> = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT CAST(\"{pk}\" AS TEXT), \"{c}\" FROM \"{physical}\" \
                     WHERE \"{c}\" IS NOT NULL AND \"{c}\" != ''",
                    pk = pk.name,
                    c = col.name
                ))?;
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?
            };
            let cleared = if col.not_null { "''" } else { "NULL" };
            for (instance, value) in rows {
                let name = format!("{}.{instance}.{}", decl.namespace, col.name);
                let stored = crate::get(&tx, &name)?.is_some();
                if stored {
                    report.already_stored += 1;
                } else {
                    report.moved += 1;
                }
                if !dry_run {
                    if !stored {
                        crate::upsert(&tx, &name, crate::BACKEND_INLINE, "", None)?;
                        crate::write_inline_value(&tx, &name, &value)?;
                    }
                    tx.execute(
                        &format!(
                            "UPDATE \"{physical}\" SET \"{c}\" = {cleared} \
                             WHERE CAST(\"{pk}\" AS TEXT) = ?1",
                            c = col.name,
                            pk = pk.name
                        ),
                        [&instance],
                    )?;
                }
                report.names.push(name);
            }
        }
    }
    tx.commit()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::testing::test_conn;
    use plugin_abi::{ColumnDef, TableDef};

    fn col(name: &str, primary_key: bool, secret: bool) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            sql_type: "TEXT".into(),
            primary_key,
            secret,
            ..Default::default()
        }
    }

    fn decl() -> SchemaDecl {
        SchemaDecl {
            namespace: "acme".into(),
            tables: vec![TableDef {
                table: "endpoints".into(),
                columns: vec![col("name", true, false), col("token", false, true)],
                indexes: vec![],
            }],
        }
    }

    fn seeded() -> Connection {
        let conn = test_conn();
        db::plugin_tables::apply_decl(&conn, &decl()).unwrap();
        conn.execute_batch(
            "INSERT INTO plug__acme__endpoints (name, token) VALUES \
             ('a', 'tok-a'), ('b', ''), ('c', NULL)",
        )
        .unwrap();
        conn
    }

    fn column(conn: &Connection, name: &str) -> Option<String> {
        conn.query_row(
            "SELECT token FROM plug__acme__endpoints WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn moves_value_into_store_and_nulls_column() {
        let conn = seeded();
        let r = migrate_secret_columns(&conn, &decl(), false).unwrap();
        assert_eq!(r.moved, 1);
        assert_eq!(r.names, vec!["acme.a.token".to_string()]);
        assert_eq!(
            crate::read_inline_value(&conn, "acme.a.token")
                .unwrap()
                .as_deref(),
            Some("tok-a")
        );
        assert_eq!(column(&conn, "a"), None);
    }

    #[test]
    fn is_idempotent() {
        let conn = seeded();
        migrate_secret_columns(&conn, &decl(), false).unwrap();
        let again = migrate_secret_columns(&conn, &decl(), false).unwrap();
        assert_eq!(again, SecretColumnMigration::default());
    }

    #[test]
    fn store_value_wins_over_column() {
        let conn = seeded();
        crate::upsert(&conn, "acme.a.token", "inline", "", None).unwrap();
        crate::write_inline_value(&conn, "acme.a.token", "kept").unwrap();
        let r = migrate_secret_columns(&conn, &decl(), false).unwrap();
        assert_eq!((r.moved, r.already_stored), (0, 1));
        assert_eq!(
            crate::read_inline_value(&conn, "acme.a.token")
                .unwrap()
                .as_deref(),
            Some("kept")
        );
        assert_eq!(column(&conn, "a"), None);
    }

    #[test]
    fn dry_run_reports_without_writing() {
        let conn = seeded();
        let r = migrate_secret_columns(&conn, &decl(), true).unwrap();
        assert_eq!(r.moved, 1);
        assert_eq!(r.names, vec!["acme.a.token".to_string()]);
        assert!(crate::get(&conn, "acme.a.token").unwrap().is_none());
        assert_eq!(column(&conn, "a").as_deref(), Some("tok-a"));
    }
}
