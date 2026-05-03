//! Runtime configuration for the orca binary.
//!
//! `Config::load()` is called once at startup. It reads `orca.toml` from the vault,
//! API keys from env/keychain, and runs one-time TOML → DB migrations.

use anyhow::{Context, Result};
use crate::consts::{APP_DB_FILE, APP_STATE_DIR};
use std::path::PathBuf;

/// All runtime configuration for the orca binary.
///
/// Static config (API keys, LLM endpoints) lives here.
/// Dynamic registries (MCP servers, Docker runtimes, etc.) live in `orca.db` — see `db.rs`.
#[derive(Debug, Clone)]
pub struct Config {
    pub anthropic_api_key: Option<String>,
    pub lmstudio_url: String,
    pub default_model: Model,
    /// State/config dir: ~/.orca (db, logs, memory, config)
    pub orca_vault: PathBuf,
    /// Obsidian knowledge vault root: ~/orca (or $ORCA_VAULT_ROOT)
    pub vault_root: PathBuf,
    pub memory_root: PathBuf,
    pub db_path: PathBuf,
}

/// Which model backend and model ID to use for a session.
///
/// Defaults to `LMStudio` (local-first). Claude is escalation-only.
#[derive(Debug, Clone)]
pub enum Model {
    /// Anthropic Claude API — requires `ANTHROPIC_API_KEY` or a keychain entry.
    Claude(String),
    /// LM Studio (OpenAI-compatible local server) — no API key needed.
    LMStudio(String),
}

