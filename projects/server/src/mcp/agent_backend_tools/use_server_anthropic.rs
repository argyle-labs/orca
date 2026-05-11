use anyhow::Result;
use async_trait::async_trait;
use orca_utils::tool::{OrcaTool, ToolCtx};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent_backend;

#[derive(Deserialize, JsonSchema)]
pub struct Args {
    pub enabled: bool,
}

pub struct AgentBackendUseServerAnthropic;

#[async_trait]
impl OrcaTool for AgentBackendUseServerAnthropic {
    const NAME: &'static str = "agent_backend_use_server_anthropic";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Toggle whether the orca server makes Anthropic API calls directly \
         when the resolver picks Claude. When false (default), Claude-routed agents return \
         a delegate-to-claude-code envelope instead. Requires a stored API key when true.";
    type Args = Args;

    async fn run(args: Args, _ctx: &ToolCtx) -> Result<String> {
        agent_backend::set_use_server_anthropic(args.enabled)?;
        Ok(format!(
            "agent_backend.use_server_anthropic = {}",
            args.enabled
        ))
    }
}
