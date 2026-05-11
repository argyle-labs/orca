use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::agent_backend::{AgentBackendOverride, OverrideArgs, OverrideResult};
use orca_utils::tool::{OrcaTool, ToolCtx};

use crate::agent_backend;

#[async_trait]
impl OrcaTool for AgentBackendOverride {
    async fn run(args: OverrideArgs, _ctx: &ToolCtx) -> Result<OverrideResult> {
        if args.backend == "clear" {
            let removed = agent_backend::clear_override(&args.agent)?;
            return Ok(OverrideResult {
                agent: args.agent,
                backend: "none".to_string(),
                cleared: removed,
            });
        }

        if !crate::agents::list_embedded_agents()
            .iter()
            .any(|(name, _)| name == &args.agent)
        {
            anyhow::bail!("unknown agent: {}", args.agent);
        }

        agent_backend::set_override(&args.agent, &args.backend)?;
        Ok(OverrideResult {
            agent: args.agent,
            backend: args.backend,
            cleared: false,
        })
    }
}
