//! Server metadata / lifecycle tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HealthArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HealthOutput {
    pub ok: bool,
}

/// Liveness probe — returns {ok: true} when the server is alive.
#[orca_tool(domain = "system", verb = "health")]
async fn health(
    _args: HealthArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<HealthOutput> {
    Ok(HealthOutput { ok: true })
}
