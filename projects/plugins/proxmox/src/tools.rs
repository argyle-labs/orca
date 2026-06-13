//! Proxmox tool surface — flat 4-tool surface (`proxmox.{list, detail, update,
//! delete}`). An endpoint is the primary resource; nodes/VMs/containers nest
//! into listings and detail. Lifecycle actions go through `update` —
//! `vmid` targets a QEMU VM, `ctid` targets an LXC container.
#![allow(clippy::disallowed_types)] // Proxmox upstream JSON is intentional JsonAny

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── Rows / results ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxmoxEndpointEntry {
    pub name: String,
    pub base_url: String,
    pub token_id: String,
    pub insecure: bool,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxActionResult {
    pub node: String,
    pub vmid: u64,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upid: Option<String>,
    pub status: u16,
}

mod native_support {
    use super::ProxmoxActionResult;
    use crate::{Client, Config, ProxmoxActionResult as IntResult};
    use anyhow::{Context, Result};

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
        let row = db::proxmox::get(&conn, name)?
            .with_context(|| format!("proxmox endpoint '{name}' not registered"))?;
        if !row.enabled {
            anyhow::bail!("proxmox endpoint '{name}' is disabled");
        }
        let cfg = Config::new(row.base_url, row.token_id, row.token_secret).insecure(row.insecure);
        Ok(Client::new(cfg))
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// proxmox.list — endpoints, optionally drilling into one
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct ProxmoxListArgs {
    /// Drill into one endpoint. Without `node`, returns its cluster nodes;
    /// with `node`, returns VMs + containers on that node.
    #[arg(long)]
    pub endpoint: Option<String>,
    #[arg(long)]
    pub node: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxmoxListOutput {
    /// Always populated unless `endpoint` is set.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<ProxmoxEndpointEntry>,
    /// Cluster nodes for `endpoint` (without `node`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nodes: Option<::contract::JsonAny>,
    /// VMs + containers on `node` (with `endpoint` + `node`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vms: Option<::contract::JsonAny>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub containers: Option<::contract::JsonAny>,
}

/// List Proxmox resources. No args → registered endpoints. `endpoint` →
/// nodes for that endpoint. `endpoint` + `node` → VMs + containers on that node.
#[orca_tool(domain = "proxmox", verb = "list")]
async fn proxmox_list(
    args: ProxmoxListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ProxmoxListOutput> {
    let mut out = ProxmoxListOutput::default();
    match (args.endpoint.as_deref(), args.node.as_deref()) {
        (None, None) => {
            let conn = db::open_default()?;
            out.endpoints = db::proxmox::list(&conn)?
                .into_iter()
                .map(|r| ProxmoxEndpointEntry {
                    name: r.name,
                    base_url: r.base_url,
                    token_id: r.token_id,
                    insecure: r.insecure,
                    enabled: r.enabled,
                })
                .collect();
        }
        (Some(ep), None) => {
            let client = native_support::make_client(ep)?;
            out.nodes = Some(client.nodes().await?.into());
        }
        (Some(ep), Some(node)) => {
            let client = native_support::make_client(ep)?;
            out.vms = Some(client.vms(node).await?.into());
            out.containers = Some(client.containers(node).await?.into());
        }
        (None, Some(_)) => anyhow::bail!("`node` requires `endpoint`"),
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// proxmox.detail — endpoint detail with nodes/VMs/containers nested
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxDetailArgs {
    /// Endpoint name.
    pub endpoint: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxmoxNodeDetail {
    pub node: String,
    pub vms: ::contract::JsonAny,
    pub containers: ::contract::JsonAny,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxmoxDetailOutput {
    pub endpoint: ProxmoxEndpointEntry,
    pub nodes: Vec<ProxmoxNodeDetail>,
}

#[orca_tool(domain = "proxmox", verb = "detail")]
async fn proxmox_detail(
    args: ProxmoxDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ProxmoxDetailOutput> {
    let conn = db::open_default()?;
    let row = db::proxmox::get(&conn, &args.endpoint)?
        .ok_or_else(|| anyhow::anyhow!("proxmox endpoint '{}' not registered", args.endpoint))?;
    drop(conn);
    let endpoint = ProxmoxEndpointEntry {
        name: row.name.clone(),
        base_url: row.base_url.clone(),
        token_id: row.token_id.clone(),
        insecure: row.insecure,
        enabled: row.enabled,
    };
    let client = native_support::make_client(&args.endpoint)?;
    let nodes_raw: contract::JsonAny = client.nodes().await?.into();
    let mut nodes = Vec::new();
    for n in nodes_raw.0.as_array().cloned().unwrap_or_default() {
        let Some(node_name) = n.get("node").and_then(|v| v.as_str()).map(str::to_string) else {
            continue;
        };
        let vms = client.vms(&node_name).await.unwrap_or_default();
        let containers = client.containers(&node_name).await.unwrap_or_default();
        nodes.push(ProxmoxNodeDetail {
            node: node_name,
            vms: vms.into(),
            containers: containers.into(),
        });
    }
    Ok(ProxmoxDetailOutput { endpoint, nodes })
}

// ═══════════════════════════════════════════════════════════════════════════
// proxmox.update — register/update endpoint, OR run VM/container action
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxmoxUpdateArgs {
    /// Endpoint register/update: `name` + `base_url` + `token_id` + `token_secret`.
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub base_url: Option<String>,
    #[arg(long)]
    pub token_id: Option<String>,
    #[arg(long)]
    pub token_secret: Option<String>,
    /// Allow self-signed TLS (endpoint register).
    #[arg(long)]
    pub insecure: Option<bool>,

    /// VM/container lifecycle action: provide `endpoint` + `node` + (`vmid` for
    /// a QEMU VM OR `ctid` for an LXC container) + `action`.
    #[arg(long)]
    pub endpoint: Option<String>,
    #[arg(long)]
    pub node: Option<String>,
    /// QEMU VM id.
    #[arg(long)]
    pub vmid: Option<u64>,
    /// LXC container id.
    #[arg(long)]
    pub ctid: Option<u64>,
    /// `start` | `stop` | `shutdown` | `reboot`.
    #[arg(long)]
    pub action: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxmoxUpdateOutput {
    pub applied: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_result: Option<ProxmoxActionResult>,
}

/// [MUTATES STATE] Register an endpoint OR run a VM/container lifecycle action.
#[orca_tool(domain = "proxmox", verb = "update")]
async fn proxmox_update(
    args: ProxmoxUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ProxmoxUpdateOutput> {
    let mut out = ProxmoxUpdateOutput::default();

    if let Some(name) = &args.name {
        let base_url = args
            .base_url
            .clone()
            .ok_or_else(|| anyhow::anyhow!("base_url required to register endpoint"))?;
        let token_id = args
            .token_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("token_id required to register endpoint"))?;
        let token_secret = args
            .token_secret
            .clone()
            .ok_or_else(|| anyhow::anyhow!("token_secret required to register endpoint"))?;
        let row = db::proxmox::EndpointRow {
            name: name.clone(),
            base_url,
            token_id,
            token_secret,
            insecure: args.insecure.unwrap_or(false),
            enabled: true,
        };
        let conn = db::open_default()?;
        db::proxmox::upsert(&conn, &row)?;
        out.applied.push(format!("endpoint-upserted:{name}"));
    }

    match (args.vmid, args.ctid) {
        (Some(_), Some(_)) => anyhow::bail!("set either `vmid` or `ctid`, not both"),
        (None, None) => {}
        (vmid_or_ctid, ctid_or_none) => {
            let endpoint = args
                .endpoint
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("endpoint required for lifecycle action"))?;
            let node = args
                .node
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("node required for lifecycle action"))?;
            let action_str = args
                .action
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("action required for lifecycle action"))?;
            let action: crate::ProxmoxAction = action_str.parse()?;
            let client = native_support::make_client(endpoint)?;
            let result: ProxmoxActionResult = if let Some(vmid) = vmid_or_ctid {
                out.applied
                    .push(format!("vm-action:{node}:{vmid}:{action_str}"));
                client.vm_action(node, vmid, action).await?.into()
            } else {
                let ctid = ctid_or_none.expect("checked above");
                out.applied
                    .push(format!("container-action:{node}:{ctid}:{action_str}"));
                client.container_action(node, ctid, action).await?.into()
            };
            out.action_result = Some(result);
        }
    }

    if out.applied.is_empty() {
        anyhow::bail!("no proxmox.update operation specified");
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// proxmox.delete — remove a registered endpoint
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxDeleteArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxDeleteOutput {
    pub name: String,
    pub changed: bool,
}

#[orca_tool(domain = "proxmox", verb = "delete")]
async fn proxmox_delete(
    args: ProxmoxDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ProxmoxDeleteOutput> {
    let conn = db::open_default()?;
    let changed = db::proxmox::remove(&conn, &args.name)?;
    Ok(ProxmoxDeleteOutput {
        name: args.name,
        changed,
    })
}
