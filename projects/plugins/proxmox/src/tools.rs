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
    #[arg(long)]
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
// proxmox.create — register a new endpoint
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxCreateArgs {
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub base_url: String,
    #[arg(long)]
    pub token_id: String,
    #[arg(long)]
    pub token_secret: String,
    /// Allow self-signed TLS.
    #[arg(long)]
    pub insecure: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxCreateOutput {
    pub endpoint: ProxmoxEndpointEntry,
}

/// [MUTATES STATE] Register a new Proxmox endpoint. Errors if `name` is
/// already taken — use proxmox.update to modify an existing endpoint.
#[orca_tool(domain = "proxmox", verb = "create")]
async fn proxmox_create(
    args: ProxmoxCreateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ProxmoxCreateOutput> {
    let row = db::proxmox::EndpointRow {
        name: args.name.clone(),
        base_url: args.base_url.clone(),
        token_id: args.token_id.clone(),
        token_secret: args.token_secret.clone(),
        insecure: args.insecure.unwrap_or(false),
        enabled: true,
    };
    let conn = db::open_default()?;
    db::proxmox::insert(&conn, &row).map_err(|e| {
        if e.to_string().contains("UNIQUE") {
            anyhow::anyhow!(
                "proxmox endpoint '{}' already exists — use proxmox.update to modify it",
                row.name
            )
        } else {
            e
        }
    })?;
    Ok(ProxmoxCreateOutput {
        endpoint: ProxmoxEndpointEntry {
            name: row.name,
            base_url: row.base_url,
            token_id: row.token_id,
            insecure: row.insecure,
            enabled: row.enabled,
        },
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// proxmox.update — PATCH endpoint fields OR run VM/container action
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxmoxUpdateArgs {
    /// Endpoint to patch or target for a lifecycle action.
    #[arg(long)]
    pub name: String,

    // PATCH fields — all optional
    #[arg(long)]
    pub base_url: Option<String>,
    #[arg(long)]
    pub token_id: Option<String>,
    #[arg(long)]
    pub token_secret: Option<String>,
    #[arg(long)]
    pub insecure: Option<bool>,
    #[arg(long)]
    pub enabled: Option<bool>,

    // Lifecycle action fields
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

/// [MUTATES STATE] PATCH an existing Proxmox endpoint's fields, or run a
/// VM/container lifecycle action. Endpoint must already exist.
#[orca_tool(domain = "proxmox", verb = "update")]
async fn proxmox_update(
    args: ProxmoxUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ProxmoxUpdateOutput> {
    let conn = db::open_default()?;
    let mut row = db::proxmox::get(&conn, &args.name)?
        .ok_or_else(|| anyhow::anyhow!("proxmox endpoint '{}' not registered", args.name))?;
    let mut out = ProxmoxUpdateOutput::default();

    // PATCH fields
    let mut changed_fields: Vec<String> = Vec::new();
    if let Some(v) = args.base_url {
        row.base_url = v;
        changed_fields.push("base_url".into());
    }
    if let Some(v) = args.token_id {
        row.token_id = v;
        changed_fields.push("token_id".into());
    }
    if let Some(v) = args.token_secret {
        row.token_secret = v;
        changed_fields.push("token_secret".into());
    }
    if let Some(v) = args.insecure {
        row.insecure = v;
        changed_fields.push("insecure".into());
    }
    if let Some(v) = args.enabled {
        row.enabled = v;
        changed_fields.push("enabled".into());
    }
    if !changed_fields.is_empty() {
        db::proxmox::update(&conn, &row)?;
        out.applied.extend(changed_fields);
    }
    drop(conn);

    // Lifecycle action
    match (args.vmid, args.ctid) {
        (Some(_), Some(_)) => anyhow::bail!("set either `vmid` or `ctid`, not both"),
        (None, None) => {}
        (vmid_or_ctid, ctid_or_none) => {
            let node = args
                .node
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("node required for lifecycle action"))?;
            let action_str = args
                .action
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("action required for lifecycle action"))?;
            let action: crate::ProxmoxAction = action_str.parse()?;
            let client = native_support::make_client(&args.name)?;
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
        anyhow::bail!(
            "no proxmox.update operation specified; pass field flags to patch or vmid/ctid + action for a lifecycle action"
        );
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// proxmox.delete — remove a registered endpoint
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxDeleteArgs {
    #[arg(long)]
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
