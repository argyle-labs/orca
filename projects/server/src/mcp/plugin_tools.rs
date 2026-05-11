use anyhow::Result;
use async_trait::async_trait;
use orca_utils::tool::{OrcaTool, OrcaToolDef, ToolCtx};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::mcp::handlers;

// ── list_plugins ──────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ListPluginsArgs {
    /// Filter by workspace tier (omit for all)
    pub workspace: Option<String>,
}
pub struct ListPlugins;
impl OrcaToolDef for ListPlugins {
    const NAME: &'static str = "list_plugins";
    const DESCRIPTION: &'static str = "List all orca plugins registered in orca.db.";
    type Args = ListPluginsArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for ListPlugins {
    async fn run(args: ListPluginsArgs, _: &ToolCtx) -> Result<String> {
        use serde_json::json;
        handlers::plugin_list(&json!({ "workspace": args.workspace }))
    }
}
// ── add_plugin ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct AddPluginArgs {
    /// Path or URL to plugin manifest
    pub manifest: String,
    /// Optional instance ID override
    pub instance_id: Option<String>,
}
pub struct AddPlugin;
impl OrcaToolDef for AddPlugin {
    const NAME: &'static str = "add_plugin";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Install an orca plugin from a manifest path or URL.";
    type Args = AddPluginArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for AddPlugin {
    async fn run(args: AddPluginArgs, _: &ToolCtx) -> Result<String> {
        let id = crate::commands::install_plugin(&args.manifest, args.instance_id.as_deref())?;
        Ok(format!("Plugin '{id}' installed successfully."))
    }
}
// ── remove_plugin ─────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct RemovePluginArgs {
    pub id: String,
}
pub struct RemovePlugin;
impl OrcaToolDef for RemovePlugin {
    const NAME: &'static str = "remove_plugin";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Remove an installed orca plugin by ID.";
    type Args = RemovePluginArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for RemovePlugin {
    async fn run(args: RemovePluginArgs, _: &ToolCtx) -> Result<String> {
        if crate::commands::remove_plugin(&args.id)? {
            Ok(format!("Plugin '{}' removed.", args.id))
        } else {
            Ok(format!("Plugin '{}' not found.", args.id))
        }
    }
}
// ── enable_plugin / disable_plugin ────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct PluginIdArgs {
    pub id: String,
}

pub struct EnablePlugin;
impl OrcaToolDef for EnablePlugin {
    const NAME: &'static str = "enable_plugin";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Enable a registered orca plugin.";
    type Args = PluginIdArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for EnablePlugin {
    async fn run(args: PluginIdArgs, _: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        if db::plugins::set_enabled(&conn, &args.id, true)? {
            Ok(format!("Plugin '{}' enabled.", args.id))
        } else {
            Ok(format!("Plugin '{}' not found.", args.id))
        }
    }
}
pub struct DisablePlugin;
impl OrcaToolDef for DisablePlugin {
    const NAME: &'static str = "disable_plugin";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Disable a registered orca plugin.";
    type Args = PluginIdArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for DisablePlugin {
    async fn run(args: PluginIdArgs, _: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        if db::plugins::set_enabled(&conn, &args.id, false)? {
            Ok(format!("Plugin '{}' disabled.", args.id))
        } else {
            Ok(format!("Plugin '{}' not found.", args.id))
        }
    }
}
// ── plugin_creds ──────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ListPluginCredsArgs {
    pub plugin: String,
}
pub struct ListPluginCreds;
impl OrcaToolDef for ListPluginCreds {
    const NAME: &'static str = "list_plugin_creds";
    const DESCRIPTION: &'static str =
        "List all stored credentials for a plugin (keys only — values are never returned).";
    type Args = ListPluginCredsArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for ListPluginCreds {
    async fn run(args: ListPluginCredsArgs, _: &ToolCtx) -> Result<String> {
        use serde_json::json;
        handlers::plugin_creds_list(&json!({ "plugin": args.plugin }))
    }
}
#[derive(Deserialize, JsonSchema)]
pub struct SetPluginCredArgs {
    pub plugin: String,
    pub key: String,
    pub value: String,
}
pub struct SetPluginCred;
impl OrcaToolDef for SetPluginCred {
    const NAME: &'static str = "set_plugin_cred";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Store a credential value for a plugin in orca.db.";
    type Args = SetPluginCredArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for SetPluginCred {
    async fn run(args: SetPluginCredArgs, _: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        db::plugin_creds::set(&conn, &args.plugin, &args.key, &args.value)?;
        Ok(format!(
            "Stored credential '{}' for plugin '{}'.",
            args.key, args.plugin
        ))
    }
}
#[derive(Deserialize, JsonSchema)]
pub struct RemovePluginCredArgs {
    pub plugin: String,
    pub key: String,
}
pub struct RemovePluginCred;
impl OrcaToolDef for RemovePluginCred {
    const NAME: &'static str = "remove_plugin_cred";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a stored credential for a plugin from orca.db.";
    type Args = RemovePluginCredArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for RemovePluginCred {
    async fn run(args: RemovePluginCredArgs, _: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        if db::plugin_creds::delete(&conn, &args.plugin, &args.key)? {
            Ok(format!(
                "Removed credential '{}' from plugin '{}'.",
                args.key, args.plugin
            ))
        } else {
            Ok(format!(
                "Credential '{}' not found for plugin '{}'.",
                args.key, args.plugin
            ))
        }
    }
}
#[derive(Deserialize, JsonSchema)]
pub struct SyncPluginCredsArgs {
    pub plugin: String,
}
pub struct SyncPluginCreds;
impl OrcaToolDef for SyncPluginCreds {
    const NAME: &'static str = "sync_plugin_creds";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Sync stored credentials for a plugin to its runtime environment.";
    type Args = SyncPluginCredsArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for SyncPluginCreds {
    async fn run(args: SyncPluginCredsArgs, _: &ToolCtx) -> Result<String> {
        crate::commands::creds_cmd::sync_plugin_creds(&args.plugin)?;
        Ok(format!("Synced credentials for plugin '{}'.", args.plugin))
    }
}
// ── register ──────────────────────────────────────────────────────────────────

pub fn register(reg: &mut orca_utils::tool::ToolRegistry) {
    reg.register::<ListPlugins>()
        .register::<AddPlugin>()
        .register::<RemovePlugin>()
        .register::<EnablePlugin>()
        .register::<DisablePlugin>()
        .register::<ListPluginCreds>()
        .register::<SetPluginCred>()
        .register::<RemovePluginCred>()
        .register::<SyncPluginCreds>();
}
