//! Server metadata / lifecycle tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Deserialize, JsonSchema)]
pub struct HealthArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HealthOutput {
    pub ok: bool,
}

pub struct ApiHealth;
impl OrcaToolDef for ApiHealth {
    const NAME: &'static str = "api.health";
    const DESCRIPTION: &'static str =
        "Liveness probe — returns {ok: true} when the server is alive.";
    type Args = HealthArgs;
    type Output = HealthOutput;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};

    #[async_trait]
    impl OrcaTool for ApiHealth {
        const NAME: &'static str = <Self as OrcaToolDef>::NAME;
        const DESCRIPTION: &'static str = <Self as OrcaToolDef>::DESCRIPTION;
        type Args = <Self as OrcaToolDef>::Args;
        type Output = <Self as OrcaToolDef>::Output;
        async fn run(_args: HealthArgs, _ctx: &ToolCtx) -> Result<HealthOutput> {
            Ok(HealthOutput { ok: true })
        }
    }
}
