//! Read-only aggregate fleet topology/capacity view.
//!
//! One call returns the whole tree — every joined peer plus the local host as a
//! node, each carrying its core/RAM capacity and the guests (VMs/containers/
//! LXCs) it claims to run — instead of fanning out `system.info.detail` per host
//! and stitching by hand. Reuses the update fan-out plumbing:
//! [`fleet_update::fleet_targets`] to enumerate the pod and
//! [`fleet_update::dispatch_at`] to read each host (in-process for the local
//! host, over the mesh for remote peers).
//!
//! Graceful degradation: an unreachable/timed-out peer becomes a node with
//! `reachable = false` + `error`, never a hard failure of the whole aggregate.

use anyhow::Result;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::fleet_update::{dispatch_at, fleet_targets};
use system::system_info_tool::{SystemInfoDetail, SystemInfoDetailArgs};
use system::system_info_types::SystemInfoReport;

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FleetTopologyArgs {}

/// One guest (VM / container / LXC) a node claims to run, projected from that
/// host's `SystemInfoReport.claims`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FleetGuest {
    /// Stable orca UUID for the claim when the reporter minted one; falls back
    /// to the provider-native id.
    pub id: String,
    /// Guest display name.
    pub name: String,
    /// `"vm"`, `"container"`, `"lxc"`.
    pub kind: String,
    /// Service roles attributed to the guest (from the claim's `service_role`
    /// hint). Empty when the provider couldn't attribute one.
    pub roles: Vec<String>,
    /// Hostname of the fleet node the guest actually runs on, when the provider
    /// resolved it; `None` = the reporting node is the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Normalized run-state (`"running"`/`"stopped"`/`"paused"`), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// One fleet node: a joined peer or the local host, with its capacity and guests.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FleetNode {
    /// Display hostname of the node.
    pub host: String,
    /// True when the per-host read succeeded. A false node carries `error` and
    /// leaves the capacity/guest fields at their empty defaults.
    pub reachable: bool,
    /// Per-host read error (connect/timeout/dispatch), populated only when
    /// `reachable = false`. One bad peer never aborts the aggregate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Logical CPU count reported by the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    /// Total RAM in MiB reported by the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_total_mb: Option<u64>,
    /// Guests (VMs/containers/LXCs) this node claims to run.
    pub guests: Vec<FleetGuest>,
}

/// The whole fleet tree plus a trivial capacity rollup over reachable nodes.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct FleetTopologyOutput {
    /// One entry per joined peer plus the local host.
    pub nodes: Vec<FleetNode>,
    /// Sum of `cores` over reachable nodes that reported a count.
    pub total_cores: u32,
    /// Sum of `mem_total_mb` over reachable nodes that reported it.
    pub total_mem_mb: u64,
}

/// Project one host's fat report into a reachable [`FleetNode`]. Pure — no I/O.
fn node_from_report(host: String, report: SystemInfoReport) -> FleetNode {
    let guests = report
        .claims
        .into_iter()
        .map(|c| FleetGuest {
            id: if c.uuid.is_empty() { c.id } else { c.uuid },
            name: c.name,
            kind: c.kind,
            roles: c.service_role.into_iter().collect(),
            parent: c.runs_on,
            state: c.state,
        })
        .collect();
    FleetNode {
        host,
        reachable: true,
        error: None,
        cores: report.cpu_logical,
        mem_total_mb: report.mem_total_mb,
        guests,
    }
}

/// Project a per-host read failure into an unreachable [`FleetNode`]. Pure.
fn node_from_error(host: String, error: String) -> FleetNode {
    FleetNode {
        host,
        reachable: false,
        error: Some(error),
        ..Default::default()
    }
}

/// Sum the capacity of reachable nodes into the output rollup.
fn rollup(out: &mut FleetTopologyOutput) {
    for n in &out.nodes {
        if n.reachable {
            out.total_cores += n.cores.unwrap_or(0);
            out.total_mem_mb += n.mem_total_mb.unwrap_or(0);
        }
    }
}

