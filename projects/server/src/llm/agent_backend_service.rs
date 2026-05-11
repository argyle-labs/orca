//! `AgentBackendService` impl that wraps `crate::llm::resolve::*`.
//!
//! Wired into `ToolCtx` from `mcp::build_tool_registry` so the four
//! agent_backend tools (which now live in `tools-def`) can dispatch
//! through the trait instead of calling server-internal modules.

use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::services::agent_backend::AgentBackendService;

use crate::llm::resolve;

pub struct ServerAgentBackend;

#[async_trait]
impl AgentBackendService for ServerAgentBackend {
    async fn current_mode(&self) -> Result<String> {
        Ok(resolve::current_mode()?.as_str().to_string())
    }

    async fn set_mode(&self, mode: &str) -> Result<String> {
        let parsed = resolve::Mode::parse(mode)?;
        resolve::set_mode(parsed)?;
        Ok(parsed.as_str().to_string())
    }

    async fn use_server_anthropic(&self) -> Result<bool> {
        resolve::use_server_anthropic()
    }

    async fn set_use_server_anthropic(&self, enabled: bool) -> Result<()> {
        resolve::set_use_server_anthropic(enabled)
    }

    async fn list_overrides(&self) -> Result<Vec<(String, String)>> {
        resolve::list_overrides()
    }

    async fn set_override(&self, agent: &str, backend: &str) -> Result<()> {
        resolve::set_override(agent, backend)
    }

    async fn clear_override(&self, agent: &str) -> Result<bool> {
        resolve::clear_override(agent)
    }

    async fn agent_exists(&self, agent: &str) -> Result<bool> {
        Ok(crate::agents::list_embedded_agents()
            .iter()
            .any(|(name, _)| name == agent))
    }

    async fn api_key_present(&self) -> Result<bool> {
        let conn = db::open_default()?;
        Ok(db::settings::secret_get(&conn, "anthropic_api_key")?.is_some())
    }
}
