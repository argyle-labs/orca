//! Service trait for the `plugin_runtime` domain — generic per-plugin
//! encrypted KV store backed by the `plugin_data` table in orca.db.
//!
//! Values are stored as opaque strings (typically JSON-stringified by the
//! plugin itself). The trait keeps the wire shape as `String` to match the
//! REST surface; callers that want structured data should JSON.parse client-side.

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait PluginRuntimeService: Send + Sync {
    /// Fetch a single key for a plugin. Errors when the key does not exist.
    async fn get(&self, plugin: &str, key: &str) -> Result<String>;

    /// Upsert a single key for a plugin.
    async fn set(&self, plugin: &str, key: &str, value: &str) -> Result<()>;
}
