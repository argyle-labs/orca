use std::collections::HashMap;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::prelude::*;

// ── Schema database config ────────────────────────────────────────────────────

#[derive(Clone)]
struct DbConfig {
    name: String,
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
    container: Option<String>,
    domains_file: Option<String>,
}

impl From<brain_utils::db::SchemaDbRow> for DbConfig {
    fn from(r: brain_utils::db::SchemaDbRow) -> Self {
        DbConfig {
            name: r.name,
            host: r.host.unwrap_or_default(),
            port: r.port.unwrap_or(3306),
            user: r.user,
            password: r.password,
            database: r.database,
            container: r.container,
            domains_file: r.domains_file,
        }
    }
}

// ── TOML migration types (used only for one-shot import) ─────────────────────

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
struct TomlBrainConfig {
    schema: Option<TomlSchemaSection>,
}

/// Load schema DB configs from brain.db. If the table is empty, attempt a
/// one-shot migration from brain.toml (idempotent: INSERT OR IGNORE).
fn load_db_configs() -> Vec<DbConfig> {
    let Ok(conn) = brain_utils::db::open_default() else {
        return vec![];
    };

    // Try DB first
    if let Ok(rows) = brain_utils::db::list_schema_databases(&conn) {
        if !rows.is_empty() {
            return rows.into_iter().map(DbConfig::from).collect();
        }
    }

    // DB empty — attempt one-shot migration from brain.toml
    let home = std::env::var("HOME").unwrap_or_default();
    let toml_path =
        std::env::var("BRAIN_CONFIG").unwrap_or_else(|_| format!("{home}/.brain/brain.toml"));

    if let Ok(raw) = std::fs::read_to_string(&toml_path)
        && let Ok(cfg) = toml::from_str::<TomlBrainConfig>(&raw)
    {
        let dbs = cfg.schema.map(|s| s.databases).unwrap_or_default();
        for d in &dbs {
            let row = brain_utils::db::SchemaDbRow {
                name: d.name.clone(),
                host: if d.host.is_empty() { None } else { Some(d.host.clone()) },
                port: if d.port == 0 { None } else { Some(d.port) },
                user: d.user.clone(),
                password: d.password.clone(),
                database: d.database.clone(),
                container: d.container.clone(),
                domains_file: d.domains_file.clone(),
                enabled: true,
            };
            let _ = brain_utils::db::upsert_schema_database(&conn, &row);
        }
        if !dbs.is_empty() {
            return dbs
                .into_iter()
                .map(|d| DbConfig {
                    name: d.name,
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

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/{rest}")
    } else {
        path.to_string()
    }
}

pub(crate) fn load_domains(domains_file: &Option<String>) -> Value {
    let Some(path) = domains_file else {
        return json!([]);
    };
    let expanded = expand_tilde(path);
    std::fs::read_to_string(&expanded)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(json!([]))
}

// ── GET /api/schema ───────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/schema",
    operation_id = "getSchema",
    responses(
        (status = 200, description = "Database schema tabs", body = super::SchemaResponse),
        (status = 404, description = "No databases configured", body = ErrorResponse),
        (status = 500, description = "All DB connections failed", body = ErrorResponse),
    ),
    tag = "schema"
)]
pub async fn schema_handler() -> Response {
    let configs = load_db_configs();
    if configs.is_empty() {
        return err(
            StatusCode::NOT_FOUND,
            "No databases configured — use `brain schema add` or POST /api/schema/databases",
        );
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
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("All databases failed: {}", errors.join("; ")),
        );
    }

    let show_tabs = tabs.len() > 1;
    let errors_opt = if errors.is_empty() {
        None
    } else {
        Some(errors)
    };
    Json(json!({ "tabs": tabs, "showTabs": show_tabs, "errors": errors_opt })).into_response()
}