impl Model {
    /// Parse a /model <spec> argument.
    /// Accepts: "claude-sonnet-4-6", "claude:claude-sonnet-4-6", "lmstudio:model-id"
    pub fn parse(s: &str) -> Self {
        if let Some(m) = s.strip_prefix("lmstudio:") {
            Model::LMStudio(m.to_string())
        } else if let Some(m) = s.strip_prefix("claude:") {
            Model::Claude(m.to_string())
        } else if s.starts_with("claude-") {
            Model::Claude(s.to_string())
        } else {
            // Assume LM Studio if no prefix and not a known Claude model
            Model::LMStudio(s.to_string())
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let home = dirs::home_dir().context("no home dir")?;
        let orca_vault = home.join(APP_STATE_DIR);
        let vault_root = std::env::var("ORCA_VAULT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("orca"));
        let memory_root = orca_vault.join("memory");
        let db_path = orca_vault.join(APP_DB_FILE);

        // API key: env var takes priority, then macOS Keychain
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .or_else(crate::auth::load_api_key_from_keychain);

        let lmstudio_url =
            std::env::var("LMSTUDIO_URL").unwrap_or_else(|_| "http://localhost:1234".to_string());

        // Always default to LM Studio. Claude is escalation-only.
        // Model ID resolved at session start from /v1/models.
        let default_model = Model::LMStudio(String::new());

        let toml_path = orca_vault.join("orca.toml");
        if toml_path.exists() {
            // One-time migration: [[mcp.servers]] and [[schema.databases]] → orca.db
            migrate_toml_servers_to_db(&toml_path, &db_path);
            migrate_toml_schema_databases_to_db(&toml_path, &db_path);
        }
        // Auto-register Colima if it's running and no runtimes are in the DB yet.
        migrate_colima_runtime(&db_path);

        Ok(Config {
            anthropic_api_key: api_key,
            lmstudio_url,
            default_model,
            orca_vault,
            vault_root,
            memory_root,
            db_path,
        })
    }

    pub fn orca_toml_path(&self) -> PathBuf {
        self.orca_vault.join("orca.toml")
    }

    pub fn agents_dir(&self) -> PathBuf {
        dirs::home_dir().unwrap_or_default().join(".claude/agents")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.orca_vault.join("logs/sessions")
    }

    pub fn config_dir(&self) -> PathBuf {
        dirs::home_dir().unwrap_or_default().join("code/orca/config")
    }
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

    let Ok(raw) = std::fs::read_to_string(toml_path) else { return };
    let Ok(parsed) = toml::from_str::<LegacyToml>(&raw) else { return };
    if parsed.mcp.servers.is_empty() { return }

    let Ok(conn) = crate::db::open(db_path) else { return };
    for s in &parsed.mcp.servers {
        let args_json = serde_json::to_string(&s.args).unwrap_or_else(|_| "[]".into());
        let env_json = serde_json::to_string(&s.env).unwrap_or_else(|_| "{}".into());
        let _ = conn.execute(
            "INSERT OR IGNORE INTO mcp_servers (name, command, args, env, enabled)
             VALUES (?1, ?2, ?3, ?4, 1)",
            rusqlite::params![s.name, s.command, args_json, env_json],
        );
    }
    tracing::info!(
        "migrated {} mcp server(s) from orca.toml to orca.db",
        parsed.mcp.servers.len()
    );
}

fn migrate_colima_runtime(db_path: &std::path::Path) {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };
    let sock = format!("{home}/.colima/default/docker.sock");
    if !std::path::Path::new(&sock).exists() {
        return;
    }
    let Ok(conn) = crate::db::open(db_path) else { return };
    // Only auto-register if no runtimes exist yet
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM docker_runtimes", [], |r| r.get(0))
        .unwrap_or(0);
    if count > 0 {
        return;
    }
    let _ = conn.execute(
        "INSERT OR IGNORE INTO docker_runtimes (name, socket_path, host, enabled)
         VALUES ('colima', ?1, NULL, 1)",
        rusqlite::params![format!("~/.colima/default/docker.sock")],
    );
    tracing::info!("auto-registered colima docker runtime in orca.db");
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

    let Ok(raw) = std::fs::read_to_string(toml_path) else { return };
    let Ok(parsed) = toml::from_str::<LegacyToml>(&raw) else { return };
    let dbs = parsed.schema.map(|s| s.databases).unwrap_or_default();
    if dbs.is_empty() { return }

    let Ok(conn) = crate::db::open(db_path) else { return };
    for d in &dbs {
        let host: Option<&str> = if d.host.is_empty() { None } else { Some(&d.host) };
        let port: Option<i64> = if d.port == 0 { None } else { Some(d.port as i64) };
        let _ = conn.execute(
            "INSERT OR IGNORE INTO schema_databases
                (name, host, port, user, password, database, container, domains_file, enabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)",
            rusqlite::params![
                d.name, host, port, d.user, d.password, d.database,
                d.container, d.domains_file,
            ],
        );
    }
    tracing::info!(
        "migrated {} schema database(s) from orca.toml to orca.db",
        dbs.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lmstudio_prefix() {
        let m = Model::parse("lmstudio:qwen3");
        assert!(
            matches!(m, Model::LMStudio(ref s) if s == "qwen3"),
            "got: {m:?}"
        );
    }

    #[test]
    fn parse_claude_colon_prefix() {
        let m = Model::parse("claude:claude-opus-4-7");
        assert!(
            matches!(m, Model::Claude(ref s) if s == "claude-opus-4-7"),
            "got: {m:?}"
        );
    }

    #[test]
    fn parse_claude_dash_prefix() {
        let m = Model::parse("claude-sonnet-4-6");
        assert!(
            matches!(m, Model::Claude(ref s) if s == "claude-sonnet-4-6"),
            "got: {m:?}"
        );
    }

    #[test]
    fn parse_unknown_defaults_to_lmstudio() {
        let m = Model::parse("some-local-model");
        assert!(
            matches!(m, Model::LMStudio(ref s) if s == "some-local-model"),
            "got: {m:?}"
        );
    }

    #[test]
    fn parse_empty_defaults_to_lmstudio() {
        let m = Model::parse("");
        assert!(
            matches!(m, Model::LMStudio(ref s) if s.is_empty()),
            "got: {m:?}"
        );
    }
}
