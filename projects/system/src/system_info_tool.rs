//! On-demand host-facts endpoints split out of `system.detail`.
//!
//! `system.detail` is the LEAN install/state/config snapshot (plus lean
//! topology facts) that the pod roster and topology views fetch and keep warm.
//! The FAT host snapshot — hardware, per-process CPU, network interfaces, GPUs,
//! filesystem capacity, and the metrics history ring — lives here on
//! `system.info.detail`, fetched on demand and peer-dispatchable, so it is
//! never embedded in the mesh-traversed roster and never dialed on a read path.
//!
//! The one growing list on the fat report — `claims` (VMs/containers a host
//! runs) — is paginated on its own `system.info.claims.list` verb, mirroring
//! `pod.list` / `containers.list`. Bounded lists (`top_processes`,
//! `interfaces`, `gpus`) stay inline. Time-series stays on `system.history`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::system_detail_view::{DEFAULT_POINTS, SystemDetailView, build_view};
use crate::system_info::{current_or_collect, history::read_tail};
use crate::system_info_types::SystemInfoReport;
use derive::orca_tool;

/// Fat host snapshot plus optional SVG-projected metric charts. The report
/// carries the full hardware/process/interface/GPU/history detail; `charts` is
/// filled only when the caller supplies both chart dimensions (native/UI
/// clients) — CLI/automation callers omit them and read the raw `history` ring
/// on the report or the cursor-paginated `system.history`.
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SystemInfoDetailOutput {
    /// Cross-platform OS / hardware / process / network snapshot.
    pub host: SystemInfoReport,
    /// Chart-ready SVG-space metric series (CPU/mem/GPU history), projected to
    /// the caller's `chartWidth`/`chartHeight`. Absent unless both dimensions
    /// are supplied. Folded in from the former `system.detail_view` tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charts: Option<SystemDetailView>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemInfoDetailArgs {
    /// SVG-space width to project the chart series against. Supply with
    /// `chartHeight` to have the response fill its `charts` section.
    #[arg(long)]
    pub chart_width: Option<u32>,
    /// SVG-space height to project the chart series against. Requires
    /// `chartWidth`.
    #[arg(long)]
    pub chart_height: Option<u32>,
    /// Tail length of history to project into the chart series. Defaults to
    /// ≈1h of samples. Only consulted when charts are requested.
    #[arg(long)]
    pub chart_points: Option<usize>,
}

/// Fat host snapshot for one host: OS, hardware, per-process CPU, network
/// interfaces, GPUs, filesystem capacity, and the metrics history ring. Pass
/// `chartWidth`+`chartHeight` to additionally get SVG-projected metric charts.
/// On-demand and peer-dispatchable — never embedded in the pod roster.
#[orca_tool(domain = "system.info", verb = "detail")]
async fn system_info_detail(
    args: SystemInfoDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SystemInfoDetailOutput> {
    let host = (*current_or_collect()).clone();
    let charts = match (args.chart_width, args.chart_height) {
        (Some(w), Some(h)) => {
            let n = args.chart_points.unwrap_or(DEFAULT_POINTS);
            Some(build_view(&read_tail(n), w, h))
        }
        _ => None,
    };
    Ok(SystemInfoDetailOutput { host, charts })
}

/// One page of a host's topology claims.
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SystemInfoClaimsOutput {
    /// Claims on this page (VMs/containers/LXCs this host runs).
    pub claims: Vec<contract::TopologyClaim>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total claims across all pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemInfoClaimsArgs {
    /// Max items to return this page. Clamped to `[1, 200]`; defaults to 50.
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

/// Cursor-paginated list of the topology claims this host reports — the one
/// list on the fat host snapshot that grows with hosted VMs/containers.
/// Mirrors `pod.list` / `containers.list`.
#[orca_tool(domain = "system.info.claims", verb = "list")]
async fn system_info_claims_list(
    args: SystemInfoClaimsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SystemInfoClaimsOutput> {
    let claims = current_or_collect().claims.clone();
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(claims, &params);
    Ok(SystemInfoClaimsOutput {
        claims: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}
