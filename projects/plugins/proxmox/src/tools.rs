//! Proxmox tool surface.

//!
//! Endpoint registry: `proxmox.{list, detail, create, update, delete}` —
//! generated wholesale by `#[endpoint_resource]`. The macro emits the row
//! struct, db helpers (`endpoint_db::*`), schema fragment, args/output
//! types, and the five `#[orca_tool]`-annotated functions in one shot.
//!
//! Hand-written tools for cluster drill-in and lifecycle:
//!   - `proxmox.nodes`            list nodes for an endpoint
//!   - `proxmox.node_detail`      VMs + containers on one node
//!   - `proxmox.action`           VM/container start/stop/shutdown/reboot
//!
//! Imports flow through `plugin_toolkit::prelude::*` only.
#![allow(clippy::disallowed_types)] // Proxmox upstream JSON is intentional JsonAny

use plugin_toolkit::prelude::*;

use crate::{Client, Config};

// ═══════════════════════════════════════════════════════════════════════════
// proxmox.{list,detail,create,update,delete} — endpoint registry CRUD.
// ═══════════════════════════════════════════════════════════════════════════

#[endpoint_resource(plugin = "proxmox")]
pub struct ProxmoxEndpoint {
    pub name: String,
    pub base_url: String,
    pub token_id: String,
    #[secret]
    pub token_secret: String,
    pub insecure: bool,
    pub enabled: bool,
}

// ── Action result + From impl ──────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxActionResult {
    pub node: String,
    pub vmid: u64,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upid: Option<String>,
    pub status: u16,
}

impl From<crate::ProxmoxActionResult> for ProxmoxActionResult {
    fn from(r: crate::ProxmoxActionResult) -> Self {
        Self {
            node: r.node,
            vmid: r.vmid,
            action: r.action,
            upid: r.upid,
            status: r.status,
        }
    }
}

// ── HTTP client helper ─────────────────────────────────────────────────────

fn make_client(name: &str) -> Result<Client> {
    let conn = runtime::open_db()?;
    let row = endpoint_db::get(&conn, name)?
        .with_context(|| format!("proxmox endpoint '{name}' not registered"))?;
    if !row.enabled {
        bail!("proxmox endpoint '{name}' is disabled");
    }
    let cfg = Config::new(row.base_url, row.token_id, row.token_secret).insecure(row.insecure);
    Ok(Client::new(cfg))
}

// ═══════════════════════════════════════════════════════════════════════════
// proxmox.nodes — list cluster nodes for an endpoint
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxNodesArgs {
    #[arg(long)]
    pub endpoint: String,
}

/// List Proxmox cluster nodes for a registered endpoint.
#[orca_tool(domain = "proxmox", verb = "nodes")]
async fn proxmox_nodes(args: ProxmoxNodesArgs, _ctx: &ToolCtx) -> Result<JsonAny> {
    let client = make_client(&args.endpoint)?;
    Ok(client.nodes().await?.into())
}

// ═══════════════════════════════════════════════════════════════════════════
// proxmox.node_detail — VMs + containers on one node
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxNodeDetailArgs {
    #[arg(long)]
    pub endpoint: String,
    #[arg(long)]
    pub node: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxmoxNodeDetailOutput {
    pub node: String,
    pub vms: JsonAny,
    pub containers: JsonAny,
}

/// List VMs + containers on one node of a registered Proxmox endpoint.
#[orca_tool(domain = "proxmox", verb = "node_detail")]
async fn proxmox_node_detail(
    args: ProxmoxNodeDetailArgs,
    _ctx: &ToolCtx,
) -> Result<ProxmoxNodeDetailOutput> {
    let client = make_client(&args.endpoint)?;
    Ok(ProxmoxNodeDetailOutput {
        node: args.node.clone(),
        vms: client.vms(&args.node).await?.into(),
        containers: client.containers(&args.node).await?.into(),
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// proxmox.action — VM/container lifecycle action
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxActionArgs {
    #[arg(long)]
    pub endpoint: String,
    #[arg(long)]
    pub node: String,
    /// QEMU VM id.
    #[arg(long)]
    pub vmid: Option<u64>,
    /// LXC container id.
    #[arg(long)]
    pub ctid: Option<u64>,
    /// `start` | `stop` | `shutdown` | `reboot`.
    #[arg(long)]
    pub action: String,
}

/// [MUTATES STATE] Run a lifecycle action against one VM (`vmid`) or
/// container (`ctid`) on the named node.
#[orca_tool(domain = "proxmox", verb = "action", role = "admin")]
async fn proxmox_action(args: ProxmoxActionArgs, _ctx: &ToolCtx) -> Result<ProxmoxActionResult> {
    if args.vmid.is_some() && args.ctid.is_some() {
        bail!("set either `vmid` or `ctid`, not both");
    }
    let action: crate::ProxmoxAction = args.action.parse()?;
    let client = make_client(&args.endpoint)?;
    let result = if let Some(vmid) = args.vmid {
        client.vm_action(&args.node, vmid, action).await?
    } else if let Some(ctid) = args.ctid {
        client.container_action(&args.node, ctid, action).await?
    } else {
        bail!("`vmid` or `ctid` required");
    };
    Ok(result.into())
}
