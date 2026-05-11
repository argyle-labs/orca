//! Agent-backend API-key tools — defs + native impls in one file.
//!
//! Wasm-safe metadata is always compiled; the DB-backed `run` impls require
//! the `native` feature.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::OrcaToolDef;

#[derive(Deserialize, JsonSchema)]
pub struct ClearArgs {}

pub struct AgentBackendClearApiKey;
impl OrcaToolDef for AgentBackendClearApiKey {
    const NAME: &'static str = "agent_backend_clear_api_key";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove the stored Anthropic API key from the encrypted orca DB.";
    type Args = ClearArgs;
    type Output = String;
}

#[derive(Deserialize, JsonSchema)]
pub struct SetArgs {
    /// Anthropic API key (sk-ant-...)
    pub key: String,
}

pub struct AgentBackendSetApiKey;
impl OrcaToolDef for AgentBackendSetApiKey {
    const NAME: &'static str = "agent_backend_set_api_key";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Store an Anthropic API key in the encrypted orca DB \
         (settings table, key 'secrets.anthropic_api_key'). The DB is SQLCipher-encrypted \
         at rest. Required for server-side Anthropic calls.";
    type Args = SetArgs;
    type Output = String;
}

#[derive(Deserialize, JsonSchema)]
pub struct StatusArgs {}

pub struct AgentBackendApiKeyStatus;
impl OrcaToolDef for AgentBackendApiKeyStatus {
    const NAME: &'static str = "agent_backend_api_key_status";
    const DESCRIPTION: &'static str = "Report whether an Anthropic API key is stored in the encrypted orca DB. \
         Never echoes the raw key — only a masked preview.";
    type Args = StatusArgs;
    type Output = String;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_db as db;
    use orca_utils::tool::{OrcaTool, ToolCtx};

    #[async_trait]
    impl OrcaTool for AgentBackendClearApiKey {
        const NAME: &'static str = <Self as OrcaToolDef>::NAME;
        const DESCRIPTION: &'static str = <Self as OrcaToolDef>::DESCRIPTION;
        type Args = <Self as OrcaToolDef>::Args;
        type Output = <Self as OrcaToolDef>::Output;

        async fn run(_args: ClearArgs, _ctx: &ToolCtx) -> Result<String> {
            let conn = db::open_default()?;
            let removed = db::settings::secret_delete(&conn, "anthropic_api_key")?;
            Ok(if removed {
                "removed Anthropic API key from orca DB".to_string()
            } else {
                "no Anthropic API key was stored".to_string()
            })
        }
    }

    #[async_trait]
    impl OrcaTool for AgentBackendSetApiKey {
        const NAME: &'static str = <Self as OrcaToolDef>::NAME;
        const DESCRIPTION: &'static str = <Self as OrcaToolDef>::DESCRIPTION;
        type Args = <Self as OrcaToolDef>::Args;
        type Output = <Self as OrcaToolDef>::Output;

        async fn run(args: SetArgs, _ctx: &ToolCtx) -> Result<String> {
            if args.key.trim().is_empty() {
                anyhow::bail!("key must not be empty");
            }
            let conn = db::open_default()?;
            db::settings::secret_set(&conn, "anthropic_api_key", &args.key)?;
            Ok(format!(
                "stored Anthropic API key in encrypted orca DB ({})",
                db::settings::mask_key(&args.key)
            ))
        }
    }

    #[async_trait]
    impl OrcaTool for AgentBackendApiKeyStatus {
        const NAME: &'static str = <Self as OrcaToolDef>::NAME;
        const DESCRIPTION: &'static str = <Self as OrcaToolDef>::DESCRIPTION;
        type Args = <Self as OrcaToolDef>::Args;
        type Output = <Self as OrcaToolDef>::Output;

        async fn run(_args: StatusArgs, _ctx: &ToolCtx) -> Result<String> {
            let conn = db::open_default()?;
            match db::settings::secret_get(&conn, "anthropic_api_key")? {
                Some(k) => Ok(format!("present: {}", db::settings::mask_key(&k))),
                None => Ok("absent".to_string()),
            }
        }
    }
}
