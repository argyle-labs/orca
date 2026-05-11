//! Server-side impl of `PluginRuntimeService` — thin shim over
//! `db::plugin_data` for the `get_plugin_data` and `set_plugin_data` tools.
//!
//! The DB column is TEXT (JSON-stringified). We parse on read and serialize
//! on write so the trait surface stays typed (`serde_json::Value`).

use anyhow::{Context, Result};
use async_trait::async_trait;
use orca_tools_def::services::plugin_runtime::PluginRuntimeService;
use serde_json::Value;

pub struct ServerPluginRuntime;

#[async_trait]
impl PluginRuntimeService for ServerPluginRuntime {
    async fn get(&self, plugin: &str, key: &str) -> Result<Value> {
        let conn = db::open_default()?;
        match db::plugin_data::get(&conn, plugin, key)? {
            Some(row) => serde_json::from_str(&row.value)
                .with_context(|| format!("plugin_data row for {plugin}/{key} is not valid JSON")),
            None => anyhow::bail!("key '{key}' not found for plugin '{plugin}'"),
        }
    }

    async fn set(&self, plugin: &str, key: &str, value: &Value) -> Result<()> {
        let conn = db::open_default()?;
        let text = serde_json::to_string(value)?;
        db::plugin_data::set(&conn, plugin, key, &text)
    }
}
