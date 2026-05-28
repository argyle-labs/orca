//! Server metadata / lifecycle tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use orca_macro::orca_tool;

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HealthArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HealthOutput {
    pub ok: bool,
}

/// Liveness probe — returns {ok: true} when the server is alive.
#[orca_tool(domain = "system", verb = "health", remote_ok = true)]
async fn health(_args: HealthArgs, _ctx: &orca_contract::ToolCtx) -> anyhow::Result<HealthOutput> {
    Ok(HealthOutput { ok: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_contract::ToolCtx;
    use orca_utils::config::{Config, Model};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn empty_ctx() -> ToolCtx {
        ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/orca-fleet-meta-test.db"),
            ports: Default::default(),
        }))
    }

    #[tokio::test]
    async fn health_returns_ok_true() {
        let ctx = empty_ctx();
        let out = health(HealthArgs {}, &ctx).await.unwrap();
        assert!(out.ok);
    }
}
