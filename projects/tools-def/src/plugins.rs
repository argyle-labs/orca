//! Plugins domain tools — registry CRUD + credential management.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

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

// ── list_plugins ────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

pub struct ListPlugins;
impl OrcaToolDef for ListPlugins {
    const NAME: &'static str = "list_plugins";
    const DESCRIPTION: &'static str = "List all orca plugins registered in orca.db.";
    type Args = ListPluginsArgs;
    type Output = ListPluginsOutput;
}

// ── add_plugin ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

pub struct AddPlugin;
impl OrcaToolDef for AddPlugin {
    const NAME: &'static str = "add_plugin";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Install an orca plugin from a manifest path or URL.";
    type Args = AddPluginArgs;
    type Output = AddPluginOutput;
}

// ── remove_plugin / enable / disable ────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

pub struct RemovePlugin;
impl OrcaToolDef for RemovePlugin {
    const NAME: &'static str = "remove_plugin";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Remove an installed orca plugin by ID.";
    type Args = PluginIdArgs;
    type Output = PluginMutationResult;
}

pub struct EnablePlugin;
impl OrcaToolDef for EnablePlugin {
    const NAME: &'static str = "enable_plugin";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Enable a registered orca plugin.";
    type Args = PluginIdArgs;
    type Output = PluginMutationResult;
}

pub struct DisablePlugin;
impl OrcaToolDef for DisablePlugin {
    const NAME: &'static str = "disable_plugin";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Disable a registered orca plugin.";
    type Args = PluginIdArgs;
    type Output = PluginMutationResult;
}

// ── plugin creds ────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

pub struct ListPluginCreds;
impl OrcaToolDef for ListPluginCreds {
    const NAME: &'static str = "list_plugin_creds";
    const DESCRIPTION: &'static str =
        "List all stored credentials for a plugin (keys only — values are never returned).";
    type Args = ListPluginCredsArgs;
    type Output = ListPluginCredsOutput;
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

pub struct SetPluginCred;
impl OrcaToolDef for SetPluginCred {
    const NAME: &'static str = "set_plugin_cred";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Store a credential value for a plugin in orca.db.";
    type Args = SetPluginCredArgs;
    type Output = PluginCredMutationResult;
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
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
    type Output = PluginCredMutationResult;
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

pub struct SyncPluginCreds;
impl OrcaToolDef for SyncPluginCreds {
    const NAME: &'static str = "sync_plugin_creds";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Sync stored credentials for a plugin to its runtime environment.";
    type Args = SyncPluginCredsArgs;
    type Output = SyncPluginCredsOutput;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::plugins as svc_plug;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn svc_plug::PluginsService>> {
        ctx.service::<Arc<dyn svc_plug::PluginsService>>()
    }

    #[async_trait]
    impl OrcaTool for ListPlugins {
        async fn run(args: ListPluginsArgs, ctx: &ToolCtx) -> Result<ListPluginsOutput> {
            let plugins = svc(ctx)?
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
    }

    #[async_trait]
    impl OrcaTool for AddPlugin {
        async fn run(args: AddPluginArgs, ctx: &ToolCtx) -> Result<AddPluginOutput> {
            let id = svc(ctx)?
                .install_plugin(&args.manifest, args.instance_id.as_deref())
                .await?;
            Ok(AddPluginOutput { id })
        }
    }

    #[async_trait]
    impl OrcaTool for RemovePlugin {
        async fn run(args: PluginIdArgs, ctx: &ToolCtx) -> Result<PluginMutationResult> {
            let changed = svc(ctx)?.remove_plugin(&args.id).await?;
            Ok(PluginMutationResult {
                id: args.id,
                changed,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for EnablePlugin {
        async fn run(args: PluginIdArgs, ctx: &ToolCtx) -> Result<PluginMutationResult> {
            let changed = svc(ctx)?.set_plugin_enabled(&args.id, true).await?;
            Ok(PluginMutationResult {
                id: args.id,
                changed,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for DisablePlugin {
        async fn run(args: PluginIdArgs, ctx: &ToolCtx) -> Result<PluginMutationResult> {
            let changed = svc(ctx)?.set_plugin_enabled(&args.id, false).await?;
            Ok(PluginMutationResult {
                id: args.id,
                changed,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for ListPluginCreds {
        async fn run(args: ListPluginCredsArgs, ctx: &ToolCtx) -> Result<ListPluginCredsOutput> {
            let credentials = svc(ctx)?
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
    }

    #[async_trait]
    impl OrcaTool for SetPluginCred {
        async fn run(args: SetPluginCredArgs, ctx: &ToolCtx) -> Result<PluginCredMutationResult> {
            svc(ctx)?
                .set_plugin_cred(&args.plugin, &args.key, &args.value)
                .await?;
            Ok(PluginCredMutationResult {
                plugin: args.plugin,
                key: args.key,
                changed: true,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for RemovePluginCred {
        async fn run(
            args: RemovePluginCredArgs,
            ctx: &ToolCtx,
        ) -> Result<PluginCredMutationResult> {
            let changed = svc(ctx)?
                .remove_plugin_cred(&args.plugin, &args.key)
                .await?;
            Ok(PluginCredMutationResult {
                plugin: args.plugin,
                key: args.key,
                changed,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for SyncPluginCreds {
        async fn run(args: SyncPluginCredsArgs, ctx: &ToolCtx) -> Result<SyncPluginCredsOutput> {
            svc(ctx)?.sync_plugin_creds(&args.plugin).await?;
            Ok(SyncPluginCredsOutput {
                plugin: args.plugin,
            })
        }
    }
}
