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

use crate::OrcaToolDef;

// ── get_plugin_data ─────────────────────────────────────────────────────────

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

pub struct GetPluginData;
impl OrcaToolDef for GetPluginData {
    const NAME: &'static str = "get_plugin_data";
    const DESCRIPTION: &'static str =
        "Read a single key from a plugin's encrypted KV store in orca.db.";
    type Args = GetPluginDataArgs;
    type Output = GetPluginDataOutput;
}

// ── set_plugin_data ─────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetPluginDataArgs {
    pub plugin: String,
    pub key: String,
    /// Arbitrary JSON value — the host serializes it to TEXT at the storage edge.
    #[cfg_attr(feature = "wasm", tsify(type = "unknown"))]
    #[cfg_attr(feature = "cli", arg(skip))]
    pub value: Value,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
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
