use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::agent_backend::{
    AgentBackendOverrideEntry, AgentBackendStatus, AgentBackendStatusArgs, AgentBackendStatusOutput,
};
use orca_utils::tool::{OrcaTool, ToolCtx};

use crate::agent_backend;

#[async_trait]
impl OrcaTool for AgentBackendStatus {
    async fn run(
        _args: AgentBackendStatusArgs,
        _ctx: &ToolCtx,
    ) -> Result<AgentBackendStatusOutput> {
        let mode = agent_backend::current_mode()?;
        let use_server = agent_backend::use_server_anthropic()?;
        let overrides = agent_backend::list_overrides()?;
        let conn = db::open_default()?;
        let key_present = db::settings::secret_get(&conn, "anthropic_api_key")?.is_some();
        Ok(AgentBackendStatusOutput {
            mode: mode.as_str().to_string(),
            use_server_anthropic: use_server,
            api_key_in_db: key_present,
            overrides: overrides
                .into_iter()
                .map(|(agent, backend)| AgentBackendOverrideEntry { agent, backend })
                .collect(),
        })
    }
}
