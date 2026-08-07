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

/// Which spec surface `spec.detail` reports. `openapi` (default) dumps orca's
/// own OpenAPI JSON; `graphql` parses a registered `<repo>.graphql` SDL.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default,
)]
#[serde(rename_all = "camelCase")]
pub enum SpecDetailFormat {
    #[default]
    Openapi,
    Graphql,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SpecDetailArgs {
    /// Which spec surface to report. Defaults to `openapi`.
    #[arg(long, value_enum, default_value = "openapi")]
    #[serde(default)]
    pub format: SpecDetailFormat,
    /// `format=graphql`: the repo whose `<repo>.graphql` schema to parse.
    #[arg(long)]
    #[serde(default)]
    pub repo: Option<String>,
}

/// `spec.detail` payload — one variant per `format`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SpecDetailOutput {
    Openapi(SpecDetailReport),
    Graphql(spec::GraphQlInfoData),
}

/// Detail for a spec surface. `format=openapi` (default) dumps orca's own OpenAPI
/// JSON document (used by build pipelines that don't want to spin up the HTTP
/// server); `format=graphql` parses a registered `<repo>.graphql` SDL into a
/// structured types/queries/mutations view.
#[orca_tool(domain = "spec", verb = "detail")]
async fn spec_detail(
    args: SpecDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SpecDetailOutput> {
    match args.format {
        SpecDetailFormat::Openapi => {
            let spec = crate::serve::openapi::orca_spec_json();
            Ok(SpecDetailOutput::Openapi(SpecDetailReport {
                spec: serde_json::to_string_pretty(&spec)?,
            }))
        }
        SpecDetailFormat::Graphql => {
            let repo = args
                .repo
                .ok_or_else(|| anyhow::anyhow!("`repo` is required for format=graphql"))?;
            Ok(SpecDetailOutput::Graphql(
                spec::graphql_detail(&repo).await?,
            ))
        }
    }
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
        let out = spec_detail(SpecDetailArgs::default(), &ctx).await.unwrap();
        let SpecDetailOutput::Openapi(report) = out else {
            panic!("default format should be openapi");
        };
        #[derive(serde::Deserialize)]
        struct Shape {
            openapi: String,
            paths: std::collections::BTreeMap<String, serde::de::IgnoredAny>,
        }
        let v: Shape = serde_json::from_str(&report.spec).unwrap();
        assert!(!v.openapi.is_empty(), "missing openapi field");
        assert!(!v.paths.is_empty(), "missing paths field");
    }
}
