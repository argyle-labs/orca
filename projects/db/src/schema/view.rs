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
    use super::tsv_rows;

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
}
