//! Plugins domain tools — registry CRUD + credential management.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use orca_tools_macro::orca_tool;
#[cfg(feature = "native")]
use std::sync::Arc;

// ── Typed entities ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginEntry {
    pub id: String,
    pub tier: String,
    pub mode: String,
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
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<ListPluginsOutput> {
    let plugins = ctx
        .service::<Arc<dyn PluginsService>>()?
        .list_plugins(args.workspace.as_deref())
        .await?
        .into_iter()
        .map(|p| PluginEntry {
            id: p.id,
            tier: p.tier,
            mode: p.mode,
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
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<AddPluginOutput> {
    let id = ctx
        .service::<Arc<dyn PluginsService>>()?
        .install_plugin(&args.manifest, args.instance_id.as_deref())
        .await?;
    Ok(AddPluginOutput { id })
}

/// [MUTATES STATE] Remove an installed orca plugin by ID.
#[orca_tool(domain = "system.plugin", verb = "delete")]
async fn remove_plugin(
    args: PluginIdArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<PluginMutationResult> {
    let changed = ctx
        .service::<Arc<dyn PluginsService>>()?
        .remove_plugin(&args.id)
        .await?;
    Ok(PluginMutationResult {
        id: args.id,
        changed,
    })
}

/// [MUTATES STATE] Enable or disable a registered orca plugin.
#[orca_tool(domain = "system.plugin", verb = "update")]
async fn update_plugin(
    args: UpdatePluginArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<PluginMutationResult> {
    let changed = ctx
        .service::<Arc<dyn PluginsService>>()?
        .set_plugin_enabled(&args.id, args.enabled)
        .await?;
    Ok(PluginMutationResult {
        id: args.id,
        changed,
    })
}

/// List all stored credentials for a plugin (keys only — values are never returned).
#[orca_tool(domain = "system.plugin.cred", verb = "list")]
async fn plugin_cred_list(
    args: ListPluginCredsArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<ListPluginCredsOutput> {
    let credentials = ctx
        .service::<Arc<dyn PluginsService>>()?
        .list_plugin_creds(&args.plugin)
        .await?
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
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<PluginCredMutationResult> {
    ctx.service::<Arc<dyn PluginsService>>()?
        .set_plugin_cred(&args.plugin, &args.key, &args.value)
        .await?;
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
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<PluginCredMutationResult> {
    let changed = ctx
        .service::<Arc<dyn PluginsService>>()?
        .remove_plugin_cred(&args.plugin, &args.key)
        .await?;
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
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<SyncPluginCredsOutput> {
    ctx.service::<Arc<dyn PluginsService>>()?
        .sync_plugin_creds(&args.plugin)
        .await?;
    Ok(SyncPluginCredsOutput {
        plugin: args.plugin,
    })
}

// ─── Service trait (impl in server crate) ────────────────────────────

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

/// Embedder hook — see `services::mod` doc.
pub trait ProvidePlugins {
    fn plugins(&self) -> std::sync::Arc<dyn PluginsService>;
}

/// Register a `PluginsService` into `ToolCtx`.
pub fn register_plugins(ctx: &mut orca_tool::ToolCtx, p: &impl ProvidePlugins) {
    ctx.register_service(p.plugins());
}
