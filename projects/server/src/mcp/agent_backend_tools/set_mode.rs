use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::agent_backend::{AgentBackendSetMode, SetModeArgs, SetModeResult};
use orca_utils::tool::{OrcaTool, ToolCtx};

use crate::agent_backend;

#[async_trait]
impl OrcaTool for AgentBackendSetMode {
    async fn run(args: SetModeArgs, _ctx: &ToolCtx) -> Result<SetModeResult> {
        let mode = agent_backend::Mode::parse(&args.mode)?;
        agent_backend::set_mode(mode)?;
        Ok(SetModeResult {
            mode: mode.as_str().to_string(),
        })
    }
}
