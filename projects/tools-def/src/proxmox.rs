//! Proxmox tool defs + native impls.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxListNodesArgs {
    /// Name of a Proxmox endpoint registered via add_proxmox_endpoint
    pub endpoint: String,
}
pub struct ProxmoxListNodes;
impl OrcaToolDef for ProxmoxListNodes {
    const NAME: &'static str = "proxmox_list_nodes";
    const DESCRIPTION: &'static str = "List Proxmox VE cluster nodes for a registered endpoint.";
    type Args = ProxmoxListNodesArgs;
    type Output = crate::JsonAny;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
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
    type Output = crate::JsonAny;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
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
    type Output = crate::JsonAny;
}

/// Result of a Proxmox lifecycle action (start/stop/shutdown/reboot).
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxActionResult {
    pub node: String,
    pub vmid: u64,
    pub action: String,
    /// Proxmox returns a UPID (Unique Process ID) for async tasks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upid: Option<String>,
    pub status: u16,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
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
    type Output = ProxmoxActionResult;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
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
    type Output = ProxmoxActionResult;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use anyhow::{Context, Result};
    use async_trait::async_trait;
    use orca_db as db;
    use orca_integrations::proxmox::{Action, ActionResult, Client, Config};
    use orca_utils::tool::{OrcaTool, ToolCtx};

    impl From<ActionResult> for ProxmoxActionResult {
        fn from(r: ActionResult) -> Self {
            Self {
                node: r.node,
                vmid: r.vmid,
                action: r.action,
                upid: r.upid,
                status: r.status,
            }
        }
    }

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
        async fn run(args: ProxmoxListNodesArgs, _: &ToolCtx) -> Result<crate::JsonAny> {
            let client = make_client(&args.endpoint)?;
            Ok(client.nodes().await?.into())
        }
    }

    #[async_trait]
    impl OrcaTool for ProxmoxListVms {
        async fn run(args: ProxmoxListVmsArgs, _: &ToolCtx) -> Result<crate::JsonAny> {
            let client = make_client(&args.endpoint)?;
            Ok(client.vms(&args.node).await?.into())
        }
    }

    #[async_trait]
    impl OrcaTool for ProxmoxListContainers {
        async fn run(args: ProxmoxListContainersArgs, _: &ToolCtx) -> Result<crate::JsonAny> {
            let client = make_client(&args.endpoint)?;
            Ok(client.containers(&args.node).await?.into())
        }
    }

    #[async_trait]
    impl OrcaTool for ProxmoxVmAction {
        async fn run(args: ProxmoxVmActionArgs, _: &ToolCtx) -> Result<ProxmoxActionResult> {
            let client = make_client(&args.endpoint)?;
            let action: Action = args.action.parse()?;
            Ok(client
                .vm_action(&args.node, args.vmid, action)
                .await?
                .into())
        }
    }

    #[async_trait]
    impl OrcaTool for ProxmoxContainerAction {
        async fn run(args: ProxmoxContainerActionArgs, _: &ToolCtx) -> Result<ProxmoxActionResult> {
            let client = make_client(&args.endpoint)?;
            let action: Action = args.action.parse()?;
            Ok(client
                .container_action(&args.node, args.vmid, action)
                .await?
                .into())
        }
    }
}
