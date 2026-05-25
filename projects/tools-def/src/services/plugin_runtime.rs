//! Service trait for the `plugin_runtime` domain
#![allow(clippy::disallowed_types)] // Plugin KV store is free-form by contract — each plugin owns its schema — generic per-plugin
//! encrypted KV store backed by the `plugin_data` table in orca.db.
//!
//! Values cross the trait as `serde_json::Value`. The server-side impl
//! handles the Value↔TEXT conversion at the storage edge so neither callers
//! nor REST clients have to stringify/parse manually.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait PluginRuntimeService: Send + Sync {
    /// Fetch a single key for a plugin. Errors when the key does not exist.
    /// Value is free-form by contract — each plugin defines its own KV schema.
    #[allow(clippy::disallowed_types)]
    async fn get(&self, plugin: &str, key: &str) -> Result<Value>;

    /// Upsert a single key for a plugin.
    /// Value is free-form by contract — each plugin defines its own KV schema.
    #[allow(clippy::disallowed_types)]
    async fn set(&self, plugin: &str, key: &str, value: &Value) -> Result<()>;
}

/// Embedder hook — see `services::mod` doc.
pub trait ProvidePluginRuntime {
    fn plugin_runtime(&self) -> std::sync::Arc<dyn PluginRuntimeService>;
}

/// Register a `PluginRuntimeService` into `ToolCtx`.
pub fn register_plugin_runtime(ctx: &mut orca_tool::ToolCtx, p: &impl ProvidePluginRuntime) {
    ctx.register_service(p.plugin_runtime());
}
