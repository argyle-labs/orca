//! `namespace.spec.detail` (formerly `LifecycleService::spec_dump`). Dumps
//! orca's own OpenAPI JSON. Lives in the server crate because the spec is
//! built from `crate::serve::openapi_spec_json()`.

use orca_macro::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SpecDetailReport {
    /// Orca's own OpenAPI JSON document, pretty-printed.
    pub spec: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SpecDetailArgs {}

/// Dump orca's own OpenAPI JSON document. Used by build pipelines that don't want to spin up the HTTP server.
#[orca_tool(domain = "namespace.spec", verb = "detail")]
async fn spec_detail(
    _args: SpecDetailArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SpecDetailReport> {
    let spec = crate::serve::openapi_spec_json();
    Ok(SpecDetailReport {
        spec: serde_json::to_string_pretty(&spec)?,
    })
}
