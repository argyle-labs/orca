use anyhow::{Result, anyhow};
use plugin_abi::{DbOp, DbReply, DbRow, DbValue};

/// Access a plugin has to a core table addressed with the empty namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoreTableAccess {
    /// Shared by all plugins; rows are confined to `provider == principal`.
    SharedByProvider,
    ReadWrite,
    ReadOnly,
    /// Read-only; rows confined to enabled, non-reserved installed plugins.
    InstalledPluginCredentials,
}

/// Default-deny policy for `db.op` with namespace "": a core table is reachable
/// only if listed here, by its owner (`None` = every plugin). Everything else
/// (settings, secrets, tokens, mesh, users, …) is refused.
const CORE_TABLE_POLICY: &[(&str, Option<&str>, CoreTableAccess)] = &[
    ("endpoints", None, CoreTableAccess::SharedByProvider),
    ("ntfy_endpoints", Some("ntfy"), CoreTableAccess::ReadWrite),
    ("mcp_servers", Some("mcp"), CoreTableAccess::ReadWrite),
    ("mcp_tool_mappings", Some("mcp"), CoreTableAccess::ReadWrite),
    ("openapi_specs", Some("mcp"), CoreTableAccess::ReadWrite),
    ("plugins", Some("mcp"), CoreTableAccess::ReadOnly),
    (
        "plugin_credentials",
        Some("mcp"),
        CoreTableAccess::InstalledPluginCredentials,
    ),
];

fn core_table_access(table: &str, principal: &str) -> Option<CoreTableAccess> {
    CORE_TABLE_POLICY
        .iter()
        .find(|(t, owner, _)| *t == table && owner.is_none_or(|o| o == principal))
        .map(|(_, _, access)| *access)
}

fn deny(principal: &str, op: &DbOp, table: &str, why: &str) -> anyhow::Error {
    tracing::warn!(
        target: "plugin",
        plugin = %principal,
        op = %op.kind(),
        namespace = %op.namespace(),
        table = %table,
        "db.op refused: {why}"
    );
    anyhow!(
        "plugin '{principal}' may not db.op/{} on '{table}' (namespace '{}'): {why}",
        op.kind(),
        op.namespace()
    )
}

fn op_table(op: &DbOp) -> &str {
    match op {
        DbOp::List { table, .. }
        | DbOp::Get { table, .. }
        | DbOp::Insert { table, .. }
        | DbOp::Update { table, .. }
        | DbOp::Upsert { table, .. }
        | DbOp::Delete { table, .. } => table,
    }
}

fn row_provider_is(row: &DbRow, principal: &str) -> bool {
    matches!(row.get("provider"), Some(DbValue::Text(p)) if p == principal)
}

/// Every existing row in `table` whose `key_col` equals `key` must belong to
/// `principal` — guards Update/Delete/Upsert from touching a foreign row.
fn existing_rows_owned(
    conn: &crate::Conn,
    table: &str,
    key_col: &str,
    key: &str,
    principal: &str,
) -> Result<bool> {
    let existing = crate::plugin_tables::exec_db_op(
        conn,
        &DbOp::Get {
            namespace: String::new(),
            table: table.to_string(),
            key_col: key_col.to_string(),
            key: key.to_string(),
        },
    )?;
    Ok(existing.rows.iter().all(|r| row_provider_is(r, principal)))
}

/// The key value as a string; `Ok(None)` only when absent. Other types are
/// rejected rather than treated as absent, which would skip the ownership check.
fn key_text(v: Option<&DbValue>) -> Result<Option<String>> {
    match v {
        None => Ok(None),
        Some(DbValue::Text(s)) => Ok(Some(s.clone())),
        Some(DbValue::Int(i)) => Ok(Some(i.to_string())),
        Some(_) => Err(anyhow!("db.op: key value must be text or integer")),
    }
}

