//! Runtime configuration for the orca binary.
//!
//! `Config::load()` reads paths and env vars only — no DB access.
//! DB startup (migrations, API key loading) is handled by `db::startup`.

mod consts;
pub use consts::*;

use anyhow::{Context, Result};
use std::path::PathBuf;

/// All runtime configuration for the orca binary.
///
/// Static config (API keys, LLM endpoints) lives here.
/// Dynamic registries (MCP servers, Docker runtimes, etc.) live in `orca.db` — see the db crate.
#[derive(Debug, Clone)]
pub struct Config {
    pub anthropic_api_key: Option<String>,
    pub lmstudio_url: String,
    pub ollama_url: String,
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
/// The `url` field on LMStudio/Ollama is empty when loaded from env/config —
/// `build_backend` then falls back to the global config URL. When populated
/// from discovery it carries the specific endpoint that answered.
#[derive(Debug, Clone)]
pub enum Model {
    /// Anthropic Claude API — requires `ANTHROPIC_API_KEY` or a DB secret entry.
    Claude(String),
    /// LM Studio (OpenAI-compatible local server) — no API key needed.
    LMStudio { id: String, url: String },
    /// Ollama (OpenAI-compatible local/network server) — no API key needed.
    Ollama { id: String, url: String },
}

impl Model {
    /// Parse a /model <spec> argument.
    /// Accepts: "claude-sonnet-4-6", "claude:claude-sonnet-4-6", "lmstudio:model-id", "ollama:model-id"
    pub fn parse(s: &str) -> Self {
        if let Some(m) = s.strip_prefix("lmstudio:") {
            Model::LMStudio { id: m.to_string(), url: String::new() }
        } else if let Some(m) = s.strip_prefix("ollama:") {
            Model::Ollama { id: m.to_string(), url: String::new() }
        } else if let Some(m) = s.strip_prefix("claude:") {
            Model::Claude(m.to_string())
        } else if s.starts_with("claude-") {
            Model::Claude(s.to_string())
        } else {
            Model::LMStudio { id: s.to_string(), url: String::new() }
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Model::Claude(id) => id,
            Model::LMStudio { id, .. } | Model::Ollama { id, .. } => id,
        }
    }
}

impl Config {
    /// Load config from the environment and filesystem paths only.
    /// No DB access — call `db::startup::init` after this to run migrations
    /// and `db::startup::load_api_key` to populate `anthropic_api_key` from
    /// the encrypted DB when no env var is set.
    pub fn load() -> Result<Self> {
        let home = dirs::home_dir().context("no home dir")?;
        let orca_vault = home.join(consts::APP_STATE_DIR);
        let vault_root = std::env::var("ORCA_VAULT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("orca"));
        let memory_root = orca_vault.join("memory");
        let db_path = orca_vault.join(consts::APP_DB_FILE);

        let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
        let lmstudio_url =
            std::env::var("LMSTUDIO_URL").unwrap_or_else(|_| "http://localhost:1234".to_string());
        let ollama_url =
            std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".to_string());

        Ok(Config {
            anthropic_api_key: api_key,
            lmstudio_url,
            ollama_url,
            default_model: Model::LMStudio { id: String::new(), url: String::new() },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lmstudio_prefix() {
        let m = Model::parse("lmstudio:qwen3");
        assert!(
            matches!(m, Model::LMStudio { ref id, .. } if id == "qwen3"),
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
            matches!(m, Model::LMStudio { ref id, .. } if id == "some-local-model"),
            "got: {m:?}"
        );
    }

    #[test]
    fn parse_empty_defaults_to_lmstudio() {
        let m = Model::parse("");
        assert!(
            matches!(m, Model::LMStudio { ref id, .. } if id.is_empty()),
            "got: {m:?}"
        );
    }
}
