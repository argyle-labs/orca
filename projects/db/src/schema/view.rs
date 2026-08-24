//! Multi-tab schema view — connects to every configured DB (MySQL native,
//! MySQL-in-docker, Postgres native, SQLite native) and assembles the
//! `tabs[]` payload that drives the schema UI + the
//! `namespace.schema.view.detail` OrcaTool.

use std::collections::HashMap;

use utils::path::expand_tilde;

use mysql_async::Pool;
use mysql_async::prelude::Queryable;
use serde::Deserialize;

use crate::schema::types::{
    GetSchemaOutput, SchemaColumn, SchemaDomain, SchemaForeignKey, SchemaTab, SchemaTableInfo,
};

/// Failure modes for [`build_schema_response`]. The HTTP handler maps these
/// to specific status codes (404 / 500); CLI/MCP callers see the message.
#[derive(Debug)]
pub enum SchemaBuildError {
    /// No databases configured in orca.db (and orca.toml fallback was empty).
    NoDatabases,
    /// At least one DB was configured but every connection failed.
    AllFailed(String),
}

impl std::fmt::Display for SchemaBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDatabases => f.write_str(
                "No databases configured — use `orca schema add` or POST /api/schema/databases",
            ),
            Self::AllFailed(msg) => write!(f, "All databases failed: {msg}"),
        }
    }
}

impl std::error::Error for SchemaBuildError {}

// ── Schema database config ──────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct DbConfig {
    pub name: String,
    pub driver: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    pub container: Option<String>,
    pub domains_file: Option<String>,
}

impl From<db::schema_databases::SchemaDbRow> for DbConfig {
    fn from(r: db::schema_databases::SchemaDbRow) -> Self {
        let default_port = if r.driver == "postgres" { 5432 } else { 3306 };
        DbConfig {
            name: r.name,
            driver: r.driver,
            host: r.host.unwrap_or_default(),
            port: r.port.unwrap_or(default_port),
            user: r.user,
            password: r.password,
            database: r.database,
            container: r.container,
            domains_file: r.domains_file,
        }
    }
}

// ── TOML migration types (used only for one-shot import) ────────────────────

#[derive(Deserialize, Clone)]
struct TomlDbConfig {
    name: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    port: u16,
    user: String,
    password: String,
    database: String,
    container: Option<String>,
    #[serde(alias = "domainsFile")]
    domains_file: Option<String>,
}

#[derive(Deserialize, Default)]
struct TomlSchemaSection {
    databases: Vec<TomlDbConfig>,
}

#[derive(Deserialize, Default)]
struct TomlOrcaConfig {
    schema: Option<TomlSchemaSection>,
}

/// Load schema DB configs from orca.db. If the table is empty, attempt a
/// one-shot migration from orca.toml (idempotent: INSERT OR IGNORE).
fn load_db_configs() -> Vec<DbConfig> {
    let Ok(conn) = db::open_default() else {
        return vec![];
    };

    if let Ok(rows) = db::schema_databases::list(&conn)
        && !rows.is_empty()
    {
        return rows.into_iter().map(DbConfig::from).collect();
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let toml_path =
        std::env::var("ORCA_CONFIG").unwrap_or_else(|_| format!("{home}/.orca/orca.toml"));

    if let Ok(raw) = std::fs::read_to_string(&toml_path)
        && let Ok(cfg) = toml::from_str::<TomlOrcaConfig>(&raw)
    {
        let dbs = cfg.schema.map(|s| s.databases).unwrap_or_default();
        for d in &dbs {
            let row = db::schema_databases::SchemaDbRow {
                name: d.name.clone(),
                driver: "mysql".to_string(),
                host: if d.host.is_empty() {
                    None
                } else {
                    Some(d.host.clone())
                },
                port: if d.port == 0 { None } else { Some(d.port) },
                user: d.user.clone(),
                password: d.password.clone(),
                database: d.database.clone(),
                container: d.container.clone(),
                domains_file: d.domains_file.clone(),
                enabled: true,
            };
            _ = db::schema_databases::upsert(&conn, &row);
        }
        if !dbs.is_empty() {
            return dbs
                .into_iter()
                .map(|d| DbConfig {
                    name: d.name,
                    driver: "mysql".to_string(),
                    host: d.host,
                    port: d.port,
                    user: d.user,
                    password: d.password,
                    database: d.database,
                    container: d.container,
                    domains_file: d.domains_file,
                })
                .collect();
        }
    }

    vec![]
}

pub fn load_domains(domains_file: &Option<String>) -> Vec<SchemaDomain> {
    let Some(path) = domains_file else {
        return Vec::new();
    };
    let expanded = expand_tilde(path);
    std::fs::read_to_string(&expanded)
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<SchemaDomain>>(&raw).ok())
        .unwrap_or_default()
}

