//! Plugins domain tools — registry CRUD + credential management.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

// ── Typed entities ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginEntry {
    pub id: String,
    pub tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_command: Option<String>,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginCredEntry {
    pub key: String,
    /// `true` once the credential has been synced to the plugin runtime.
    pub synced: bool,
    pub updated_at: String,
}

// ── Args / Outputs ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListPluginsArgs {
    /// Filter by workspace tier (omit for all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListPluginsOutput {
    pub plugins: Vec<PluginEntry>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddPluginArgs {
    /// Path or URL to plugin manifest.
    pub manifest: String,
    /// Optional instance ID override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddPluginOutput {
    pub id: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PluginIdArgs {
    pub id: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdatePluginArgs {
    pub id: String,
    /// true = enable the plugin, false = disable without removing.
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PluginMutationResult {
    pub id: String,
    /// `true` when the plugin existed and the operation took effect.
    pub changed: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListPluginCredsArgs {
    pub plugin: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListPluginCredsOutput {
    pub plugin: String,
    pub credentials: Vec<PluginCredEntry>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetPluginCredArgs {
    pub plugin: String,
    pub key: String,
    pub value: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PluginCredMutationResult {
    pub plugin: String,
    pub key: String,
    pub changed: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemovePluginCredArgs {
    pub plugin: String,
    pub key: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncPluginCredsArgs {
    pub plugin: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncPluginCredsOutput {
    pub plugin: String,
}

// ── Native dispatch ─────────────────────────────────────────────────────────

/// List all orca plugins registered in orca.db.
#[orca_tool(domain = "system.plugin", verb = "list")]
async fn list_plugins(
    args: ListPluginsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListPluginsOutput> {
    let conn = db::open_default()?;
    let rows = db::plugins::list(&conn)?;
    let plugins = rows
        .into_iter()
        .filter(|p| args.workspace.as_deref().is_none_or(|w| p.tier == w))
        .map(|p| PluginEntry {
            id: p.id,
            tier: p.tier,
            mcp_command: p.mcp_command,
            enabled: p.enabled,
        })
        .collect();
    Ok(ListPluginsOutput { plugins })
}

/// [MUTATES STATE] Install an orca plugin from a manifest path or URL.
#[orca_tool(domain = "system.plugin", verb = "create")]
async fn add_plugin(
    args: AddPluginArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<AddPluginOutput> {
    let id = crate::install::install_plugin(&args.manifest, args.instance_id.as_deref())?;
    Ok(AddPluginOutput { id })
}

/// [MUTATES STATE] Remove an installed orca plugin by ID.
#[orca_tool(domain = "system.plugin", verb = "delete")]
async fn remove_plugin(
    args: PluginIdArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PluginMutationResult> {
    let changed = crate::install::remove_plugin(&args.id)?;
    Ok(PluginMutationResult {
        id: args.id,
        changed,
    })
}

/// [MUTATES STATE] Enable or disable a registered orca plugin.
#[orca_tool(domain = "system.plugin", verb = "update")]
async fn update_plugin(
    args: UpdatePluginArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PluginMutationResult> {
    let conn = db::open_default()?;
    let changed = db::plugins::set_enabled(&conn, &args.id, args.enabled)?;
    Ok(PluginMutationResult {
        id: args.id,
        changed,
    })
}

/// List all stored credentials for a plugin (keys only — values are never returned).
#[orca_tool(domain = "system.plugin.cred", verb = "list")]
async fn plugin_cred_list(
    args: ListPluginCredsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListPluginCredsOutput> {
    let conn = db::open_default()?;
    let creds = db::plugin_creds::list(&conn, &args.plugin)?;
    let credentials = creds
        .into_iter()
        .map(|c| PluginCredEntry {
            key: c.key,
            synced: c.synced_at.is_some(),
            updated_at: c.updated_at,
        })
        .collect();
    Ok(ListPluginCredsOutput {
        plugin: args.plugin,
        credentials,
    })
}

/// [MUTATES STATE] Store a credential value for a plugin in orca.db.
#[orca_tool(domain = "system.plugin.cred", verb = "create")]
async fn plugin_cred_create(
    args: SetPluginCredArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PluginCredMutationResult> {
    let conn = db::open_default()?;
    db::plugin_creds::set(&conn, &args.plugin, &args.key, &args.value)?;
    Ok(PluginCredMutationResult {
        plugin: args.plugin,
        key: args.key,
        changed: true,
    })
}

/// [MUTATES STATE] Remove a stored credential for a plugin from orca.db.
#[orca_tool(domain = "system.plugin.cred", verb = "delete")]
async fn plugin_cred_delete(
    args: RemovePluginCredArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PluginCredMutationResult> {
    let conn = db::open_default()?;
    let changed = db::plugin_creds::delete(&conn, &args.plugin, &args.key)?;
    Ok(PluginCredMutationResult {
        plugin: args.plugin,
        key: args.key,
        changed,
    })
}

/// [MUTATES STATE] Sync stored credentials for a plugin to its runtime environment.
#[orca_tool(domain = "system.plugin.cred", verb = "sync")]
async fn plugin_cred_sync(
    args: SyncPluginCredsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SyncPluginCredsOutput> {
    crate::creds::sync_plugin_creds(&args.plugin)?;
    Ok(SyncPluginCredsOutput {
        plugin: args.plugin,
    })
}
