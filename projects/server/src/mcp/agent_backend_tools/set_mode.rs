use anyhow::Result;
use async_trait::async_trait;
use orca_utils::tool::{OrcaTool, ToolCtx};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent_backend;

#[derive(Deserialize, JsonSchema)]
pub struct Args {
    /// "local" | "claude" | "hybrid"
    pub mode: String,
}

pub struct AgentBackendSetMode;

#[async_trait]
impl OrcaTool for AgentBackendSetMode {
    const NAME: &'static str = "agent_backend_set_mode";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Set the global agent backend mode. \
         local = always LM Studio. claude = always route to Claude (server-side if enabled, \
         else delegate to caller). hybrid = check per-agent override; default is Claude \
         when no override is set.";
    type Args = Args;
    type Output = String;

    async fn run(args: Args, _ctx: &ToolCtx) -> Result<String> {
        let mode = agent_backend::Mode::parse(&args.mode)?;
        agent_backend::set_mode(mode)?;
        Ok(format!("agent_backend.mode = {}", mode.as_str()))
    }
}
