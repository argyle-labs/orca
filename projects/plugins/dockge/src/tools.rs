//! Dockge tool surface — flat 4-tool shape (`dockge.{list, detail, update,
//! delete}`). An endpoint is the primary resource; stacks nest into the
//! surface via the `endpoint` arg, and stack actions ride on `.update`
//! per [[feedback-one-tool-per-resource]].
//!
//! Endpoint resolution: tools accept the endpoint *name* and load
//! `(base_url, token)` from `db::dockge` at call time.
// Upstream Dockge stack-list and log payloads are opaque per their API.
#![allow(clippy::disallowed_types)]

use anyhow::Context;
use contract::JsonAny;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Client, Config};

// ── Row shapes ──────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DockgeEndpointEntry {
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
}

fn make_client(name: &str) -> anyhow::Result<Client> {
    let conn = db::open_default()?;
    let row = db::dockge::get(&conn, name)?
        .with_context(|| format!("dockge endpoint '{name}' not registered"))?;
    if !row.enabled {
        anyhow::bail!("dockge endpoint '{name}' is disabled");
    }
    Ok(Client::new(Config::new(row.base_url, row.token)))
}

// ═══════════════════════════════════════════════════════════════════════════
// dockge.list — endpoints; with `endpoint`, drill into stacks
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct DockgeListArgs {
    /// Drill into one endpoint. When set, the output's `stacks` is
    /// populated for that endpoint.
    #[cfg_attr(feature = "cli", arg(long))]
    pub endpoint: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DockgeListOutput {
    /// Always populated when `endpoint` is omitted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<DockgeEndpointEntry>,
    /// Populated when `endpoint` is set — opaque upstream stack list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stacks: Option<JsonAny>,
}

/// List Dockge resources. No args → registered endpoints. `endpoint` →
/// stacks for that endpoint.
#[orca_tool(domain = "dockge", verb = "list")]
async fn dockge_list(
    args: DockgeListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DockgeListOutput> {
    let mut out = DockgeListOutput::default();
    match args.endpoint.as_deref() {
        None => {
            let conn = db::open_default()?;
            out.endpoints = db::dockge::list(&conn)?
                .into_iter()
                .map(|r| DockgeEndpointEntry {
                    name: r.name,
                    base_url: r.base_url,
                    enabled: r.enabled,
                })
                .collect();
        }
        Some(ep) => {
            let client = make_client(ep)?;
            out.stacks = Some(client.list().await?.into());
        }
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// dockge.detail — single stack logs
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DockgeDetailArgs {
    pub endpoint: String,
    /// Stack name (e.g. "sonarr").
    pub stack: String,
}

/// Fetch recent logs for a single Dockge stack.
#[orca_tool(domain = "dockge", verb = "detail")]
async fn dockge_detail(
    args: DockgeDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<JsonAny> {
    let client = make_client(&args.endpoint)?;
    Ok(client.logs(&args.stack).await?.into())
}

// ═══════════════════════════════════════════════════════════════════════════
// dockge.update — register endpoint OR run a stack action
// ═══════════════════════════════════════════════════════════════════════════

/// Stack action verb. Anything other than these three is rejected by
/// `dockge.update` — Dockge itself supports only these lifecycle ops on
/// a stack.
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum DockgeStackAction {
    Start,
    Stop,
    Restart,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DockgeUpdateArgs {
    /// Endpoint register/update: `name` + `base_url` + `token`.
    #[cfg_attr(feature = "cli", arg(long))]
    pub name: Option<String>,
    #[cfg_attr(feature = "cli", arg(long))]
    pub base_url: Option<String>,
    #[cfg_attr(feature = "cli", arg(long))]
    pub token: Option<String>,

    /// Stack action: `endpoint` + `stack` + `action`.
    #[cfg_attr(feature = "cli", arg(long))]
    pub endpoint: Option<String>,
    #[cfg_attr(feature = "cli", arg(long))]
    pub stack: Option<String>,
    #[cfg_attr(feature = "cli", arg(long, value_enum))]
    pub action: Option<DockgeStackAction>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DockgeUpdateOutput {
    pub applied: Vec<String>,
    /// HTTP status from the action call (200 = success on Dockge).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_status: Option<u16>,
}

/// [MUTATES STATE] Register/update an endpoint, run a stack action, or both.
#[orca_tool(domain = "dockge", verb = "update")]
async fn dockge_update(
    args: DockgeUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DockgeUpdateOutput> {
    let mut out = DockgeUpdateOutput::default();

    if let Some(name) = &args.name {
        let base_url = args
            .base_url
            .clone()
            .ok_or_else(|| anyhow::anyhow!("base_url required to register endpoint"))?;
        let token = args
            .token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("token required to register endpoint"))?;
        let row = db::dockge::EndpointRow {
            name: name.clone(),
            base_url,
            token,
            enabled: true,
        };
        let conn = db::open_default()?;
        db::dockge::upsert(&conn, &row)?;
        out.applied.push(format!("endpoint-upserted:{name}"));
    }

    match (args.endpoint.as_deref(), args.stack.as_deref(), args.action) {
        (Some(ep), Some(stack), Some(action)) => {
            let client = make_client(ep)?;
            let result = match action {
                DockgeStackAction::Start => client.start(stack).await?,
                DockgeStackAction::Stop => client.stop(stack).await?,
                DockgeStackAction::Restart => client.restart(stack).await?,
            };
            out.action_status = Some(result.status);
            out.applied
                .push(format!("stack:{ep}:{stack}:{action:?}").to_lowercase());
        }
        (None, None, None) => {}
        _ => anyhow::bail!("stack action requires endpoint + stack + action together"),
    }

    if out.applied.is_empty() {
        anyhow::bail!("no dockge.update operation specified");
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// dockge.delete — remove a registered endpoint
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DockgeDeleteArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DockgeDeleteOutput {
    pub name: String,
    pub changed: bool,
}

#[orca_tool(domain = "dockge", verb = "delete")]
async fn dockge_delete(
    args: DockgeDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DockgeDeleteOutput> {
    let conn = db::open_default()?;
    let changed = db::dockge::remove(&conn, &args.name)?;
    Ok(DockgeDeleteOutput {
        name: args.name,
        changed,
    })
}
