//! Plugins domain tools — registry CRUD + credential management.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

// ── Typed entities ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginCredEntry {
    pub key: String,
    /// `true` once the credential has been synced to the plugin runtime.
    pub synced: bool,
    pub updated_at: String,
}

// ── Args / Outputs ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListPluginsArgs {
    /// Filter by workspace tier (omit for all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListPluginsOutput {
    pub plugins: Vec<PluginEntry>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddPluginArgs {
    /// Path or URL to plugin manifest.
    pub manifest: String,
    /// Optional instance ID override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddPluginOutput {
    pub id: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PluginIdArgs {
    pub id: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PluginMutationResult {
    pub id: String,
    /// `true` when the plugin existed and the operation took effect.
    pub changed: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListPluginCredsArgs {
    pub plugin: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListPluginCredsOutput {
    pub plugin: String,
    pub credentials: Vec<PluginCredEntry>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetPluginCredArgs {
    pub plugin: String,
    pub key: String,
    pub value: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PluginCredMutationResult {
    pub plugin: String,
    pub key: String,
    pub changed: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemovePluginCredArgs {
    pub plugin: String,
    pub key: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncPluginCredsArgs {
    pub plugin: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncPluginCredsOutput {
    pub plugin: String,
}

// ── Native dispatch ─────────────────────────────────────────────────────────

#[cfg(feature = "native")]
fn plugins_svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::plugins::PluginsService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::plugins::PluginsService>>()
}

/// List all orca plugins registered in orca.db.
#[orca_tool(domain = "plugin", verb = "list")]
async fn list_plugins(
    args: ListPluginsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListPluginsOutput> {
    let plugins = plugins_svc(ctx)?
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
#[orca_tool(domain = "plugin", verb = "add")]
async fn add_plugin(
    args: AddPluginArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<AddPluginOutput> {
    let id = plugins_svc(ctx)?
        .install_plugin(&args.manifest, args.instance_id.as_deref())
        .await?;
    Ok(AddPluginOutput { id })
}

/// [MUTATES STATE] Remove an installed orca plugin by ID.
#[orca_tool(domain = "plugin", verb = "remove")]
async fn remove_plugin(
    args: PluginIdArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PluginMutationResult> {
    let changed = plugins_svc(ctx)?.remove_plugin(&args.id).await?;
    Ok(PluginMutationResult {
        id: args.id,
        changed,
    })
}

/// [MUTATES STATE] Enable a registered orca plugin.
#[orca_tool(domain = "plugin", verb = "enable")]
async fn enable_plugin(
    args: PluginIdArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PluginMutationResult> {
    let changed = plugins_svc(ctx)?.set_plugin_enabled(&args.id, true).await?;
    Ok(PluginMutationResult {
        id: args.id,
        changed,
    })
}

/// [MUTATES STATE] Disable a registered orca plugin.
#[orca_tool(domain = "plugin", verb = "disable")]
async fn disable_plugin(
    args: PluginIdArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PluginMutationResult> {
    let changed = plugins_svc(ctx)?
        .set_plugin_enabled(&args.id, false)
        .await?;
    Ok(PluginMutationResult {
        id: args.id,
        changed,
    })
}

/// List all stored credentials for a plugin (keys only — values are never returned).
#[orca_tool(domain = "plugin", verb = "list-creds")]
async fn list_plugin_creds(
    args: ListPluginCredsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListPluginCredsOutput> {
    let credentials = plugins_svc(ctx)?
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
#[orca_tool(domain = "plugin", verb = "set-cred")]
async fn set_plugin_cred(
    args: SetPluginCredArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PluginCredMutationResult> {
    plugins_svc(ctx)?
        .set_plugin_cred(&args.plugin, &args.key, &args.value)
        .await?;
    Ok(PluginCredMutationResult {
        plugin: args.plugin,
        key: args.key,
        changed: true,
    })
}

/// [MUTATES STATE] Remove a stored credential for a plugin from orca.db.
#[orca_tool(domain = "plugin", verb = "remove-cred")]
async fn remove_plugin_cred(
    args: RemovePluginCredArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PluginCredMutationResult> {
    let changed = plugins_svc(ctx)?
        .remove_plugin_cred(&args.plugin, &args.key)
        .await?;
    Ok(PluginCredMutationResult {
        plugin: args.plugin,
        key: args.key,
        changed,
    })
}

/// [MUTATES STATE] Sync stored credentials for a plugin to its runtime environment.
#[orca_tool(domain = "plugin", verb = "sync-creds")]
async fn sync_plugin_creds(
    args: SyncPluginCredsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SyncPluginCredsOutput> {
    plugins_svc(ctx)?.sync_plugin_creds(&args.plugin).await?;
    Ok(SyncPluginCredsOutput {
        plugin: args.plugin,
    })
}
