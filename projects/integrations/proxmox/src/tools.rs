//! Proxmox tool defs + native impls.
#![allow(clippy::disallowed_types)] // Proxmox API shapes are upstream-defined; JsonAny outputs are intentional

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxListNodesArgs {
    /// Name of a Proxmox endpoint registered via add_proxmox_endpoint
    pub endpoint: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxListVmsArgs {
    pub endpoint: String,
    /// Node name (e.g. "pve1")
    pub node: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxListContainersArgs {
    pub endpoint: String,
    /// Node name (e.g. "pve1")
    pub node: String,
}

/// Result of a Proxmox lifecycle action.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxActionResult {
    pub node: String,
    pub vmid: u64,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upid: Option<String>,
    pub status: u16,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxVmActionArgs {
    pub endpoint: String,
    pub node: String,
    pub vmid: u64,
    /// One of: start | stop | shutdown | reboot
    pub action: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxContainerActionArgs {
    pub endpoint: String,
    pub node: String,
    pub vmid: u64,
    /// One of: start | stop | shutdown | reboot
    pub action: String,
}

mod native_support {
    use super::*;
    use crate::{Client, Config, ProxmoxActionResult as IntResult};
    use anyhow::{Context, Result};
    use db;

    impl From<IntResult> for ProxmoxActionResult {
        fn from(r: IntResult) -> Self {
            Self {
                node: r.node,
                vmid: r.vmid,
                action: r.action,
                upid: r.upid,
                status: r.status,
            }
        }
    }

    pub(super) fn make_client(name: &str) -> Result<Client> {
        let conn = db::open_default()?;
        let row = db::proxmox::get(&conn, name)?.with_context(|| {
            format!("proxmox endpoint '{name}' not registered (use add_proxmox_endpoint)")
        })?;
        if !row.enabled {
            anyhow::bail!("proxmox endpoint '{name}' is disabled");
        }
        let cfg = Config::new(row.base_url, row.token_id, row.token_secret).insecure(row.insecure);
        Ok(Client::new(cfg))
    }
}

/// List Proxmox VE cluster nodes for a registered endpoint.
#[orca_tool(domain = "proxmox.node", verb = "list")]
async fn proxmox_node_list(
    args: ProxmoxListNodesArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<::contract::JsonAny> {
    let client = native_support::make_client(&args.endpoint)?;
    Ok(client.nodes().await?.into())
}

/// List QEMU VMs on a Proxmox node.
#[orca_tool(domain = "proxmox.vm", verb = "list")]
async fn proxmox_vm_list(
    args: ProxmoxListVmsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<::contract::JsonAny> {
    let client = native_support::make_client(&args.endpoint)?;
    Ok(client.vms(&args.node).await?.into())
}

/// List LXC containers on a Proxmox node.
#[orca_tool(domain = "proxmox.container", verb = "list")]
async fn proxmox_container_list(
    args: ProxmoxListContainersArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<::contract::JsonAny> {
    let client = native_support::make_client(&args.endpoint)?;
    Ok(client.containers(&args.node).await?.into())
}

/// [MUTATES STATE] Run a lifecycle action on a Proxmox VM (start/stop/shutdown/reboot).
#[orca_tool(domain = "proxmox.vm", verb = "update")]
async fn proxmox_vm_update(
    args: ProxmoxVmActionArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ProxmoxActionResult> {
    let client = native_support::make_client(&args.endpoint)?;
    let action: crate::ProxmoxAction = args.action.parse()?;
    Ok(client
        .vm_action(&args.node, args.vmid, action)
        .await?
        .into())
}

/// [MUTATES STATE] Run a lifecycle action on a Proxmox LXC container.
#[orca_tool(domain = "proxmox.container", verb = "update")]
async fn proxmox_container_update(
    args: ProxmoxContainerActionArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ProxmoxActionResult> {
    let client = native_support::make_client(&args.endpoint)?;
    let action: crate::ProxmoxAction = args.action.parse()?;
    Ok(client
        .container_action(&args.node, args.vmid, action)
        .await?
        .into())
}

// ── Proxmox endpoints (registered in orca.db) ───────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxmoxEndpointEntry {
    pub name: String,
    pub base_url: String,
    pub token_id: String,
    pub insecure: bool,
    pub enabled: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListProxmoxEndpointsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListProxmoxEndpointsOutput {
    pub endpoints: Vec<ProxmoxEndpointEntry>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddProxmoxEndpointArgs {
    pub name: String,
    pub base_url: String,
    pub token_id: String,
    pub token_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insecure: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxMutationResult {
    pub name: String,
    pub changed: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveProxmoxEndpointArgs {
    pub name: String,
}

/// List all Proxmox VE endpoints registered in orca.db (token secrets are redacted).
#[orca_tool(domain = "proxmox.endpoint", verb = "list")]
async fn proxmox_endpoint_list(
    _args: ListProxmoxEndpointsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListProxmoxEndpointsOutput> {
    let conn = db::open_default()?;
    let endpoints = db::proxmox::list(&conn)?
        .into_iter()
        .map(|r| ProxmoxEndpointEntry {
            name: r.name,
            base_url: r.base_url,
            token_id: r.token_id,
            insecure: r.insecure,
            enabled: r.enabled,
        })
        .collect();
    Ok(ListProxmoxEndpointsOutput { endpoints })
}

/// [MUTATES STATE] Register or update a Proxmox VE endpoint in orca.db. Auth uses an API token (PVEAPIToken header).
#[orca_tool(domain = "proxmox.endpoint", verb = "create")]
async fn proxmox_endpoint_create(
    args: AddProxmoxEndpointArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ProxmoxMutationResult> {
    let row = db::proxmox::EndpointRow {
        name: args.name.clone(),
        base_url: args.base_url,
        token_id: args.token_id,
        token_secret: args.token_secret,
        insecure: args.insecure.unwrap_or(false),
        enabled: true,
    };
    let conn = db::open_default()?;
    db::proxmox::upsert(&conn, &row)?;
    Ok(ProxmoxMutationResult {
        name: args.name,
        changed: true,
    })
}

/// [MUTATES STATE] Remove a Proxmox VE endpoint from orca.db by name.
#[orca_tool(domain = "proxmox.endpoint", verb = "delete")]
async fn proxmox_endpoint_delete(
    args: RemoveProxmoxEndpointArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ProxmoxMutationResult> {
    let conn = db::open_default()?;
    let changed = db::proxmox::remove(&conn, &args.name)?;
    Ok(ProxmoxMutationResult {
        name: args.name,
        changed,
    })
}
