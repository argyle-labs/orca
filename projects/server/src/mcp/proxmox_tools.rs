//! MCP data tools for Proxmox endpoints registered in orca.db.
//!
//! Mgmt CRUD lives in `mgmt_tools.rs`. This module exposes the actual
//! Proxmox VE operations: list nodes / VMs / containers, lifecycle actions.

use anyhow::{Context, Result};
use async_trait::async_trait;
use orca_proxmox::{Action, Client, Config};
use schemars::JsonSchema;
use serde::Deserialize;
use tool::{OrcaTool, ToolCtx};

fn make_client(name: &str) -> Result<Client> {
    let conn = db::open_default()?;
    let row = db::get_proxmox_endpoint(&conn, name)?
        .with_context(|| format!("proxmox endpoint '{name}' not registered (use add_proxmox_endpoint)"))?;
    if !row.enabled {
        anyhow::bail!("proxmox endpoint '{name}' is disabled");
    }
    let cfg = Config::new(row.base_url, row.token_id, row.token_secret).insecure(row.insecure);
    Ok(Client::new(cfg))
}

// ── list_nodes ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProxmoxListNodesArgs {
    /// Name of a Proxmox endpoint registered via add_proxmox_endpoint
    pub endpoint: String,
}
pub struct ProxmoxListNodes;
#[async_trait]
impl OrcaTool for ProxmoxListNodes {
    const NAME: &'static str = "proxmox_list_nodes";
    const DESCRIPTION: &'static str =
        "List Proxmox VE cluster nodes for a registered endpoint.";
    type Args = ProxmoxListNodesArgs;
    async fn run(args: ProxmoxListNodesArgs, _: &ToolCtx) -> Result<String> {
        let client = make_client(&args.endpoint)?;
        let v = client.nodes().await?;
        Ok(serde_json::to_string_pretty(&v)?)
    }
}

// ── list_vms ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProxmoxListVmsArgs {
    pub endpoint: String,
    /// Node name (e.g. "pve1")
    pub node: String,
}
pub struct ProxmoxListVms;
#[async_trait]
impl OrcaTool for ProxmoxListVms {
    const NAME: &'static str = "proxmox_list_vms";
    const DESCRIPTION: &'static str = "List QEMU VMs on a Proxmox node.";
    type Args = ProxmoxListVmsArgs;
    async fn run(args: ProxmoxListVmsArgs, _: &ToolCtx) -> Result<String> {
        let client = make_client(&args.endpoint)?;
        let v = client.vms(&args.node).await?;
        Ok(serde_json::to_string_pretty(&v)?)
    }
}

// ── list_containers ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProxmoxListContainersArgs {
    pub endpoint: String,
    /// Node name (e.g. "pve1")
    pub node: String,
}
pub struct ProxmoxListContainers;
#[async_trait]
impl OrcaTool for ProxmoxListContainers {
    const NAME: &'static str = "proxmox_list_containers";
    const DESCRIPTION: &'static str = "List LXC containers on a Proxmox node.";
    type Args = ProxmoxListContainersArgs;
    async fn run(args: ProxmoxListContainersArgs, _: &ToolCtx) -> Result<String> {
        let client = make_client(&args.endpoint)?;
        let v = client.containers(&args.node).await?;
        Ok(serde_json::to_string_pretty(&v)?)
    }
}

// ── vm_action ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProxmoxVmActionArgs {
    pub endpoint: String,
    pub node: String,
    /// VMID (numeric ID assigned by Proxmox)
    pub vmid: u64,
    /// One of: start | stop | shutdown | reboot
    pub action: String,
}
pub struct ProxmoxVmAction;
#[async_trait]
impl OrcaTool for ProxmoxVmAction {
    const NAME: &'static str = "proxmox_vm_action";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Run a lifecycle action on a Proxmox VM (start/stop/shutdown/reboot). \
         Returns the Proxmox UPID for tracking the async task.";
    type Args = ProxmoxVmActionArgs;
    async fn run(args: ProxmoxVmActionArgs, _: &ToolCtx) -> Result<String> {
        let client = make_client(&args.endpoint)?;
        let action: Action = args.action.parse()?;
        let result = client.vm_action(&args.node, args.vmid, action).await?;
        Ok(serde_json::to_string_pretty(&result)?)
    }
}

// ── container_action ───────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProxmoxContainerActionArgs {
    pub endpoint: String,
    pub node: String,
    pub vmid: u64,
    /// One of: start | stop | shutdown | reboot
    pub action: String,
}
pub struct ProxmoxContainerAction;
#[async_trait]
impl OrcaTool for ProxmoxContainerAction {
    const NAME: &'static str = "proxmox_container_action";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Run a lifecycle action on a Proxmox LXC container.";
    type Args = ProxmoxContainerActionArgs;
    async fn run(args: ProxmoxContainerActionArgs, _: &ToolCtx) -> Result<String> {
        let client = make_client(&args.endpoint)?;
        let action: Action = args.action.parse()?;
        let result = client
            .container_action(&args.node, args.vmid, action)
            .await?;
        Ok(serde_json::to_string_pretty(&result)?)
    }
}

// ── register ───────────────────────────────────────────────────────────────────

pub fn register(reg: &mut tool::ToolRegistry) {
    reg.register::<ProxmoxListNodes>()
        .register::<ProxmoxListVms>()
        .register::<ProxmoxListContainers>()
        .register::<ProxmoxVmAction>()
        .register::<ProxmoxContainerAction>();
}
