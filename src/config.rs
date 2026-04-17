use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub anthropic_api_key: Option<String>,
    pub lmstudio_url: String,
    pub default_model: Model,
    pub brain_vault: PathBuf,
    pub memory_root: PathBuf,
}

#[derive(Debug, Clone)]
pub enum Model {
    Claude(String),
    LMStudio(String),
}

impl Model {
    pub fn display(&self) -> String {
        match self {
            Model::Claude(m) => format!("claude:{m}"),
            Model::LMStudio(m) => format!("lmstudio:{m}"),
        }
    }

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
        let brain_vault = home.join("brain");
        let memory_root = brain_vault.join("ai/claude/memory");

        // API key: env var takes priority, then macOS Keychain
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok()
            .or_else(crate::auth::load_api_key_from_keychain);

        let lmstudio_url = std::env::var("LMSTUDIO_URL")
            .unwrap_or_else(|_| "http://localhost:1234".to_string());

        // Always default to LM Studio. Claude is escalation-only.
        // Model ID resolved at session start from /v1/models.
        let default_model = Model::LMStudio(String::new());

        Ok(Config {
            anthropic_api_key: api_key,
            lmstudio_url,
            default_model,
            brain_vault,
            memory_root,
        })
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.brain_vault.join("ai/claude/agents")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.brain_vault.join("ai/claude/logs/sessions")
    }
}