fn enabled_plugin_ids(conn: &crate::Conn) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT id FROM plugins WHERE enabled = 1")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Run `op` for `principal` under the namespace policy: its own `plug__`
/// namespace freely, core tables only per [`core_table_access`], nothing else.
pub fn exec_db_op_policed(conn: &crate::Conn, op: &DbOp, principal: &str) -> Result<DbReply> {
    let table = op_table(op);
    if crate::plugin_tables::validate_principal(principal).is_err() {
        return Err(deny(principal, op, table, "invalid principal"));
    }
    let ns = op.namespace();
    if ns == principal {
        return crate::plugin_tables::exec_db_op(conn, op);
    }
    if !ns.is_empty() {
        return Err(deny(principal, op, table, "foreign namespace"));
    }
    let Some(access) = core_table_access(table, principal) else {
        return Err(deny(principal, op, table, "core table not permitted"));
    };
    let is_read = matches!(op, DbOp::List { .. } | DbOp::Get { .. });
    match access {
        CoreTableAccess::ReadWrite => crate::plugin_tables::exec_db_op(conn, op),
        CoreTableAccess::ReadOnly if is_read => crate::plugin_tables::exec_db_op(conn, op),
        CoreTableAccess::ReadOnly => Err(deny(principal, op, table, "table is read-only")),
        CoreTableAccess::InstalledPluginCredentials if is_read => {
            let enabled = enabled_plugin_ids(conn)?;
            let mut reply = crate::plugin_tables::exec_db_op(conn, op)?;
            reply.rows.retain(|r| {
                matches!(r.get("plugin_id"), Some(DbValue::Text(id))
                    if enabled.contains(id) && !contract::plugin::is_reserved_plugin_id(id))
            });
            Ok(reply)
        }
        CoreTableAccess::InstalledPluginCredentials => {
            Err(deny(principal, op, table, "table is read-only"))
        }
        CoreTableAccess::SharedByProvider if is_read => {
            let mut reply = crate::plugin_tables::exec_db_op(conn, op)?;
            reply.rows.retain(|r| row_provider_is(r, principal));
            Ok(reply)
        }
        CoreTableAccess::SharedByProvider => {
            // IMMEDIATE so no other writer can reassign the row between the
            // ownership check and the write. Dropping `tx` (error or panic)
            // rolls back.
            let tx = rusqlite::Transaction::new_unchecked(
                conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let reply = exec_shared_write(&tx, op, table, principal)?;
            tx.commit()?;
            Ok(reply)
        }
    }
}

/// A write to a provider-shared table. A foreign row yields `affected: 0`,
/// indistinguishable from a missing one.
fn exec_shared_write(
    conn: &crate::Conn,
    op: &DbOp,
    table: &str,
    principal: &str,
) -> Result<DbReply> {
    let owned = match op {
        DbOp::List { .. } | DbOp::Get { .. } => true,
        DbOp::Insert { row, .. } | DbOp::Upsert { row, .. } if !row_provider_is(row, principal) => {
            return Err(deny(
                principal,
                op,
                table,
                "row provider must be the caller",
            ));
        }
        DbOp::Insert { .. } => true,
        DbOp::Upsert { row, .. } => match key_text(row.get("id"))? {
            Some(id) => existing_rows_owned(conn, table, "id", &id, principal)?,
            None => true,
        },
        DbOp::Update { key_col, row, .. } => {
            if row.get("provider").is_some() && !row_provider_is(row, principal) {
                return Err(deny(
                    principal,
                    op,
                    table,
                    "row provider must be the caller",
                ));
            }
            let Some(key) = key_text(row.get(key_col))? else {
                return Err(deny(principal, op, table, "update without key value"));
            };
            existing_rows_owned(conn, table, key_col, &key, principal)?
        }
        DbOp::Delete { key_col, key, .. } => {
            existing_rows_owned(conn, table, key_col, key, principal)?
        }
    };
    if !owned {
        return Ok(DbReply::default());
    }
    crate::plugin_tables::exec_db_op(conn, op)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_conn() -> crate::Conn {
        let conn = crate::Conn::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE endpoints (id TEXT PRIMARY KEY, provider TEXT NOT NULL, name TEXT NOT NULL);
             INSERT INTO endpoints VALUES ('a1', 'alpha', 'one'), ('b1', 'beta', 'two');
             CREATE TABLE ntfy_endpoints (name TEXT PRIMARY KEY);
             CREATE TABLE mcp_servers (name TEXT PRIMARY KEY);
             CREATE TABLE plugins (id TEXT PRIMARY KEY, enabled INTEGER NOT NULL);
             INSERT INTO plugins VALUES ('alpha', 1), ('beta', 0), ('orca', 1);
             CREATE TABLE plugin_credentials (name TEXT PRIMARY KEY, plugin_id TEXT, value TEXT);
             INSERT INTO plugin_credentials VALUES
               ('a', 'alpha', 'x'), ('b', 'beta', 'y'), ('o', 'orca', 'z'), ('g', 'ghost', 'w');",
        )
        .unwrap();
        conn
    }

    fn list(table: &str) -> DbOp {
        DbOp::List {
            namespace: String::new(),
            table: table.into(),
        }
    }

    fn count_provider(conn: &crate::Conn, provider: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM endpoints WHERE provider = ?1",
            [provider],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn ep_row(id: &str, provider: &str) -> DbRow {
        let mut r = DbRow::new();
        r.insert("id".into(), DbValue::Text(id.into()));
        r.insert("provider".into(), DbValue::Text(provider.into()));
        r.insert("name".into(), DbValue::Text("n".into()));
        r
    }

    fn name_row() -> DbRow {
        let mut r = DbRow::new();
        r.insert("name".into(), DbValue::Text("x".into()));
        r
    }

    #[test]
    fn db_op_refuses_settings_read() {
        let conn = policy_conn();
        let err = exec_db_op_policed(&conn, &list("settings"), "alpha")
            .unwrap_err()
            .to_string();
        assert!(err.contains("'settings'"), "got: {err}");
    }

    #[test]
    fn db_op_refuses_secrets_write() {
        let conn = policy_conn();
        let op = DbOp::Upsert {
            namespace: String::new(),
            table: "secrets".into(),
            row: name_row(),
        };
        assert!(exec_db_op_policed(&conn, &op, "mcp").is_err());
    }

    #[test]
    fn hyphenated_principal_lists_own_endpoints() {
        let conn = policy_conn();
        conn.execute(
            "INSERT INTO endpoints VALUES ('c1', 'calibre-web', 'cw')",
            [],
        )
        .unwrap();
        let reply = exec_db_op_policed(&conn, &list("endpoints"), "calibre-web").unwrap();
        assert_eq!(reply.rows.len(), 1);
        assert!(row_provider_is(&reply.rows[0], "calibre-web"));
    }

    #[test]
    fn hyphenated_principal_uses_own_namespace() {
        let conn = policy_conn();
        let mut row = DbRow::new();
        row.insert("id".into(), DbValue::Text("k".into()));
        let op = DbOp::Insert {
            namespace: "calibre-web".into(),
            table: "books".into(),
            row,
        };
        exec_db_op_policed(&conn, &op, "calibre-web").unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM plug__calibre_web__books", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn malformed_principals_refused() {
        let conn = policy_conn();
        for p in ["calibre_web", "-a", "a-", "a--b", "a\"b"] {
            let err = exec_db_op_policed(&conn, &list("endpoints"), p)
                .unwrap_err()
                .to_string();
            assert!(err.contains("invalid principal"), "{p}: {err}");
        }
    }

    #[test]
    fn endpoints_list_returns_only_own_rows() {
        let conn = policy_conn();
        let reply = exec_db_op_policed(&conn, &list("endpoints"), "alpha").unwrap();
        assert_eq!(reply.rows.len(), 1);
        assert!(row_provider_is(&reply.rows[0], "alpha"));
    }

    #[test]
    fn endpoints_update_and_delete_of_foreign_row_refused() {
        let conn = policy_conn();
        let upd = DbOp::Update {
            namespace: String::new(),
            table: "endpoints".into(),
            key_col: "id".into(),
            row: ep_row("b1", "alpha"),
        };
        assert_eq!(
            exec_db_op_policed(&conn, &upd, "alpha").unwrap().affected,
            0
        );
        let del = DbOp::Delete {
            namespace: String::new(),
            table: "endpoints".into(),
            key_col: "id".into(),
            key: "b1".into(),
        };
        assert_eq!(
            exec_db_op_policed(&conn, &del, "alpha").unwrap().affected,
            0
        );
        let missing = DbOp::Delete {
            namespace: String::new(),
            table: "endpoints".into(),
            key_col: "id".into(),
            key: "zz".into(),
        };
        assert_eq!(
            exec_db_op_policed(&conn, &missing, "alpha")
                .unwrap()
                .affected,
            0
        );
        assert_eq!(count_provider(&conn, "beta"), 1);
        let own = DbOp::Update {
            namespace: String::new(),
            table: "endpoints".into(),
            key_col: "id".into(),
            row: ep_row("a1", "alpha"),
        };
        assert_eq!(
            exec_db_op_policed(&conn, &own, "alpha").unwrap().affected,
            1
        );
    }

    #[test]
    fn endpoints_insert_with_foreign_provider_refused() {
        let conn = policy_conn();
        let op = DbOp::Insert {
            namespace: String::new(),
            table: "endpoints".into(),
            row: ep_row("a2", "beta"),
        };
        assert!(exec_db_op_policed(&conn, &op, "alpha").is_err());
        let hijack = DbOp::Upsert {
            namespace: String::new(),
            table: "endpoints".into(),
            row: ep_row("b1", "alpha"),
        };
        assert_eq!(
            exec_db_op_policed(&conn, &hijack, "alpha")
                .unwrap()
                .affected,
            0
        );
        assert_eq!(count_provider(&conn, "beta"), 1);
    }

    #[test]
    fn ntfy_endpoints_only_for_ntfy() {
        let conn = policy_conn();
        assert!(exec_db_op_policed(&conn, &list("ntfy_endpoints"), "ntfy").is_ok());
        assert!(exec_db_op_policed(&conn, &list("ntfy_endpoints"), "proxmox").is_err());
        assert!(exec_db_op_policed(&conn, &list("proxmox_endpoints"), "proxmox").is_err());
    }

    #[test]
    fn every_policy_table_exists_in_migrated_schema() {
        let conn = crate::testing::test_conn();
        for (table, _, _) in CORE_TABLE_POLICY {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "policy table `{table}` has no migration");
        }
    }

    #[test]
    fn mcp_tables_only_for_mcp() {
        let conn = policy_conn();
        let ins = DbOp::Insert {
            namespace: String::new(),
            table: "mcp_servers".into(),
            row: name_row(),
        };
        assert!(exec_db_op_policed(&conn, &ins, "mcp").is_ok());
        assert!(exec_db_op_policed(&conn, &list("mcp_servers"), "alpha").is_err());
    }

    #[test]
    fn plugin_credentials_read_only_for_mcp() {
        let conn = policy_conn();
        let reply = exec_db_op_policed(&conn, &list("plugin_credentials"), "mcp").unwrap();
        let ids: Vec<_> = reply
            .rows
            .iter()
            .map(|r| r.get("plugin_id").cloned())
            .collect();
        assert_eq!(ids, vec![Some(DbValue::Text("alpha".into()))]);
        assert!(exec_db_op_policed(&conn, &list("plugin_credentials"), "alpha").is_err());
        let ins = DbOp::Insert {
            namespace: String::new(),
            table: "plugin_credentials".into(),
            row: name_row(),
        };
        assert!(exec_db_op_policed(&conn, &ins, "mcp").is_err());
    }

    #[test]
    fn foreign_plugin_namespace_refused() {
        let conn = policy_conn();
        let op = DbOp::List {
            namespace: "beta".into(),
            table: "things".into(),
        };
        assert!(exec_db_op_policed(&conn, &op, "alpha").is_err());
        let via_core = list("plug__beta__things");
        assert!(exec_db_op_policed(&conn, &via_core, "alpha").is_err());
        for ns in ["alph", "alphax", "alpha_", "Alpha", "alpha__x"] {
            let read = DbOp::List {
                namespace: ns.into(),
                table: "things".into(),
            };
            let write = DbOp::Insert {
                namespace: ns.into(),
                table: "things".into(),
                row: name_row(),
            };
            assert!(exec_db_op_policed(&conn, &read, "alpha").is_err(), "{ns}");
            assert!(exec_db_op_policed(&conn, &write, "alpha").is_err(), "{ns}");
        }
        for t in ["plug__alphax__things", "plug__alpha__things"] {
            assert!(exec_db_op_policed(&conn, &list(t), "alpha").is_err(), "{t}");
        }
    }

    #[test]
    fn invalid_principal_refused() {
        let conn = policy_conn();
        for p in ["", "Bad", "a__b"] {
            let op = DbOp::List {
                namespace: p.into(),
                table: "things".into(),
            };
            assert!(exec_db_op_policed(&conn, &op, p).is_err(), "{p:?}");
        }
    }

    #[test]
    fn core_table_write_never_runs_ddl() {
        let conn = policy_conn();
        let mut row = ep_row("a2", "alpha");
        row.insert("evil".into(), DbValue::Text("x".into()));
        let op = DbOp::Insert {
            namespace: String::new(),
            table: "endpoints".into(),
            row,
        };
        let err = exec_db_op_policed(&conn, &op, "alpha")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown column"), "got: {err}");
        let cols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('endpoints') WHERE name = 'evil'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cols, 0);
        // A missing core table is not created either.
        let ins = DbOp::Insert {
            namespace: String::new(),
            table: "mcp_tool_mappings".into(),
            row: name_row(),
        };
        assert!(exec_db_op_policed(&conn, &ins, "mcp").is_err());
    }

    #[test]
    fn non_text_key_rejected() {
        let conn = policy_conn();
        let mut row = ep_row("b1", "alpha");
        row.insert("id".into(), DbValue::Blob(b"b1".to_vec()));
        let ups = DbOp::Upsert {
            namespace: String::new(),
            table: "endpoints".into(),
            row: row.clone(),
        };
        assert!(exec_db_op_policed(&conn, &ups, "alpha").is_err());
        let upd = DbOp::Update {
            namespace: String::new(),
            table: "endpoints".into(),
            key_col: "id".into(),
            row,
        };
        assert!(exec_db_op_policed(&conn, &upd, "alpha").is_err());
    }

    #[test]
    fn shared_write_leaves_no_open_transaction() {
        let conn = policy_conn();
        let bad = DbOp::Insert {
            namespace: String::new(),
            table: "endpoints".into(),
            row: ep_row("a1", "alpha"),
        };
        assert!(exec_db_op_policed(&conn, &bad, "alpha").is_err());
        assert!(conn.is_autocommit());
        let mut extra = ep_row("a4", "alpha");
        extra.insert("evil".into(), DbValue::Text("x".into()));
        let failing = DbOp::Upsert {
            namespace: String::new(),
            table: "endpoints".into(),
            row: extra,
        };
        assert!(exec_db_op_policed(&conn, &failing, "alpha").is_err());
        assert!(conn.is_autocommit());
        assert_eq!(count_provider(&conn, "alpha"), 1);
        let ok = DbOp::Insert {
            namespace: String::new(),
            table: "endpoints".into(),
            row: ep_row("a3", "alpha"),
        };
        assert_eq!(exec_db_op_policed(&conn, &ok, "alpha").unwrap().affected, 1);
        assert!(conn.is_autocommit());
    }
}
