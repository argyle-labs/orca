//! Config store tools — CRUD over the host-owned config row store.
//!
//! Four canonical verbs (six-verb surface — no domain verb names):
//!   - `config.list`   — enumerate rows (optionally filtered by noun/host).
//!   - `config.detail` — fetch one row by noun+name.
//!   - `config.upsert` — create-or-replace a row owned by the local host
//!     (cross-host writes route via mesh once §3.3 lands).
//!   - `config.delete` — remove a row owned by the local host.
//!
//! Each `config_row` carries a `host_owner`. Only the owning host may
//! mutate.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

// ── Args / Output ────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ConfigRowOut {
    pub id: String,
    pub host_owner: String,
    pub noun: String,
    pub name: String,
    pub json: String,
    pub is_replica: bool,
    pub updated_at: String,
    pub updated_by: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct ConfigListArgs {
    /// Filter by noun (service, schedule, backup_job, nfs_watch, …).
    #[arg(long)]
    pub noun: Option<String>,
    /// Filter by host_owner.
    #[arg(long)]
    pub host: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ConfigListOutput {
    pub rows: Vec<ConfigRowOut>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ConfigGetArgs {
    /// Row noun (service, schedule, backup_job, …).
    pub noun: String,
    /// Row name (e.g. "plex", "host.backup").
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ConfigGetOutput {
    pub row: Option<ConfigRowOut>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ConfigSetArgs {
    pub noun: String,
    pub name: String,
    /// JSON payload for the row. Must be a valid JSON document.
    pub json: String,
    /// host_owner. Defaults to the local host's display_name. Must equal
    /// the local host until cross-host routing lands (§3.3).
    #[arg(long)]
    pub host: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ConfigSetOutput {
    pub row: ConfigRowOut,
    pub created: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ConfigDeleteArgs {
    pub noun: String,
    pub name: String,
    /// host_owner. Defaults to the local host's display_name.
    #[arg(long)]
    pub host: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ConfigDeleteOutput {
    pub removed: bool,
}

// ── Native support ───────────────────────────────────────────────────────────

mod native_support {
    use super::*;

    impl From<db::config_store::ConfigRow> for ConfigRowOut {
        fn from(r: db::config_store::ConfigRow) -> Self {
            ConfigRowOut {
                id: r.id,
                host_owner: r.host_owner,
                noun: r.noun,
                name: r.name,
                json: r.json,
                is_replica: r.is_replica,
                updated_at: r.updated_at,
                updated_by: r.updated_by,
            }
        }
    }

    /// Resolve this host's canonical name for config-row ownership.
    /// Prefers the `host.display_name` setting (operator-set), falls back
    /// to the OS hostname. Mirrors what `host.info` reports.
    pub(super) fn local_host(conn: &db::Conn) -> String {
        db::settings::get(conn, "host.display_name")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "unknown".to_string())
            })
    }
}

// ── Tools ────────────────────────────────────────────────────────────────────

/// List config rows. Optionally filter by noun and/or host_owner.
#[orca_tool(domain = "config", verb = "list")]
async fn config_list(
    args: ConfigListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ConfigListOutput> {
    let conn = db::open_default()?;
    let rows = db::config_store::list(&conn, args.noun.as_deref(), args.host.as_deref())?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(ConfigListOutput { rows })
}

/// Fetch a single config row by noun+name.
#[orca_tool(domain = "config", verb = "detail")]
async fn config_get(
    args: ConfigGetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ConfigGetOutput> {
    let conn = db::open_default()?;
    let row = db::config_store::get(&conn, &args.noun, &args.name)?.map(Into::into);
    Ok(ConfigGetOutput { row })
}

/// Upsert a config row. Refuses to write rows owned by a different host
/// — cross-host writes route via the pod mesh once peer-tool dispatch
/// lands (§3.3).
#[orca_tool(domain = "config", verb = "upsert")]
async fn config_set(
    args: ConfigSetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ConfigSetOutput> {
    let conn = db::open_default()?;
    let local = native_support::local_host(&conn);
    let owner = args.host.unwrap_or_else(|| local.clone());
    let created = db::config_store::set(
        &conn, &local, &owner, &args.noun, &args.name, &args.json, "cli",
    )?;
    let row = db::config_store::get(&conn, &args.noun, &args.name)?
        .ok_or_else(|| anyhow::anyhow!("row vanished after write"))?
        .into();
    Ok(ConfigSetOutput { row, created })
}

/// Delete a config row owned by the local host.
#[orca_tool(domain = "config", verb = "delete")]
async fn config_delete(
    args: ConfigDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ConfigDeleteOutput> {
    let conn = db::open_default()?;
    let local = native_support::local_host(&conn);
    let owner = args.host.unwrap_or_else(|| local.clone());
    let removed = db::config_store::delete(&conn, &local, &owner, &args.noun, &args.name, "cli")?;
    Ok(ConfigDeleteOutput { removed })
}
