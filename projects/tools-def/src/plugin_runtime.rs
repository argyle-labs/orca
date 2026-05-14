//! Plugin runtime KV — typed tool defs for `get_plugin_data` and
//! `set_plugin_data`. Values are arbitrary JSON; the underlying TEXT column
//! holds the JSON-stringified form but callers work with structured data.
//!
//! `serde_json::Value` is used intentionally here — the plugin KV store is
//! free-form by contract; each plugin defines its own per-key schema.
#![allow(clippy::disallowed_types)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::orca_tool;

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetPluginDataArgs {
    pub plugin: String,
    pub key: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetPluginDataOutput {
    /// Stored value — arbitrary JSON. Stored as TEXT in orca.db; the
    /// host parses/serializes at the edge so callers never see a string.
    #[cfg_attr(feature = "wasm", tsify(type = "unknown"))]
    pub value: Value,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetPluginDataArgs {
    pub plugin: String,
    pub key: String,
    /// Arbitrary JSON value — the host serializes it to TEXT at the storage edge.
    #[cfg_attr(feature = "wasm", tsify(type = "unknown"))]
    pub value: Value,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetPluginDataOutput {
    pub ok: bool,
}

#[cfg(feature = "native")]
fn pr(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::plugin_runtime::PluginRuntimeService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::plugin_runtime::PluginRuntimeService>>()
}

/// Read a single key from a plugin's encrypted KV store in orca.db.
#[orca_tool(domain = "plugin-data", verb = "get")]
async fn get_plugin_data(
    args: GetPluginDataArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<GetPluginDataOutput> {
    let value = pr(ctx)?.get(&args.plugin, &args.key).await?;
    Ok(GetPluginDataOutput { value })
}

/// [MUTATES STATE] Upsert a single key in a plugin's encrypted KV store in orca.db.
#[orca_tool(domain = "plugin-data", verb = "set", cli = skip)]
async fn set_plugin_data(
    args: SetPluginDataArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SetPluginDataOutput> {
    pr(ctx)?.set(&args.plugin, &args.key, &args.value).await?;
    Ok(SetPluginDataOutput { ok: true })
}