/// Pure builder for the multi-tab schema response. Shared by the HTTP
/// handler and the `namespace.schema.view.detail` OrcaTool.
pub async fn build_schema_response() -> Result<GetSchemaOutput, SchemaBuildError> {
    let configs = load_db_configs();
    if configs.is_empty() {
        return Err(SchemaBuildError::NoDatabases);
    }

    let mut tabs = Vec::new();
    let mut errors = Vec::new();

    for cfg in &configs {
        match query_database(cfg).await {
            Ok(tab) => tabs.push(tab),
            Err(e) => errors.push(format!("{}: {e}", cfg.name)),
        }
    }

    if tabs.is_empty() {
        return Err(SchemaBuildError::AllFailed(errors.join("; ")));
    }

    let show_tabs = tabs.len() > 1;
    let errors_opt = if errors.is_empty() {
        None
    } else {
        Some(errors)
    };
    Ok(GetSchemaOutput {
        tabs,
        show_tabs,
        errors: errors_opt,
    })
}

/// Pure builder for the flattened schema-domains array.
pub fn build_schema_domains() -> Vec<SchemaDomain> {
    let configs = load_db_configs();
    let mut all: Vec<SchemaDomain> = Vec::new();
    for cfg in &configs {
        all.extend(load_domains(&cfg.domains_file));
    }
    all
}

async fn query_database(cfg: &DbConfig) -> anyhow::Result<SchemaTab> {
    match cfg.driver.as_str() {
        "postgres" => query_database_postgres(cfg).await,
        "sqlite" => query_database_sqlite(cfg).await,
        _ => match cfg.container.as_deref() {
            Some(container) => query_database_docker(cfg, container).await,
            None => query_database_mysql_native(cfg).await,
        },
    }
}

async fn query_database_mysql_native(cfg: &DbConfig) -> anyhow::Result<SchemaTab> {
    let opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname(cfg.host.clone())
        .tcp_port(cfg.port)
        .user(Some(cfg.user.clone()))
        .pass(Some(cfg.password.clone()))
        .db_name(Some(cfg.database.clone()));

    let pool = Pool::new(opts);
    let mut conn = pool.get_conn().await?;

    let db = &cfg.database;

    let raw_tables: Vec<(String, Option<String>)> = conn
        .query(format!(
            "SELECT TABLE_NAME, TABLE_COMMENT FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA='{db}' AND TABLE_TYPE='BASE TABLE' ORDER BY TABLE_NAME"
        ))
        .await?;

    let raw_cols: Vec<(String, String, String, String, String, String)> = conn
        .query(format!(
            "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, IS_NULLABLE, COLUMN_KEY, EXTRA \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA='{db}' ORDER BY TABLE_NAME, ORDINAL_POSITION"
        ))
        .await?;

    let raw_fks: Vec<(String, String, String, String)> = conn
        .query(format!(
            "SELECT TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
             FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_SCHEMA='{db}' AND REFERENCED_TABLE_NAME IS NOT NULL"
        ))
        .await?;

    drop(conn);
    pool.disconnect().await.ok();

    Ok(build_schema_value(cfg, raw_tables, raw_cols, raw_fks))
}

async fn query_database_docker(cfg: &DbConfig, container: &str) -> anyhow::Result<SchemaTab> {
    let db = &cfg.database;
    let pass_arg = format!("-p{}", cfg.password);
    let base_args: Vec<String> = vec![
        "exec".into(),
        container.into(),
        "mysql".into(),
        "-u".into(),
        cfg.user.clone(),
        pass_arg,
        cfg.database.clone(),
        "--batch".into(),
        "--silent".into(),
    ];

    let run = |sql: String| {
        let mut args = base_args.clone();
        args.extend(["-e".into(), sql]);
        async move {
            let out = tokio::process::Command::new("docker")
                .args(&args)
                .output()
                .await?;
            if !out.status.success() {
                anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
            }
            anyhow::Ok(String::from_utf8_lossy(&out.stdout).to_string())
        }
    };

    let tables_sql = format!(
        "SELECT TABLE_NAME, TABLE_COMMENT FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA='{db}' AND TABLE_TYPE='BASE TABLE' ORDER BY TABLE_NAME"
    );
    let cols_sql = format!(
        "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, IS_NULLABLE, COLUMN_KEY, EXTRA \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA='{db}' ORDER BY TABLE_NAME, ORDINAL_POSITION"
    );
    let fk_sql = format!(
        "SELECT TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
         FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_SCHEMA='{db}' AND REFERENCED_TABLE_NAME IS NOT NULL"
    );

    let (tables_tsv, cols_tsv, fk_tsv) =
        tokio::try_join!(run(tables_sql), run(cols_sql), run(fk_sql))?;

    let raw_tables: Vec<(String, Option<String>)> = tsv_rows(&tables_tsv, 2)
        .into_iter()
        .map(|mut r| (r.remove(0), r.into_iter().next().filter(|s| !s.is_empty())))
        .collect();

    let raw_cols: Vec<(String, String, String, String, String, String)> = tsv_rows(&cols_tsv, 6)
        .into_iter()
        .map(|mut r| {
            let mut g = || r.remove(0);
            (g(), g(), g(), g(), g(), g())
        })
        .collect();

    let raw_fks: Vec<(String, String, String, String)> = tsv_rows(&fk_tsv, 4)
        .into_iter()
        .map(|mut r| {
            let mut g = || r.remove(0);
            (g(), g(), g(), g())
        })
        .collect();

    Ok(build_schema_value(cfg, raw_tables, raw_cols, raw_fks))
}

