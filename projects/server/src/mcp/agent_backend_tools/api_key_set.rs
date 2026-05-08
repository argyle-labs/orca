use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tool::{OrcaTool, ToolCtx};

#[derive(Deserialize, JsonSchema)]
pub struct Args {
    /// Anthropic API key (sk-ant-...)
    pub key: String,
}

pub struct AgentBackendSetApiKey;

#[async_trait]
impl OrcaTool for AgentBackendSetApiKey {
    const NAME: &'static str = "agent_backend_set_api_key";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Store an Anthropic API key in the encrypted orca DB \
         (settings table, key 'secrets.anthropic_api_key'). The DB is SQLCipher-encrypted \
         at rest. Required for server-side Anthropic calls.";
    type Args = Args;

    async fn run(args: Args, _ctx: &ToolCtx) -> Result<String> {
        if args.key.trim().is_empty() {
            anyhow::bail!("key must not be empty");
        }
        let conn = db::open_default()?;
        db::secret_set(&conn, "anthropic_api_key", &args.key)?;
        Ok(format!(
            "stored Anthropic API key in encrypted orca DB ({})",
            db::mask_key(&args.key)
        ))
    }
}