/// READ-ONLY. Aggregate the whole fleet's topology/capacity in one call: fan out
/// a fat host read to every joined peer plus the local host and return the tree
/// (nodes → cores/RAM → guests + roles). An unreachable peer becomes a node with
/// `reachable = false` + `error` rather than failing the aggregate.
#[orca_tool(domain = "pod", verb = "topology")]
async fn topology(
    _args: FleetTopologyArgs,
    ctx: &contract::ToolCtx,
) -> Result<FleetTopologyOutput> {
    let mut out = FleetTopologyOutput::default();
    let targets = fleet_targets()?;
    for t in &targets {
        let node =
            match dispatch_at::<SystemInfoDetail>(t, SystemInfoDetailArgs::default(), ctx).await {
                Ok(detail) => node_from_report(t.host.clone(), detail.host),
                Err(e) => node_from_error(t.host.clone(), format!("{e:#}")),
            };
        out.nodes.push(node);
    }
    rollup(&mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::TopologyClaim;

    fn report_with(cpu: u32, mem: u64, claims: Vec<TopologyClaim>) -> SystemInfoReport {
        SystemInfoReport {
            cpu_logical: Some(cpu),
            mem_total_mb: Some(mem),
            claims,
            ..Default::default()
        }
    }

    #[test]
    fn maps_capacity_and_guests_from_report() {
        let claim = TopologyClaim {
            kind: "lxc".into(),
            id: "113".into(),
            uuid: "uuid-113".into(),
            name: "jellyfin".into(),
            provider: "proxmox".into(),
            provider_instance: "frigg".into(),
            runs_on: Some("frigg".into()),
            service_role: Some("media-server".into()),
            state: Some("running".into()),
            ..Default::default()
        };
        let node = node_from_report("frigg".into(), report_with(8, 32_768, vec![claim]));
        assert!(node.reachable);
        assert_eq!(node.error, None);
        assert_eq!(node.cores, Some(8));
        assert_eq!(node.mem_total_mb, Some(32_768));
        assert_eq!(node.guests.len(), 1);
        let g = &node.guests[0];
        assert_eq!(g.id, "uuid-113"); // uuid preferred over native id
        assert_eq!(g.name, "jellyfin");
        assert_eq!(g.kind, "lxc");
        assert_eq!(g.roles, vec!["media-server".to_string()]);
        assert_eq!(g.parent.as_deref(), Some("frigg"));
        assert_eq!(g.state.as_deref(), Some("running"));
    }

    #[test]
    fn guest_id_falls_back_to_native_when_no_uuid() {
        let claim = TopologyClaim {
            kind: "container".into(),
            id: "abc123".into(),
            name: "sonarr".into(),
            provider: "docker".into(),
            provider_instance: "local".into(),
            ..Default::default()
        };
        let node = node_from_report("baldur".into(), report_with(4, 8_192, vec![claim]));
        let g = &node.guests[0];
        assert_eq!(g.id, "abc123");
        assert!(g.roles.is_empty());
        assert_eq!(g.parent, None);
    }

    #[test]
    fn error_target_yields_unreachable_node() {
        let node = node_from_error("bragi".into(), "connect timed out".into());
        assert!(!node.reachable);
        assert_eq!(node.error.as_deref(), Some("connect timed out"));
        assert_eq!(node.cores, None);
        assert_eq!(node.mem_total_mb, None);
        assert!(node.guests.is_empty());
    }

    #[test]
    fn rollup_sums_only_reachable_nodes() {
        let mut out = FleetTopologyOutput {
            nodes: vec![
                node_from_report("thor".into(), report_with(16, 65_536, vec![])),
                node_from_report("loki".into(), report_with(8, 32_768, vec![])),
                node_from_error("hemlock".into(), "unreachable".into()),
            ],
            ..Default::default()
        };
        rollup(&mut out);
        assert_eq!(out.total_cores, 24);
        assert_eq!(out.total_mem_mb, 98_304);
    }
}