async fn query_database_postgres(cfg: &DbConfig) -> anyhow::Result<SchemaTab> {
    let conn_str = format!(
        "host={} port={} user={} password={} dbname={}",
        cfg.host, cfg.port, cfg.user, cfg.password, cfg.database
    );
    let (client, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls).await?;
    tokio::spawn(connection);

    let tables_rows = client
        .query(
            "SELECT table_name, '' FROM information_schema.tables \
             WHERE table_schema='public' AND table_type='BASE TABLE' ORDER BY table_name",
            &[],
        )
        .await?;

    let cols_rows = client
        .query(
            "SELECT table_name, column_name, data_type, is_nullable, '', '' \
             FROM information_schema.columns \
             WHERE table_schema='public' ORDER BY table_name, ordinal_position",
            &[],
        )
        .await?;

    let fk_rows = client
        .query(
            "SELECT tc.table_name, kcu.column_name, ccu.table_name, ccu.column_name \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema \
             JOIN information_schema.constraint_column_usage ccu \
               ON ccu.constraint_name = tc.constraint_name AND ccu.table_schema = tc.table_schema \
             WHERE tc.constraint_type = 'FOREIGN KEY' AND tc.table_schema = 'public'",
            &[],
        )
        .await?;

    let raw_tables: Vec<(String, Option<String>)> = tables_rows
        .iter()
        .map(|r| (r.get::<_, String>(0), None))
        .collect();

    let raw_cols: Vec<(String, String, String, String, String, String)> = cols_rows
        .iter()
        .map(|r| {
            (
                r.get::<_, String>(0),
                r.get::<_, String>(1),
                r.get::<_, String>(2),
                r.get::<_, String>(3),
                String::new(),
                String::new(),
            )
        })
        .collect();

    let raw_fks: Vec<(String, String, String, String)> = fk_rows
        .iter()
        .map(|r| {
            (
                r.get::<_, String>(0),
                r.get::<_, String>(1),
                r.get::<_, String>(2),
                r.get::<_, String>(3),
            )
        })
        .collect();

    Ok(build_schema_value(cfg, raw_tables, raw_cols, raw_fks))
}

