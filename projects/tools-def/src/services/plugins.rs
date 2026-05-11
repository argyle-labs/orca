//! Service trait for the `plugins` domain — plugin registry + credentials.

use anyhow::Result;
use async_trait::async_trait;

#[derive(Clone)]
pub struct PluginSummary {
    pub id: String,
    pub tier: String,
    pub mode: String,
    pub mcp_command: Option<String>,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct PluginCredSummary {
    pub key: String,
    pub synced_at: Option<String>,
    pub updated_at: String,
}

#[async_trait]
pub trait PluginsService: Send + Sync {
    async fn list_plugins(&self, workspace: Option<&str>) -> Result<Vec<PluginSummary>>;

    /// Install a plugin from a manifest path or URL. Returns the resolved id.
    async fn install_plugin(&self, manifest: &str, instance_id: Option<&str>) -> Result<String>;

    /// Returns `true` when a plugin was removed, `false` when none matched `id`.
    async fn remove_plugin(&self, id: &str) -> Result<bool>;

    /// Returns `true` when the plugin existed and was toggled, `false` when
    /// no plugin matched `id`.
    async fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<bool>;

    async fn list_plugin_creds(&self, plugin: &str) -> Result<Vec<PluginCredSummary>>;
    async fn set_plugin_cred(&self, plugin: &str, key: &str, value: &str) -> Result<()>;

    /// Returns `true` when the credential existed and was removed.
    async fn remove_plugin_cred(&self, plugin: &str, key: &str) -> Result<bool>;

    async fn sync_plugin_creds(&self, plugin: &str) -> Result<()>;
}
