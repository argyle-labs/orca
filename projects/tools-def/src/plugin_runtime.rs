//! Plugin runtime KV — typed tool defs for `get_plugin_data` and
//! `set_plugin_data`. Values are opaque strings (typically JSON-stringified)
//! to match the REST + DB surface.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

// ── get_plugin_data ─────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetPluginDataArgs {
    pub plugin: String,
    pub key: String,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetPluginDataOutput {
    /// Stored value — opaque string. Typically JSON-stringified by the plugin.
    pub value: String,
}

pub struct GetPluginData;
impl OrcaToolDef for GetPluginData {
    const NAME: &'static str = "get_plugin_data";
    const DESCRIPTION: &'static str =
        "Read a single key from a plugin's encrypted KV store in orca.db.";
    type Args = GetPluginDataArgs;
    type Output = GetPluginDataOutput;
}

// ── set_plugin_data ─────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetPluginDataArgs {
    pub plugin: String,
    pub key: String,
    /// Opaque value — encoded by the caller (usually `JSON.stringify(obj)`).
    pub value: String,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetPluginDataOutput {
    pub ok: bool,
}

pub struct SetPluginData;
impl OrcaToolDef for SetPluginData {
    const NAME: &'static str = "set_plugin_data";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Upsert a single key in a plugin's encrypted KV store in orca.db.";
    type Args = SetPluginDataArgs;
    type Output = SetPluginDataOutput;
}

// ═══════════════════════════════════════════════════════════════════════════
// Native run impls
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::plugin_runtime as svc;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn pr(ctx: &ToolCtx) -> Result<Arc<dyn svc::PluginRuntimeService>> {
        ctx.service::<Arc<dyn svc::PluginRuntimeService>>()
    }

    #[async_trait]
    impl OrcaTool for GetPluginData {
        async fn run(args: GetPluginDataArgs, ctx: &ToolCtx) -> Result<GetPluginDataOutput> {
            let value = pr(ctx)?.get(&args.plugin, &args.key).await?;
            Ok(GetPluginDataOutput { value })
        }
    }

    #[async_trait]
    impl OrcaTool for SetPluginData {
        async fn run(args: SetPluginDataArgs, ctx: &ToolCtx) -> Result<SetPluginDataOutput> {
            pr(ctx)?.set(&args.plugin, &args.key, &args.value).await?;
            Ok(SetPluginDataOutput { ok: true })
        }
    }
}
