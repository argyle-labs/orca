//! Per-peer retention controls.
//!
//! Three knobs per system, three resolution layers (peer override → global
//! default → built-in):
//!
//! * `days`     — age cap; rows older than N days are deleted.
//! * `max_mb`   — total `payload_json` bytes per peer; oldest first.
//! * `max_rows` — hard row count.
//!
//! Setting a knob without `peer` writes the global default. Setting with
//! `peer` writes the per-peer override. `unset=true` removes the override
//! (falling back to the global / built-in).
//!
//! Acceptance: every on-disk artifact owned by a peer (DB rows, JSONL
//! history ring) must honor that peer's caps. See `host_status_sweep`
//! for the periodic enforcer.

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RetentionView {
    /// `None` for the global default row, `Some(peer)` for a per-peer override.
    pub peer_id: Option<String>,
    /// Resolved age cap in days. Always present (falls back to built-in default).
    pub days: f64,
    /// Resolved size cap in megabytes. `None` = unlimited.
    pub max_mb: Option<f64>,
    /// Resolved row count cap. Always present (falls back to safety guard).
    pub max_rows: i64,
}

// ── get ──────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RetentionGetArgs {
    /// Peer id to resolve. Omit for the global default view.
    #[arg(long)]
    pub peer: Option<String>,
}

/// Resolve the effective retention policy for one peer (or the global
/// default if `peer` is omitted). Returns the same shape `set` accepts.
#[orca_tool(domain = "system", verb = "retention_get")]
async fn system_retention_get(
    args: RetentionGetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<RetentionView> {
    let peer_for_resolve = args.peer.clone().unwrap_or_default();
    let view = db::pool::with_pooled_or_open(|conn| {
        let policy = db::host_status::retention_for(conn, &peer_for_resolve);
        Ok(RetentionView {
            peer_id: args.peer.clone(),
            days: policy.age_secs as f64 / 86_400.0,
            max_mb: policy.max_bytes.map(|b| b as f64 / 1_048_576.0),
            max_rows: policy.max_rows,
        })
    })?;
    Ok(view)
}

// ── set ──────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RetentionSetArgs {
    /// Peer id to scope this knob to. Omit to write the global default.
    #[arg(long)]
    pub peer: Option<String>,
    /// Age cap in days. Pass to set, omit to leave unchanged.
    #[arg(long)]
    pub days: Option<f64>,
    /// Size cap in megabytes. Pass to set, omit to leave unchanged.
    #[arg(long = "max-mb")]
    pub max_mb: Option<f64>,
    /// Row count cap. Pass to set, omit to leave unchanged.
    #[arg(long = "max-rows")]
    pub max_rows: Option<i64>,
    /// When true, REMOVE any override and fall back to the global / built-in
    /// default. Mutually exclusive with the value args.
    #[arg(long, default_value_t = false)]
    pub unset: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RetentionSetOutput {
    pub effective: RetentionView,
}

/// Set one or more retention knobs for a peer (or the global default).
/// Returns the resolved policy after the write.
#[orca_tool(domain = "system", verb = "retention_set")]
async fn system_retention_set(
    args: RetentionSetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<RetentionSetOutput> {
    let local_host = crate::host_identity::machine_id_short().to_string();
    let peer_param = args.peer.as_deref();
    let peer_for_resolve = args.peer.clone().unwrap_or_default();

    db::pool::with_pooled_or_open(|conn| {
        if args.unset {
            db::host_status::set_retention_days(conn, &local_host, peer_param, None)?;
            db::host_status::set_retention_max_mb(conn, &local_host, peer_param, None)?;
            db::host_status::set_retention_max_rows(conn, &local_host, peer_param, None)?;
        } else {
            if let Some(d) = args.days {
                db::host_status::set_retention_days(conn, &local_host, peer_param, Some(d))?;
            }
            if let Some(m) = args.max_mb {
                db::host_status::set_retention_max_mb(conn, &local_host, peer_param, Some(m))?;
            }
            if let Some(r) = args.max_rows {
                db::host_status::set_retention_max_rows(conn, &local_host, peer_param, Some(r))?;
            }
        }
        Ok(())
    })?;

    let effective = db::pool::with_pooled_or_open(|conn| {
        let policy = db::host_status::retention_for(conn, &peer_for_resolve);
        Ok(RetentionView {
            peer_id: args.peer.clone(),
            days: policy.age_secs as f64 / 86_400.0,
            max_mb: policy.max_bytes.map(|b| b as f64 / 1_048_576.0),
            max_rows: policy.max_rows,
        })
    })?;

    Ok(RetentionSetOutput { effective })
}

// ── list ─────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RetentionListArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RetentionListOutput {
    /// One row per peer present in `host_status`, plus a row with
    /// `peerId=None` representing the global default.
    pub rows: Vec<RetentionView>,
}

/// Resolved retention for every peer + the global default. UI uses this
/// to render the per-system retention controls.
#[orca_tool(domain = "system", verb = "retention_list")]
async fn system_retention_list(
    _args: RetentionListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<RetentionListOutput> {
    let rows = db::pool::with_pooled_or_open(|conn| {
        let mut rows = Vec::new();

        // Global default first.
        let global = db::host_status::retention_for(conn, "");
        rows.push(RetentionView {
            peer_id: None,
            days: global.age_secs as f64 / 86_400.0,
            max_mb: global.max_bytes.map(|b| b as f64 / 1_048_576.0),
            max_rows: global.max_rows,
        });

        // Then one row per peer present in host_status.
        for peer_id in db::host_status::distinct_peer_ids(conn)? {
            let p = db::host_status::retention_for(conn, &peer_id);
            rows.push(RetentionView {
                peer_id: Some(peer_id),
                days: p.age_secs as f64 / 86_400.0,
                max_mb: p.max_bytes.map(|b| b as f64 / 1_048_576.0),
                max_rows: p.max_rows,
            });
        }
        Ok(rows)
    })?;
    Ok(RetentionListOutput { rows })
}
