use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tool::{OrcaTool, ToolCtx};

use crate::agent_backend;

#[derive(Deserialize, JsonSchema)]
pub struct Args {}

pub struct AgentBackendStatus;

#[async_trait]
impl OrcaTool for AgentBackendStatus {
    const NAME: &'static str = "agent_backend_status";
    const DESCRIPTION: &'static str =
        "Show the current agent backend configuration: mode (local|claude|hybrid), \
         per-agent overrides, and whether server-side Anthropic calls are enabled.";
    type Args = Args;

    async fn run(_args: Args, _ctx: &ToolCtx) -> Result<String> {
        let mode = agent_backend::current_mode()?;
        let use_server = agent_backend::use_server_anthropic()?;
        let overrides = agent_backend::list_overrides()?;
        let conn = db::open_default()?;
        let key_present = db::secret_get(&conn, "anthropic_api_key")?.is_some();

        let mut out = String::new();
        out.push_str(&format!("mode: {}\n", mode.as_str()));
        out.push_str(&format!("use_server_anthropic: {use_server}\n"));
        out.push_str(&format!("api_key_in_db: {key_present}\n"));
        if overrides.is_empty() {
            out.push_str("overrides: (none)\n");
        } else {
            out.push_str("overrides:\n");
            for (agent, backend) in overrides {
                out.push_str(&format!("  @{agent} -> {backend}\n"));
            }
        }
        Ok(out)
    }
}
