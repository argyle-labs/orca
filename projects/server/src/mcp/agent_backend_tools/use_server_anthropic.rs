use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::agent_backend::{
    AgentBackendUseServerAnthropic, UseServerAnthropicArgs, UseServerAnthropicResult,
};
use orca_utils::tool::{OrcaTool, ToolCtx};

use crate::agent_backend;

#[async_trait]
impl OrcaTool for AgentBackendUseServerAnthropic {
    async fn run(args: UseServerAnthropicArgs, _ctx: &ToolCtx) -> Result<UseServerAnthropicResult> {
        agent_backend::set_use_server_anthropic(args.enabled)?;
        Ok(UseServerAnthropicResult {
            enabled: args.enabled,
        })
    }
}
