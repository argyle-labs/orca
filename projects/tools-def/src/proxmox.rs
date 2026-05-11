//! Proxmox tool defs + native impls.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::OrcaToolDef;

#[derive(Deserialize, JsonSchema)]
pub struct ProxmoxListNodesArgs {
    /// Name of a Proxmox endpoint registered via add_proxmox_endpoint
    pub endpoint: String,
}
pub struct ProxmoxListNodes;
impl OrcaToolDef for ProxmoxListNodes {
    const NAME: &'static str = "proxmox_list_nodes";
    const DESCRIPTION: &'static str = "List Proxmox VE cluster nodes for a registered endpoint.";
    type Args = ProxmoxListNodesArgs;
    type Output = String;
}

#[derive(Deserialize, JsonSchema)]
pub struct ProxmoxListVmsArgs {
    pub endpoint: String,
    /// Node name (e.g. "pve1")
    pub node: String,
}
pub struct ProxmoxListVms;
impl OrcaToolDef for ProxmoxListVms {
    const NAME: &'static str = "proxmox_list_vms";
    const DESCRIPTION: &'static str = "List QEMU VMs on a Proxmox node.";
    type Args = ProxmoxListVmsArgs;
    type Output = String;
}

#[derive(Deserialize, JsonSchema)]
pub struct ProxmoxListContainersArgs {
    pub endpoint: String,
    /// Node name (e.g. "pve1")
    pub node: String,
}
pub struct ProxmoxListContainers;
impl OrcaToolDef for ProxmoxListContainers {
    const NAME: &'static str = "proxmox_list_containers";
    const DESCRIPTION: &'static str = "List LXC containers on a Proxmox node.";
    type Args = ProxmoxListContainersArgs;
    type Output = String;
}

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
impl OrcaToolDef for ProxmoxVmAction {
    const NAME: &'static str = "proxmox_vm_action";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Run a lifecycle action on a Proxmox VM (start/stop/shutdown/reboot). \
         Returns the Proxmox UPID for tracking the async task.";
    type Args = ProxmoxVmActionArgs;
    type Output = String;
}

#[derive(Deserialize, JsonSchema)]
pub struct ProxmoxContainerActionArgs {
    pub endpoint: String,
    pub node: String,
    pub vmid: u64,
    /// One of: start | stop | shutdown | reboot
    pub action: String,
}
pub struct ProxmoxContainerAction;
impl OrcaToolDef for ProxmoxContainerAction {
    const NAME: &'static str = "proxmox_container_action";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Run a lifecycle action on a Proxmox LXC container.";
    type Args = ProxmoxContainerActionArgs;
    type Output = String;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use anyhow::{Context, Result};
    use async_trait::async_trait;
    use orca_db as db;
    use orca_integrations::proxmox::{Action, Client, Config};
    use orca_utils::tool::{OrcaTool, ToolCtx};

    fn make_client(name: &str) -> Result<Client> {
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

    #[async_trait]
    impl OrcaTool for ProxmoxListNodes {
        const NAME: &'static str = <Self as OrcaToolDef>::NAME;
        const DESCRIPTION: &'static str = <Self as OrcaToolDef>::DESCRIPTION;
        type Args = <Self as OrcaToolDef>::Args;
        type Output = <Self as OrcaToolDef>::Output;
        async fn run(args: ProxmoxListNodesArgs, _: &ToolCtx) -> Result<String> {
            let client = make_client(&args.endpoint)?;
            let v = client.nodes().await?;
            Ok(serde_json::to_string_pretty(&v)?)
        }
    }

    #[async_trait]
    impl OrcaTool for ProxmoxListVms {
        const NAME: &'static str = <Self as OrcaToolDef>::NAME;
        const DESCRIPTION: &'static str = <Self as OrcaToolDef>::DESCRIPTION;
        type Args = <Self as OrcaToolDef>::Args;
        type Output = <Self as OrcaToolDef>::Output;
        async fn run(args: ProxmoxListVmsArgs, _: &ToolCtx) -> Result<String> {
            let client = make_client(&args.endpoint)?;
            let v = client.vms(&args.node).await?;
            Ok(serde_json::to_string_pretty(&v)?)
        }
    }

    #[async_trait]
    impl OrcaTool for ProxmoxListContainers {
        const NAME: &'static str = <Self as OrcaToolDef>::NAME;
        const DESCRIPTION: &'static str = <Self as OrcaToolDef>::DESCRIPTION;
        type Args = <Self as OrcaToolDef>::Args;
        type Output = <Self as OrcaToolDef>::Output;
        async fn run(args: ProxmoxListContainersArgs, _: &ToolCtx) -> Result<String> {
            let client = make_client(&args.endpoint)?;
            let v = client.containers(&args.node).await?;
            Ok(serde_json::to_string_pretty(&v)?)
        }
    }

    #[async_trait]
    impl OrcaTool for ProxmoxVmAction {
        const NAME: &'static str = <Self as OrcaToolDef>::NAME;
        const DESCRIPTION: &'static str = <Self as OrcaToolDef>::DESCRIPTION;
        type Args = <Self as OrcaToolDef>::Args;
        type Output = <Self as OrcaToolDef>::Output;
        async fn run(args: ProxmoxVmActionArgs, _: &ToolCtx) -> Result<String> {
            let client = make_client(&args.endpoint)?;
            let action: Action = args.action.parse()?;
            let result = client.vm_action(&args.node, args.vmid, action).await?;
            Ok(serde_json::to_string_pretty(&result)?)
        }
    }

    #[async_trait]
    impl OrcaTool for ProxmoxContainerAction {
        const NAME: &'static str = <Self as OrcaToolDef>::NAME;
        const DESCRIPTION: &'static str = <Self as OrcaToolDef>::DESCRIPTION;
        type Args = <Self as OrcaToolDef>::Args;
        type Output = <Self as OrcaToolDef>::Output;
        async fn run(args: ProxmoxContainerActionArgs, _: &ToolCtx) -> Result<String> {
            let client = make_client(&args.endpoint)?;
            let action: Action = args.action.parse()?;
            let result = client
                .container_action(&args.node, args.vmid, action)
                .await?;
            Ok(serde_json::to_string_pretty(&result)?)
        }
    }
}
