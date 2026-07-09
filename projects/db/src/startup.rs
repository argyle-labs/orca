//! One-time startup routines: TOML → DB migrations, API key loading, runtime auto-registration.
//!
//! Call `init(config)` once at binary startup (after `Config::load()`) to run all migrations
//! and auto-register any detected runtimes. Call `load_api_key(config)` to read the Anthropic
//! API key from the encrypted DB when none is provided via environment variable.

use contract::config::Config;

use crate::{open, to_json_arr, to_json_obj};

/// Run all one-time startup tasks: TOML migrations.
pub fn init(config: &Config) {
    let toml_path = config.orca_toml_path();
    if toml_path.exists() {
        migrate_toml_servers_to_db(&toml_path, &config.db_path);
        migrate_toml_schema_databases_to_db(&toml_path, &config.db_path);
    }
}

/// Read the Anthropic API key from the encrypted DB secrets table.
/// Returns `None` if the DB can't be opened or no key is stored.
pub fn load_api_key(config: &Config) -> Option<String> {
    let conn = open(&config.db_path).ok()?;
    crate::settings::secret_get(&conn, "anthropic_api_key")
        .ok()
        .flatten()
}

fn migrate_toml_servers_to_db(toml_path: &std::path::Path, db_path: &std::path::Path) {
    #[derive(serde::Deserialize, Default)]
    struct LegacyToml {
        #[serde(default)]
        mcp: LegacyMcp,
    }
    #[derive(serde::Deserialize, Default)]
    struct LegacyMcp {
        #[serde(default)]
        servers: Vec<LegacyServer>,
    }
    #[derive(serde::Deserialize)]
    struct LegacyServer {
        name: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
    }

    let Ok(raw) = std::fs::read_to_string(toml_path) else {
        return;
    };
    let Ok(parsed) = toml::from_str::<LegacyToml>(&raw) else {
        return;
    };
    if parsed.mcp.servers.is_empty() {
        return;
    }

    let Ok(conn) = open(db_path) else { return };
    for s in &parsed.mcp.servers {
        let args_json = to_json_arr(&s.args);
        let env_json = to_json_obj(&s.env);
        conn.execute(
            "INSERT OR IGNORE INTO mcp_servers (name, command, args, env, enabled)
             VALUES (?1, ?2, ?3, ?4, 1)",
            rusqlite::params![s.name, s.command, args_json, env_json],
        )
        .ok();
    }
    tracing::info!(
        "migrated {} mcp server(s) from orca.toml to orca.db",
        parsed.mcp.servers.len()
    );
}

fn migrate_toml_schema_databases_to_db(toml_path: &std::path::Path, db_path: &std::path::Path) {
    #[derive(serde::Deserialize, Default)]
    struct LegacyToml {
        schema: Option<LegacySchema>,
    }
    #[derive(serde::Deserialize, Default)]
    struct LegacySchema {
        #[serde(default)]
        databases: Vec<LegacySchemaDb>,
    }
    #[derive(serde::Deserialize)]
    struct LegacySchemaDb {
        name: String,
        #[serde(default)]
        host: String,
        #[serde(default)]
        port: u16,
        #[serde(default)]
        user: String,
        #[serde(default)]
        password: String,
        #[serde(default)]
        database: String,
        container: Option<String>,
        #[serde(alias = "domainsFile")]
        domains_file: Option<String>,
    }

    let Ok(raw) = std::fs::read_to_string(toml_path) else {
        return;
    };
    let Ok(parsed) = toml::from_str::<LegacyToml>(&raw) else {
        return;
    };
    let dbs = parsed.schema.map(|s| s.databases).unwrap_or_default();
    if dbs.is_empty() {
        return;
    }

    let Ok(conn) = open(db_path) else { return };
    for d in &dbs {
        let host: Option<&str> = if d.host.is_empty() {
            None
        } else {
            Some(&d.host)
        };
        let port: Option<i64> = if d.port == 0 {
            None
        } else {
            Some(d.port as i64)
        };
        conn.execute(
            "INSERT OR IGNORE INTO schema_databases
                (name, host, port, user, password, database, container, domains_file, enabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)",
            rusqlite::params![
                d.name,
                host,
                port,
                d.user,
                d.password,
                d.database,
                d.container,
                d.domains_file,
            ],
        )
        .ok();
    }
    tracing::info!(
        "migrated {} schema database(s) from orca.toml to orca.db",
        dbs.len()
    );
}
