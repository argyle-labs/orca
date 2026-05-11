use anyhow::Result;
use async_trait::async_trait;
use orca_utils::tool::{OrcaTool, ToolCtx};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
pub struct Args {}

pub struct AgentBackendClearApiKey;

#[async_trait]
impl OrcaTool for AgentBackendClearApiKey {
    const NAME: &'static str = "agent_backend_clear_api_key";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove the stored Anthropic API key from the encrypted orca DB.";
    type Args = Args;
    type Output = String;

    async fn run(_args: Args, _ctx: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        let removed = db::settings::secret_delete(&conn, "anthropic_api_key")?;
        Ok(if removed {
            "removed Anthropic API key from orca DB".to_string()
        } else {
            "no Anthropic API key was stored".to_string()
        })
    }
}
