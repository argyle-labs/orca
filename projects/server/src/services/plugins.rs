//! `PluginsService` impl — wraps `db::plugins`, `db::plugin_creds`, and the
//! `commands::plugin_cmd` / `commands::creds_cmd` install/sync routines.

use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::services::plugins::{PluginCredSummary, PluginSummary, PluginsService};

pub struct ServerPlugins;

#[async_trait]
impl PluginsService for ServerPlugins {
    async fn list_plugins(&self, workspace: Option<&str>) -> Result<Vec<PluginSummary>> {
        let conn = db::open_default()?;
        let rows = db::plugins::list(&conn)?;
        Ok(rows
            .into_iter()
            .filter(|p| workspace.is_none_or(|w| p.tier == w))
            .map(|p| PluginSummary {
                id: p.id,
                tier: p.tier,
                mode: p.mode,
                mcp_command: p.mcp_command,
                enabled: p.enabled,
            })
            .collect())
    }

    async fn install_plugin(&self, manifest: &str, instance_id: Option<&str>) -> Result<String> {
        crate::commands::install_plugin(manifest, instance_id)
    }

    async fn remove_plugin(&self, id: &str) -> Result<bool> {
        crate::commands::remove_plugin(id)
    }

    async fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let conn = db::open_default()?;
        db::plugins::set_enabled(&conn, id, enabled)
    }

    async fn list_plugin_creds(&self, plugin: &str) -> Result<Vec<PluginCredSummary>> {
        let conn = db::open_default()?;
        let creds = db::plugin_creds::list(&conn, plugin)?;
        Ok(creds
            .into_iter()
            .map(|c| PluginCredSummary {
                key: c.key,
                synced_at: c.synced_at,
                updated_at: c.updated_at,
            })
            .collect())
    }

    async fn set_plugin_cred(&self, plugin: &str, key: &str, value: &str) -> Result<()> {
        let conn = db::open_default()?;
        db::plugin_creds::set(&conn, plugin, key, value)
    }

    async fn remove_plugin_cred(&self, plugin: &str, key: &str) -> Result<bool> {
        let conn = db::open_default()?;
        db::plugin_creds::delete(&conn, plugin, key)
    }

    async fn sync_plugin_creds(&self, plugin: &str) -> Result<()> {
        crate::commands::creds_cmd::sync_plugin_creds(plugin)
    }
}
