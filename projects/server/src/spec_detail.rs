//! `namespace.spec.detail` (formerly `LifecycleService::spec_dump`). Dumps
//! orca's own OpenAPI JSON. Lives in the server crate because the spec is
//! built from `crate::serve::openapi::orca_spec_json()`.

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SpecDetailReport {
    /// Orca's own OpenAPI JSON document, pretty-printed.
    pub spec: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct SpecDetailArgs {}

/// Dump orca's own OpenAPI JSON document. Used by build pipelines that don't want to spin up the HTTP server.
#[orca_tool(domain = "spec", verb = "detail")]
async fn spec_detail(
    _args: SpecDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SpecDetailReport> {
    let spec = crate::serve::openapi::orca_spec_json();
    Ok(SpecDetailReport {
        spec: serde_json::to_string_pretty(&spec)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::ToolCtx;
    use contract::config::{Config, Model};
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
            db_path: PathBuf::from("/tmp/orca-spec-detail-test.db"),
            ports: Default::default(),
        }))
    }

    #[tokio::test]
    async fn spec_detail_returns_valid_json_openapi_doc() {
        let ctx = empty_ctx();
        let out = spec_detail(SpecDetailArgs {}, &ctx).await.unwrap();
        #[derive(serde::Deserialize)]
        struct Shape {
            openapi: String,
            paths: std::collections::BTreeMap<String, serde::de::IgnoredAny>,
        }
        let v: Shape = serde_json::from_str(&out.spec).unwrap();
        assert!(!v.openapi.is_empty(), "missing openapi field");
        assert!(!v.paths.is_empty(), "missing paths field");
    }
}
