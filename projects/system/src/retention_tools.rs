//! Retention controls — per-peer host_status caps plus instance-global
//! history knobs.
//!
//! **Per-peer host_status caps** (three knobs, three resolution layers:
//! peer override → global default → built-in):
//!
//! * `days`     — age cap; rows older than N days are deleted.
//! * `max_mb`   — total `payload_json` bytes per peer; oldest first.
//! * `max_rows` — hard row count.
//!
//! Setting a knob without `peer` writes the global default. Setting with
//! `peer` writes the per-peer override. `unset=true` removes the override
//! (falling back to the global / built-in).
//!
//! **Instance-global knobs** (one value per orca instance, not per peer):
//!
//! * `scheduler_runs_per_job` — rows kept per job in `scheduler_runs`.
//! * `session_events_days`    — age cap (days) for the `session_events` audit log.
//!
//! These are only meaningful on the global view (`peer` omitted); they are
//! reported and settable there and ignored when a `peer` is given.
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
    /// Instance-global: rows kept per job in `scheduler_runs`. Populated only
    /// on the global (peerId=null) view; `None` for per-peer rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduler_runs_per_job: Option<i64>,
    /// Instance-global: `session_events` retention window in days. Populated
    /// only on the global (peerId=null) view; `None` for per-peer rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_events_days: Option<i64>,
}

/// Build a view for `peer_id`, resolving the per-peer host_status policy and —
/// only for the global row (`peer_id == None`) — the instance-global knobs.
fn build_view(conn: &rusqlite::Connection, peer_id: Option<String>) -> RetentionView {
    let resolve_key = peer_id.clone().unwrap_or_default();
    let policy = db::host_status::retention_for(conn, &resolve_key);
    let (scheduler_runs_per_job, session_events_days) = if peer_id.is_none() {
        (
            Some(db::scheduler_runs::retain_per_job(conn)),
            Some(db::maintenance::session_events_retention_days(conn) as i64),
        )
    } else {
        (None, None)
    };
    RetentionView {
        peer_id,
        days: policy.age_secs as f64 / 86_400.0,
        max_mb: policy.max_bytes.map(|b| b as f64 / 1_048_576.0),
        max_rows: policy.max_rows,
        scheduler_runs_per_job,
        session_events_days,
    }
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
    let view = db::pool::with_pooled_or_open(|conn| Ok(build_view(conn, args.peer.clone())))?;
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
    /// Instance-global: rows kept per job in `scheduler_runs`. Only valid on
    /// the global view (omit `peer`); must be positive. Omit to leave unchanged.
    #[arg(long = "scheduler-runs-per-job")]
    pub scheduler_runs_per_job: Option<i64>,
    /// Instance-global: `session_events` retention window in days. Only valid
    /// on the global view (omit `peer`). Omit to leave unchanged.
    #[arg(long = "session-events-days")]
    pub session_events_days: Option<i64>,
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

    // Instance-global knobs are only meaningful for the whole instance, not a
    // single peer — reject them when scoped to a peer rather than silently
    // writing an instance-wide value under a per-peer intent.
    if peer_param.is_some()
        && (args.scheduler_runs_per_job.is_some() || args.session_events_days.is_some())
    {
        anyhow::bail!(
            "scheduler-runs-per-job / session-events-days are instance-global; omit --peer to set them"
        );
    }
    if matches!(args.scheduler_runs_per_job, Some(n) if n <= 0) {
        anyhow::bail!("scheduler-runs-per-job must be positive");
    }
    if matches!(args.session_events_days, Some(d) if d < 0) {
        anyhow::bail!("session-events-days must be zero or positive");
    }

    db::pool::with_pooled_or_open(|conn| {
        if args.unset {
            db::host_status::set_retention_days(conn, &local_host, peer_param, None)?;
            db::host_status::set_retention_max_mb(conn, &local_host, peer_param, None)?;
            db::host_status::set_retention_max_rows(conn, &local_host, peer_param, None)?;
            // Instance-global knobs only reset on the global view.
            if peer_param.is_none() {
                db::settings::delete(conn, db::scheduler_runs::RETAIN_SETTING)?;
                db::settings::delete(conn, db::maintenance::SESSION_EVENTS_RETENTION_SETTING)?;
            }
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
            if let Some(n) = args.scheduler_runs_per_job {
                db::settings::set(conn, db::scheduler_runs::RETAIN_SETTING, &n.to_string())?;
            }
            if let Some(d) = args.session_events_days {
                db::settings::set(
                    conn,
                    db::maintenance::SESSION_EVENTS_RETENTION_SETTING,
                    &d.to_string(),
                )?;
            }
        }
        Ok(())
    })?;

    let effective = db::pool::with_pooled_or_open(|conn| Ok(build_view(conn, args.peer.clone())))?;

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

        // Global default first (carries the instance-global knobs).
        rows.push(build_view(conn, None));

        // Then one row per peer present in host_status.
        for peer_id in db::host_status::distinct_peer_ids(conn)? {
            rows.push(build_view(conn, Some(peer_id)));
        }
        Ok(rows)
    })?;
    Ok(RetentionListOutput { rows })
}
