//! Service trait for the agent_backend domain.
//!
//! Server impls this by wrapping `crate::llm::resolve::*`. Tools dispatch
//! through `ctx.service::<Arc<dyn AgentBackendService>>()`.

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait AgentBackendService: Send + Sync {
    /// Current global mode as its canonical string ("local"|"claude"|"hybrid").
    async fn current_mode(&self) -> Result<String>;

    /// Set the global mode. Accepts the same strings as `current_mode`
    /// returns. Returns the parsed canonical mode string for echo.
    async fn set_mode(&self, mode: &str) -> Result<String>;

    async fn use_server_anthropic(&self) -> Result<bool>;
    async fn set_use_server_anthropic(&self, enabled: bool) -> Result<()>;

    /// All per-agent overrides as (agent, backend) pairs.
    async fn list_overrides(&self) -> Result<Vec<(String, String)>>;

    async fn set_override(&self, agent: &str, backend: &str) -> Result<()>;

    /// Remove a per-agent override. Returns `true` if one was present.
    async fn clear_override(&self, agent: &str) -> Result<bool>;

    /// Validate an agent name against the embedded agent set.
    async fn agent_exists(&self, agent: &str) -> Result<bool>;

    /// Whether an Anthropic API key is currently stored in the encrypted DB.
    async fn api_key_present(&self) -> Result<bool>;
}
