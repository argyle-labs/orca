use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub anthropic_api_key: Option<String>,
    pub lmstudio_url: String,
    pub default_model: Model,
    pub brain_vault: PathBuf,
    pub memory_root: PathBuf,
    pub db_path: PathBuf,
}

#[derive(Debug, Clone)]
pub enum Model {
    Claude(String),
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
        let brain_vault = home.join(".brain");
        let memory_root = brain_vault.join("memory");
        let db_path = brain_vault.join("brain.db");

        // API key: env var takes priority, then macOS Keychain
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .or_else(crate::auth::load_api_key_from_keychain);

        let lmstudio_url =
            std::env::var("LMSTUDIO_URL").unwrap_or_else(|_| "http://localhost:1234".to_string());

        // Always default to LM Studio. Claude is escalation-only.
        // Model ID resolved at session start from /v1/models.
        let default_model = Model::LMStudio(String::new());

        // One-time migration: if brain.toml has [[mcp.servers]], write them to DB.
        let toml_path = brain_vault.join("brain.toml");
        if toml_path.exists() {
            migrate_toml_servers_to_db(&toml_path, &db_path);
        }

        Ok(Config {
            anthropic_api_key: api_key,
            lmstudio_url,
            default_model,
            brain_vault,
            memory_root,
            db_path,
        })
    }

    pub fn brain_toml_path(&self) -> PathBuf {
        self.brain_vault.join("brain.toml")
    }

    pub fn agents_dir(&self) -> PathBuf {
        dirs::home_dir().unwrap_or_default().join(".claude/agents")
    }

    pub fn logs_dir(&self) -> PathBuf {
        dirs::home_dir().unwrap_or_default().join(".brain/logs/sessions")
    }

    pub fn config_dir(&self) -> PathBuf {
        dirs::home_dir().unwrap_or_default().join("code/brain/config")
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
        "migrated {} mcp server(s) from brain.toml to brain.db",
        parsed.mcp.servers.len()
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
