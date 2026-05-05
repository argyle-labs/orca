use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tool::{OrcaTool, ToolCtx};

use crate::agent_backend;

#[derive(Deserialize, JsonSchema)]
pub struct Args {
    pub agent: String,
    /// "local" | "claude" | "clear" (clear removes the override)
    pub backend: String,
}

pub struct AgentBackendOverride;

#[async_trait]
impl OrcaTool for AgentBackendOverride {
    const NAME: &'static str = "agent_backend_override";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Set, change, or clear a per-agent backend override \
         (only consulted in hybrid mode). backend=clear deletes the override.";
    type Args = Args;

    async fn run(args: Args, _ctx: &ToolCtx) -> Result<String> {
        if args.backend == "clear" {
            let removed = agent_backend::clear_override(&args.agent)?;
            return Ok(if removed {
                format!("cleared override for @{}", args.agent)
            } else {
                format!("no override set for @{}", args.agent)
            });
        }

        if !orca_agents::list_embedded_agents()
            .iter()
            .any(|(name, _)| name == &args.agent)
        {
            anyhow::bail!("unknown agent: {}", args.agent);
        }

        agent_backend::set_override(&args.agent, &args.backend)?;
        Ok(format!("@{} -> {}", args.agent, args.backend))
    }
}
