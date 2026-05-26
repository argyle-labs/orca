//! Proxmox tool defs + native impls.
#![allow(clippy::disallowed_types)] // Proxmox API shapes are upstream-defined; JsonAny outputs are intentional

use orca_macro::orca_tool;
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

#[cfg(feature = "native")]
mod native_support {
    use super::*;
    use crate::{Client, Config, ProxmoxActionResult as IntResult};
    use anyhow::{Context, Result};
    use orca_db as db;

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
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<::orca_contract::JsonAny> {
    let client = native_support::make_client(&args.endpoint)?;
    Ok(client.nodes().await?.into())
}

/// List QEMU VMs on a Proxmox node.
#[orca_tool(domain = "proxmox.vm", verb = "list")]
async fn proxmox_vm_list(
    args: ProxmoxListVmsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<::orca_contract::JsonAny> {
    let client = native_support::make_client(&args.endpoint)?;
    Ok(client.vms(&args.node).await?.into())
}

/// List LXC containers on a Proxmox node.
#[orca_tool(domain = "proxmox.container", verb = "list")]
async fn proxmox_container_list(
    args: ProxmoxListContainersArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<::orca_contract::JsonAny> {
    let client = native_support::make_client(&args.endpoint)?;
    Ok(client.containers(&args.node).await?.into())
}

/// [MUTATES STATE] Run a lifecycle action on a Proxmox VM (start/stop/shutdown/reboot).
#[orca_tool(domain = "proxmox.vm", verb = "update")]
async fn proxmox_vm_update(
    args: ProxmoxVmActionArgs,
    _ctx: &orca_contract::ToolCtx,
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
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ProxmoxActionResult> {
    let client = native_support::make_client(&args.endpoint)?;
    let action: crate::ProxmoxAction = args.action.parse()?;
    Ok(client
        .container_action(&args.node, args.vmid, action)
        .await?
        .into())
}
