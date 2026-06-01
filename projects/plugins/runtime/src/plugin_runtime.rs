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

use derive::orca_tool;

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetPluginDataArgs {
    pub plugin: String,
    pub key: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetPluginDataOutput {
    /// Stored value — arbitrary JSON. Stored as TEXT in orca.db; the
    /// host parses/serializes at the edge so callers never see a string.
    pub value: Value,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetPluginDataArgs {
    pub plugin: String,
    pub key: String,
    /// Arbitrary JSON value — the host serializes it to TEXT at the storage edge.
    pub value: Value,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetPluginDataOutput {
    pub ok: bool,
}

/// Read a single key from a plugin's encrypted KV store in orca.db.
#[orca_tool(domain = "plugin.data", verb = "get")]
async fn get_plugin_data(
    args: GetPluginDataArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<GetPluginDataOutput> {
    use anyhow::Context;
    let conn = db::open_default()?;
    let value = match db::plugin_data::get(&conn, &args.plugin, &args.key)? {
        Some(row) => serde_json::from_str::<Value>(&row.value).with_context(|| {
            format!(
                "plugin_data row for {}/{} is not valid JSON",
                args.plugin, args.key
            )
        })?,
        None => anyhow::bail!("key '{}' not found for plugin '{}'", args.key, args.plugin),
    };
    Ok(GetPluginDataOutput { value })
}

/// [MUTATES STATE] Upsert a single key in a plugin's encrypted KV store in orca.db.
#[orca_tool(domain = "plugin.data", verb = "set", cli = skip)]
async fn set_plugin_data(
    args: SetPluginDataArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SetPluginDataOutput> {
    let conn = db::open_default()?;
    let text = serde_json::to_string(&args.value)?;
    db::plugin_data::set(&conn, &args.plugin, &args.key, &text)?;
    Ok(SetPluginDataOutput { ok: true })
}