async fn query_database_sqlite(cfg: &DbConfig) -> anyhow::Result<SchemaTab> {
    let path = cfg.database.clone();
    let cfg_clone = cfg.clone();

    tokio::task::spawn_blocking(move || -> anyhow::Result<SchemaTab> {
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;

        let table_names: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };

        let raw_tables: Vec<(String, Option<String>)> =
            table_names.iter().map(|n| (n.clone(), None)).collect();

        let mut raw_cols: Vec<(String, String, String, String, String, String)> = Vec::new();
        let mut raw_fks: Vec<(String, String, String, String)> = Vec::new();

        for table in &table_names {
            let cols: Vec<(String, String, String, String, String, String)> = {
                let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
                stmt.query_map([], |r| {
                    Ok((
                        table.clone(),
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        if r.get::<_, i32>(3)? != 0 { "NO".to_string() } else { "YES".to_string() },
                        if r.get::<_, i32>(5)? != 0 { "PRI".to_string() } else { String::new() },
                        String::new(),
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?
            };
            raw_cols.extend(cols);

            let fks: Vec<(String, String, String, String)> = {
                let mut stmt = conn.prepare(&format!("PRAGMA foreign_key_list(\"{table}\")"))?;
                stmt.query_map([], |r| {
                    Ok((
                        table.clone(),
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?
            };
            raw_fks.extend(fks);
        }

        Ok(build_schema_value(&cfg_clone, raw_tables, raw_cols, raw_fks))
    })
    .await?
}

fn tsv_rows(raw: &str, ncols: usize) -> Vec<Vec<String>> {
    raw.lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut parts: Vec<String> = line.split('\t').map(str::to_string).collect();
            parts.resize(ncols, String::new());
            parts
        })
        .collect()
}

fn build_schema_value(
    cfg: &DbConfig,
    raw_tables: Vec<(String, Option<String>)>,
    raw_cols: Vec<(String, String, String, String, String, String)>,
    raw_fks: Vec<(String, String, String, String)>,
) -> SchemaTab {
    let tables: Vec<SchemaTableInfo> = raw_tables
        .into_iter()
        .map(|(name, comment)| SchemaTableInfo {
            name,
            comment: comment.unwrap_or_default(),
        })
        .collect();

    let mut fk_lookup: HashMap<(String, String), String> = HashMap::new();
    for (tbl, col, ref_tbl, _) in &raw_fks {
        fk_lookup.insert((tbl.clone(), col.clone()), ref_tbl.clone());
    }

    let mut columns: HashMap<String, Vec<SchemaColumn>> = HashMap::new();
    for (table, col_name, typ, nullable, key, extra) in raw_cols {
        let fk_target = fk_lookup.get(&(table.clone(), col_name.clone())).cloned();
        columns.entry(table).or_default().push(SchemaColumn {
            name: col_name,
            type_name: typ,
            nullable: nullable == "YES",
            key,
            extra,
            fk_target,
        });
    }

    let foreign_keys: Vec<SchemaForeignKey> = raw_fks
        .into_iter()
        .map(|(table, column, ref_table, ref_column)| SchemaForeignKey {
            table,
            column,
            ref_table,
            ref_column,
        })
        .collect();

    let domains = load_domains(&cfg.domains_file);

    SchemaTab {
        title: cfg.name.clone(),
        tables,
        columns,
        foreign_keys,
        domains,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cfg() -> DbConfig {
        DbConfig {
            name: "main".into(),
            driver: "sqlite".into(),
            host: String::new(),
            port: 0,
            user: String::new(),
            password: String::new(),
            database: ":memory:".into(),
            container: None,
            domains_file: None,
        }
    }

    #[test]
    fn dbconfig_from_row_defaults_postgres_port() {
        let row = db::schema_databases::SchemaDbRow {
            name: "pg".into(),
            driver: "postgres".into(),
            host: None,
            port: None,
            user: "u".into(),
            password: "p".into(),
            database: "d".into(),
            container: None,
            domains_file: None,
            enabled: true,
        };
        let cfg = DbConfig::from(row);
        assert_eq!(cfg.port, 5432);
        assert_eq!(cfg.host, "");
    }

    #[test]
    fn dbconfig_from_row_defaults_mysql_port_and_keeps_explicit() {
        let row = db::schema_databases::SchemaDbRow {
            name: "my".into(),
            driver: "mysql".into(),
            host: Some("db.local".into()),
            port: Some(3307),
            user: "u".into(),
            password: "p".into(),
            database: "d".into(),
            container: Some("c".into()),
            domains_file: Some("~/d.json".into()),
            enabled: true,
        };
        let cfg = DbConfig::from(row);
        assert_eq!(cfg.port, 3307);
        assert_eq!(cfg.host, "db.local");
        assert_eq!(cfg.container.as_deref(), Some("c"));
        assert_eq!(cfg.domains_file.as_deref(), Some("~/d.json"));
    }

    #[test]
    fn dbconfig_from_row_mysql_default_port_when_none() {
        let row = db::schema_databases::SchemaDbRow {
            name: "my".into(),
            driver: "mysql".into(),
            host: None,
            port: None,
            user: "u".into(),
            password: "p".into(),
            database: "d".into(),
            container: None,
            domains_file: None,
            enabled: true,
        };
        assert_eq!(DbConfig::from(row).port, 3306);
    }

    #[test]
    fn schema_build_error_display_no_databases() {
        let msg = SchemaBuildError::NoDatabases.to_string();
        assert!(msg.contains("No databases configured"));
    }

    #[test]
    fn schema_build_error_display_all_failed() {
        let msg = SchemaBuildError::AllFailed("boom".into()).to_string();
        assert_eq!(msg, "All databases failed: boom");
    }

    #[test]
    fn load_domains_none_yields_empty() {
        assert!(load_domains(&None).is_empty());
    }

    #[test]
    fn load_domains_missing_file_yields_empty() {
        assert!(load_domains(&Some("/no/such/file.json".into())).is_empty());
    }

    #[test]
    fn load_domains_reads_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("domains.json");
        std::fs::write(
            &path,
            r##"[{"key":"k","label":"L","color":"#fff","tables":["t1","t2"]}]"##,
        )
        .unwrap();
        let domains = load_domains(&Some(path.to_string_lossy().into_owned()));
        assert_eq!(domains.len(), 1);
        assert_eq!(domains[0].key, "k");
        assert_eq!(domains[0].tables, vec!["t1", "t2"]);
    }

    #[test]
    fn load_domains_invalid_json_yields_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(load_domains(&Some(path.to_string_lossy().into_owned())).is_empty());
    }

    #[test]
    fn build_schema_value_maps_tables_columns_fks() {
        let cfg = base_cfg();
        let raw_tables = vec![
            ("users".into(), Some("people".into())),
            ("orders".into(), None),
        ];
        let raw_cols = vec![
            (
                "users".into(),
                "id".into(),
                "int".into(),
                "NO".into(),
                "PRI".into(),
                "auto_increment".into(),
            ),
            (
                "orders".into(),
                "user_id".into(),
                "int".into(),
                "YES".into(),
                "MUL".into(),
                String::new(),
            ),
        ];
        let raw_fks = vec![(
            "orders".into(),
            "user_id".into(),
            "users".into(),
            "id".into(),
        )];
        let tab = build_schema_value(&cfg, raw_tables, raw_cols, raw_fks);

        assert_eq!(tab.title, "main");
        assert_eq!(tab.tables.len(), 2);
        let users_tab = tab.tables.iter().find(|t| t.name == "users").unwrap();
        assert_eq!(users_tab.comment, "people");
        let orders_tab = tab.tables.iter().find(|t| t.name == "orders").unwrap();
        assert_eq!(orders_tab.comment, "");

        let id_col = &tab.columns["users"][0];
        assert_eq!(id_col.name, "id");
        assert!(!id_col.nullable);
        assert_eq!(id_col.key, "PRI");
        assert_eq!(id_col.fk_target, None);

        let uid_col = &tab.columns["orders"][0];
        assert!(uid_col.nullable);
        assert_eq!(uid_col.fk_target.as_deref(), Some("users"));

        assert_eq!(tab.foreign_keys.len(), 1);
        assert_eq!(tab.foreign_keys[0].ref_table, "users");
    }

    #[test]
    fn build_schema_value_empty_inputs() {
        let cfg = base_cfg();
        let tab = build_schema_value(&cfg, vec![], vec![], vec![]);
        assert!(tab.tables.is_empty());
        assert!(tab.columns.is_empty());
        assert!(tab.foreign_keys.is_empty());
        assert!(tab.domains.is_empty());
    }

    #[test]
    fn schema_column_serializes_type_rename() {
        let col = SchemaColumn {
            name: "id".into(),
            type_name: "int".into(),
            nullable: false,
            key: "PRI".into(),
            extra: String::new(),
            fk_target: None,
        };
        let v = serde_json::to_value(&col).unwrap();
        assert_eq!(v["type"], "int");
        assert!(v.get("type_name").is_none());
    }

    #[test]
    fn tsv_rows_normal() {
        let raw = "foo\tbar\nbaz\tqux\n";
        let rows = tsv_rows(raw, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["foo".to_string(), "bar".to_string()]);
        assert_eq!(rows[1], vec!["baz".to_string(), "qux".to_string()]);
    }

    #[test]
    fn tsv_rows_empty_input() {
        let rows = tsv_rows("", 1);
        assert!(rows.is_empty());
    }

    #[test]
    fn tsv_rows_short_row_fills_empty_strings() {
        let raw = "only_one_field\n";
        let rows = tsv_rows(raw, 3);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            vec!["only_one_field".to_string(), String::new(), String::new()]
        );
    }

    #[test]
    fn tsv_rows_extra_fields_are_truncated() {
        // resize both pads and truncates — a longer row is cut down to ncols.
        let raw = "a\tb\tc\td\n";
        let rows = tsv_rows(raw, 2);
        assert_eq!(rows[0].len(), 2);
        assert_eq!(rows[0], vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn tsv_rows_skips_blank_lines() {
        let raw = "a\tb\n\nc\td\n\n";
        let rows = tsv_rows(raw, 2);
        assert_eq!(rows.len(), 2);
    }

    // ── docker TSV → build_schema_value round-trip (mirrors query_database_docker
    //    parsing without invoking docker) ─────────────────────────────────────
    #[test]
    fn docker_tsv_parsing_shapes_match_build_schema_value() {
        let cfg = base_cfg();
        let tables_tsv = "users\tpeople\norders\t\n";
        let cols_tsv = "users\tid\tint\tNO\tPRI\tauto_increment\n\
                        orders\tuser_id\tint\tYES\tMUL\t\n";
        let fk_tsv = "orders\tuser_id\tusers\tid\n";

        let raw_tables: Vec<(String, Option<String>)> = tsv_rows(tables_tsv, 2)
            .into_iter()
            .map(|mut r| (r.remove(0), r.into_iter().next().filter(|s| !s.is_empty())))
            .collect();
        let raw_cols: Vec<(String, String, String, String, String, String)> = tsv_rows(cols_tsv, 6)
            .into_iter()
            .map(|mut r| {
                let mut g = || r.remove(0);
                (g(), g(), g(), g(), g(), g())
            })
            .collect();
        let raw_fks: Vec<(String, String, String, String)> = tsv_rows(fk_tsv, 4)
            .into_iter()
            .map(|mut r| {
                let mut g = || r.remove(0);
                (g(), g(), g(), g())
            })
            .collect();

        let tab = build_schema_value(&cfg, raw_tables, raw_cols, raw_fks);
        assert_eq!(tab.tables.len(), 2);
        // empty TABLE_COMMENT for orders is filtered → renders as "".
        let orders = tab.tables.iter().find(|t| t.name == "orders").unwrap();
        assert_eq!(orders.comment, "");
        assert_eq!(tab.columns["orders"][0].fk_target.as_deref(), Some("users"));
    }

    #[test]
    fn build_schema_value_loads_domains_from_cfg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("domains.json");
        std::fs::write(
            &path,
            r##"[{"key":"core","label":"Core","color":"#000","tables":["users"]}]"##,
        )
        .unwrap();
        let mut cfg = base_cfg();
        cfg.domains_file = Some(path.to_string_lossy().into_owned());

        let tab = build_schema_value(&cfg, vec![], vec![], vec![]);
        assert_eq!(tab.domains.len(), 1);
        assert_eq!(tab.domains[0].key, "core");
    }

    #[test]
    fn build_schema_value_fk_only_no_matching_column() {
        // An FK whose (table,col) never appears in raw_cols still lands in
        // foreign_keys but leaves no column to annotate.
        let cfg = base_cfg();
        let raw_fks = vec![(
            "orders".into(),
            "user_id".into(),
            "users".into(),
            "id".into(),
        )];
        let tab = build_schema_value(&cfg, vec![], vec![], raw_fks);
        assert_eq!(tab.foreign_keys.len(), 1);
        assert!(tab.columns.is_empty());
    }

    // ── SchemaTab serialization shape (assert on the wire string) ───────────
    #[test]
    fn schema_tab_serializes_expected_keys() {
        let cfg = base_cfg();
        let tab = build_schema_value(
            &cfg,
            vec![("t".into(), None)],
            vec![(
                "t".into(),
                "c".into(),
                "text".into(),
                "YES".into(),
                String::new(),
                String::new(),
            )],
            vec![],
        );
        let s = serde_json::to_string(&tab).unwrap();
        assert!(s.contains("\"title\":\"main\""));
        assert!(s.contains("\"tables\""));
        assert!(s.contains("\"columns\""));
        assert!(s.contains("\"foreign_keys\"") || s.contains("\"foreignKeys\""));
        // nullable column with empty key/extra still serializes its column name.
        assert!(s.contains("\"c\""));
    }

    // ── SchemaForeignKey serialization ──────────────────────────────────────
    #[test]
    fn schema_foreign_key_serializes() {
        let fk = SchemaForeignKey {
            table: "orders".into(),
            column: "user_id".into(),
            ref_table: "users".into(),
            ref_column: "id".into(),
        };
        let s = serde_json::to_string(&fk).unwrap();
        assert!(s.contains("orders"));
        assert!(s.contains("users"));
    }

    // ── TOML config deserialization (aliases, defaults) ─────────────────────
    #[test]
    fn toml_config_parses_aliases_and_defaults() {
        let raw = r#"
[schema]
[[schema.databases]]
name = "app"
user = "root"
password = "secret"
database = "appdb"
domainsFile = "~/domains.json"
"#;
        let cfg: TomlOrcaConfig = toml::from_str(raw).unwrap();
        let dbs = cfg.schema.unwrap().databases;
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].name, "app");
        // omitted host/port fall back to serde defaults.
        assert_eq!(dbs[0].host, "");
        assert_eq!(dbs[0].port, 0);
        // domainsFile alias maps to domains_file.
        assert_eq!(dbs[0].domains_file.as_deref(), Some("~/domains.json"));
        assert!(dbs[0].container.is_none());
    }

    #[test]
    fn toml_config_empty_defaults() {
        let cfg: TomlOrcaConfig = toml::from_str("").unwrap();
        assert!(cfg.schema.is_none());
    }

    // ── SQLite introspection (fully deterministic against a temp DB file) ────
    fn sqlite_cfg(path: &std::path::Path) -> DbConfig {
        DbConfig {
            name: "sqlite_tab".into(),
            driver: "sqlite".into(),
            host: String::new(),
            port: 0,
            user: String::new(),
            password: String::new(),
            database: path.to_string_lossy().into_owned(),
            container: None,
            domains_file: None,
        }
    }

    fn seed_sqlite(path: &std::path::Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);\n\
             CREATE TABLE orders (\
                id INTEGER PRIMARY KEY, \
                user_id INTEGER REFERENCES users(id));",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn query_database_sqlite_introspects_tables_columns_fks() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("app.sqlite");
        seed_sqlite(&db_path);

        let cfg = sqlite_cfg(&db_path);
        let tab = query_database_sqlite(&cfg).await.unwrap();

        assert_eq!(tab.title, "sqlite_tab");
        assert_eq!(tab.tables.len(), 2);
        assert!(tab.tables.iter().any(|t| t.name == "users"));
        assert!(tab.tables.iter().any(|t| t.name == "orders"));

        // users.id is PRIMARY KEY → key "PRI" (PRAGMA pk flag). SQLite reports
        // notnull=0 for an INTEGER PRIMARY KEY rowid alias, so nullable is true.
        let users_cols = &tab.columns["users"];
        let id = users_cols.iter().find(|c| c.name == "id").unwrap();
        assert_eq!(id.key, "PRI");
        // name is TEXT NOT NULL → notnull=1 → not nullable.
        let name = users_cols.iter().find(|c| c.name == "name").unwrap();
        assert!(!name.nullable);
        assert_eq!(name.key, "");

        // orders.user_id is a nullable FK targeting users.
        let uid = tab.columns["orders"]
            .iter()
            .find(|c| c.name == "user_id")
            .unwrap();
        assert!(uid.nullable);
        assert_eq!(uid.fk_target.as_deref(), Some("users"));

        assert_eq!(tab.foreign_keys.len(), 1);
        let fk = &tab.foreign_keys[0];
        assert_eq!(fk.table, "orders");
        assert_eq!(fk.column, "user_id");
        assert_eq!(fk.ref_table, "users");
        assert_eq!(fk.ref_column, "id");
    }

    // ── Connection-error paths for the network drivers ──────────────────────
    // Port 1 refuses immediately, so these exercise the driver setup + the `?`
    // error propagation without needing a live server (fast, deterministic).

    fn refused_cfg(driver: &str) -> DbConfig {
        DbConfig {
            name: "down".into(),
            driver: driver.into(),
            host: "127.0.0.1".into(),
            port: 1,
            user: "u".into(),
            password: "p".into(),
            database: "d".into(),
            container: None,
            domains_file: None,
        }
    }

    #[tokio::test]
    async fn query_database_mysql_native_connection_refused_errors() {
        let cfg = refused_cfg("mysql");
        assert!(query_database_mysql_native(&cfg).await.is_err());
    }

    #[tokio::test]
    async fn query_database_postgres_connection_refused_errors() {
        let cfg = refused_cfg("postgres");
        assert!(query_database_postgres(&cfg).await.is_err());
    }

    #[tokio::test]
    async fn query_database_dispatches_to_mysql_native_arm() {
        // driver != postgres/sqlite and container None → mysql-native arm.
        let cfg = refused_cfg("mysql");
        assert!(query_database(&cfg).await.is_err());
    }

    #[tokio::test]
    async fn query_database_dispatches_to_postgres_arm() {
        let cfg = refused_cfg("postgres");
        assert!(query_database(&cfg).await.is_err());
    }

    #[tokio::test]
    async fn query_database_dispatches_to_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("app.sqlite");
        seed_sqlite(&db_path);
        let cfg = sqlite_cfg(&db_path);
        // Goes through the driver match arm for "sqlite".
        let tab = query_database(&cfg).await.unwrap();
        assert_eq!(tab.tables.len(), 2);
    }

    #[tokio::test]
    async fn query_database_sqlite_missing_file_errors() {
        let cfg = sqlite_cfg(std::path::Path::new("/no/such/dir/missing.sqlite"));
        // READ_ONLY open of a nonexistent file fails.
        assert!(query_database_sqlite(&cfg).await.is_err());
    }

    // ── load_db_configs / build_schema_* against an isolated orca.db ─────────

    // Serializes tests that mutate process-global env (HOME / ORCA_CONFIG).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn load_db_configs_reads_rows_from_db() {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Point ORCA_CONFIG at a nonexistent file so the toml fallback is inert.
        // SAFETY: ENV_LOCK serializes all env mutation in this test module.
        unsafe {
            std::env::set_var("ORCA_CONFIG", "/no/such/orca.toml");
        }
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("orca.db");
        let configs = crate::with_thread_db_path(&db_path, || {
            let conn = db::open_default().unwrap();
            let row = db::schema_databases::SchemaDbRow {
                name: "pg".into(),
                driver: "postgres".into(),
                host: Some("h".into()),
                port: Some(6000),
                user: "u".into(),
                password: "p".into(),
                database: "d".into(),
                container: None,
                domains_file: None,
                enabled: true,
            };
            db::schema_databases::upsert(&conn, &row).unwrap();
            drop(conn);
            load_db_configs()
        });
        drop(guard);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "pg");
        assert_eq!(configs[0].driver, "postgres");
        assert_eq!(configs[0].port, 6000);
    }

    #[test]
    fn load_db_configs_migrates_from_toml_when_db_empty() {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("orca.toml");
        std::fs::write(
            &toml_path,
            r#"
[schema]
[[schema.databases]]
name = "legacy"
host = "db.local"
port = 3307
user = "root"
password = "pw"
database = "legacydb"
"#,
        )
        .unwrap();
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("ORCA_CONFIG", &toml_path);
        }
        let db_path = dir.path().join("orca.db");
        let (configs, persisted) = crate::with_thread_db_path(&db_path, || {
            let migrated = load_db_configs();
            // Second call now reads the freshly-migrated rows straight from db.
            let conn = db::open_default().unwrap();
            let rows = db::schema_databases::list(&conn).unwrap();
            (migrated, rows)
        });
        drop(guard);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "legacy");
        assert_eq!(configs[0].driver, "mysql");
        assert_eq!(configs[0].host, "db.local");
        assert_eq!(configs[0].port, 3307);
        // migration persisted the row (INSERT OR IGNORE via upsert).
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].name, "legacy");
    }

    #[test]
    fn load_db_configs_empty_when_no_db_rows_and_no_toml() {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("ORCA_CONFIG", "/no/such/orca.toml");
        }
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("orca.db");
        let configs = crate::with_thread_db_path(&db_path, load_db_configs);
        drop(guard);
        assert!(configs.is_empty());
    }

    #[test]
    fn build_schema_domains_flattens_across_configs() {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("ORCA_CONFIG", "/no/such/orca.toml");
        }
        let dir = tempfile::tempdir().unwrap();
        let domains_path = dir.path().join("domains.json");
        std::fs::write(
            &domains_path,
            r##"[{"key":"a","label":"A","color":"#111","tables":["t"]}]"##,
        )
        .unwrap();
        let db_path = dir.path().join("orca.db");
        let domains = crate::with_thread_db_path(&db_path, || {
            let conn = db::open_default().unwrap();
            let row = db::schema_databases::SchemaDbRow {
                name: "d1".into(),
                driver: "sqlite".into(),
                host: None,
                port: None,
                user: String::new(),
                password: String::new(),
                database: ":memory:".into(),
                container: None,
                domains_file: Some(domains_path.to_string_lossy().into_owned()),
                enabled: true,
            };
            db::schema_databases::upsert(&conn, &row).unwrap();
            drop(conn);
            build_schema_domains()
        });
        drop(guard);
        assert_eq!(domains.len(), 1);
        assert_eq!(domains[0].key, "a");
    }

    #[test]
    fn build_schema_domains_empty_when_no_configs() {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("ORCA_CONFIG", "/no/such/orca.toml");
        }
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("orca.db");
        let domains = crate::with_thread_db_path(&db_path, build_schema_domains);
        drop(guard);
        assert!(domains.is_empty());
    }

    #[test]
    fn build_schema_response_no_databases_errors() {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("ORCA_CONFIG", "/no/such/orca.toml");
        }
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("orca.db");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // block_on is synchronous — the ENV_LOCK guard never crosses an await.
        let res = rt.block_on(crate::with_db_path(db_path, build_schema_response()));
        drop(guard);
        assert!(matches!(res, Err(SchemaBuildError::NoDatabases)));
    }

    #[test]
    fn build_schema_response_builds_single_tab_from_sqlite() {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("ORCA_CONFIG", "/no/such/orca.toml");
        }
        let dir = tempfile::tempdir().unwrap();
        let sqlite_path = dir.path().join("app.sqlite");
        seed_sqlite(&sqlite_path);
        let db_path = dir.path().join("orca.db");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let out = rt.block_on(crate::with_db_path(db_path, async {
            let conn = db::open_default().unwrap();
            let row = db::schema_databases::SchemaDbRow {
                name: "only".into(),
                driver: "sqlite".into(),
                host: None,
                port: None,
                user: String::new(),
                password: String::new(),
                database: sqlite_path.to_string_lossy().into_owned(),
                container: None,
                domains_file: None,
                enabled: true,
            };
            db::schema_databases::upsert(&conn, &row).unwrap();
            drop(conn);
            build_schema_response().await
        }));
        drop(guard);

        let out = out.unwrap();
        // single configured db → no tab bar, no errors.
        assert!(!out.show_tabs);
        assert!(out.errors.is_none());
        assert_eq!(out.tabs.len(), 1);
        assert_eq!(out.tabs[0].tables.len(), 2);
    }

    #[test]
    fn build_schema_response_multi_tab_sets_show_tabs_and_partial_errors() {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("ORCA_CONFIG", "/no/such/orca.toml");
        }
        let dir = tempfile::tempdir().unwrap();
        // Two healthy sqlite dbs → two tabs → show_tabs true; one broken db →
        // the errors vector is populated but does not suppress the good tabs.
        let ok1 = dir.path().join("one.sqlite");
        let ok2 = dir.path().join("two.sqlite");
        seed_sqlite(&ok1);
        seed_sqlite(&ok2);
        let db_path = dir.path().join("orca.db");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let out = rt.block_on(crate::with_db_path(db_path, async {
            let conn = db::open_default().unwrap();
            for (name, path) in [
                ("first", ok1.to_string_lossy().into_owned()),
                ("second", ok2.to_string_lossy().into_owned()),
                ("broken", "/no/such/dir/missing.sqlite".to_string()),
            ] {
                let row = db::schema_databases::SchemaDbRow {
                    name: name.into(),
                    driver: "sqlite".into(),
                    host: None,
                    port: None,
                    user: String::new(),
                    password: String::new(),
                    database: path,
                    container: None,
                    domains_file: None,
                    enabled: true,
                };
                db::schema_databases::upsert(&conn, &row).unwrap();
            }
            drop(conn);
            build_schema_response().await
        }));
        drop(guard);

        let out = out.unwrap();
        assert!(out.show_tabs, "two healthy tabs → tab bar shown");
        assert_eq!(out.tabs.len(), 2);
        let errors = out.errors.expect("broken db populates errors");
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].contains("broken"),
            "error names the db: {errors:?}"
        );
    }

    #[test]
    fn build_schema_response_all_failed_when_connection_fails() {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("ORCA_CONFIG", "/no/such/orca.toml");
        }
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("orca.db");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let res = rt.block_on(crate::with_db_path(db_path, async {
            let conn = db::open_default().unwrap();
            // sqlite pointed at a nonexistent file → the only tab fails to build.
            let row = db::schema_databases::SchemaDbRow {
                name: "broken".into(),
                driver: "sqlite".into(),
                host: None,
                port: None,
                user: String::new(),
                password: String::new(),
                database: "/no/such/dir/missing.sqlite".into(),
                container: None,
                domains_file: None,
                enabled: true,
            };
            db::schema_databases::upsert(&conn, &row).unwrap();
            drop(conn);
            build_schema_response().await
        }));
        drop(guard);
        match res {
            Err(SchemaBuildError::AllFailed(msg)) => assert!(msg.contains("broken")),
            Err(SchemaBuildError::NoDatabases) => panic!("expected AllFailed, got NoDatabases"),
            Ok(_) => panic!("expected AllFailed, got Ok"),
        }
    }
}
