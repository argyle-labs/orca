//! Server metadata / lifecycle tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HealthArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HealthOutput {
    pub ok: bool,
}

/// Liveness probe — returns {ok: true} when the server is alive.
#[orca_tool(domain = "system", verb = "health")]
async fn health(_args: HealthArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<HealthOutput> {
    Ok(HealthOutput { ok: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::ToolCtx;
    use std::path::PathBuf;
    use std::sync::Arc;
    use utils::config::{Config, Model};

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
