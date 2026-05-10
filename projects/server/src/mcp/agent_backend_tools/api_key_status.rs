use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tool::{OrcaTool, ToolCtx};

#[derive(Deserialize, JsonSchema)]
pub struct Args {}

pub struct AgentBackendApiKeyStatus;

#[async_trait]
impl OrcaTool for AgentBackendApiKeyStatus {
    const NAME: &'static str = "agent_backend_api_key_status";
    const DESCRIPTION: &'static str = "Report whether an Anthropic API key is stored in the encrypted orca DB. \
         Never echoes the raw key — only a masked preview.";
    type Args = Args;

    async fn run(_args: Args, _ctx: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        match db::settings::secret_get(&conn, "anthropic_api_key")? {
            Some(k) => Ok(format!("present: {}", db::settings::mask_key(&k))),
            None => Ok("absent".to_string()),
        }
    }
}