async fn query_database(cfg: &DbConfig) -> anyhow::Result<Value> {
    let db = &cfg.database;
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

    let (tables_raw, cols_raw, fk_raw) = tokio::try_join!(
        mysql_query_cfg(cfg, &tables_sql),
        mysql_query_cfg(cfg, &cols_sql),
        mysql_query_cfg(cfg, &fk_sql),
    )?;

    let tables: Vec<Value> = parse_mysql_tsv(&tables_raw, &["name", "comment"])
        .into_iter()
        .map(|mut row| {
            let name = row.remove("name").unwrap_or_default();
            json!({ "name": name, "comment": row.remove("comment").unwrap_or_default() })
        })
        .collect();

    let fk_rows = parse_mysql_tsv(&fk_raw, &["table", "column", "ref_table", "ref_column"]);
    let mut fk_lookup: HashMap<(String, String), String> = HashMap::new();
    for row in &fk_rows {
        let key = (
            row.get("table").cloned().unwrap_or_default(),
            row.get("column").cloned().unwrap_or_default(),
        );
        fk_lookup.insert(key, row.get("ref_table").cloned().unwrap_or_default());
    }

    let mut columns: HashMap<String, Vec<Value>> = HashMap::new();
    for row in parse_mysql_tsv(
        &cols_raw,
        &["table", "name", "type", "nullable", "key", "extra"],
    ) {
        let table = row.get("table").cloned().unwrap_or_default();
        let col_name = row.get("name").cloned().unwrap_or_default();
        let fk_target = fk_lookup.get(&(table.clone(), col_name.clone())).cloned();
        columns.entry(table).or_default().push(json!({
            "name": col_name,
            "type": row.get("type"),
            "nullable": row.get("nullable") == Some(&"YES".to_string()),
            "key": row.get("key"),
            "extra": row.get("extra"),
            "fk_target": fk_target,
        }));
    }

    let foreign_keys: Vec<Value> = fk_rows
        .into_iter()
        .map(|row| {
            json!({
                "table": row.get("table"),
                "column": row.get("column"),
                "refTable": row.get("ref_table"),
                "refColumn": row.get("ref_column"),
            })
        })
        .collect();

    let domains = load_domains(&cfg.domains_file);

    Ok(json!({
        "title": cfg.name,
        "tables": tables,
        "columns": columns,
        "foreignKeys": foreign_keys,
        "domains": domains,
    }))
}

async fn mysql_query_cfg(cfg: &DbConfig, sql: &str) -> anyhow::Result<String> {
    let pass_arg = format!("-p{}", cfg.password);
    let mysql_args = [
        "-u",
        cfg.user.as_str(),
        pass_arg.as_str(),
        cfg.database.as_str(),
        "--batch",
        "--silent",
        "-e",
        sql,
    ];

    let out = if let Some(container) = &cfg.container {
        let mut args = vec!["exec", container.as_str(), "mysql"];
        args.extend_from_slice(&mysql_args);
        Command::new("docker").args(&args).output().await?
    } else {
        let port_str = cfg.port.to_string();
        let mut args = vec!["-h", cfg.host.as_str(), "-P", port_str.as_str()];
        args.extend_from_slice(&mysql_args);
        Command::new("mysql").args(&args).output().await?
    };

    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{}", e.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub(crate) fn parse_mysql_tsv(raw: &str, cols: &[&str]) -> Vec<HashMap<String, String>> {
    raw.lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let values: Vec<&str> = line.split('\t').collect();
            cols.iter()
                .enumerate()
                .map(|(i, &col)| (col.to_string(), values.get(i).unwrap_or(&"").to_string()))
                .collect()
        })
        .collect()
}

// ── GET /api/schema/domains ───────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/schema/domains",
    operation_id = "getSchemaDomains",
    responses(
        (status = 200, description = "All domain definitions from all configured databases"),
    ),
    tag = "schema"
)]
pub async fn schema_domains_handler() -> Response {
    let configs = load_db_configs();
    let mut all: Vec<Value> = Vec::new();
    for cfg in &configs {
        if let Value::Array(domains) = load_domains(&cfg.domains_file) {
            all.extend(domains);
        }
    }
    Json(json!(all)).into_response()
}
