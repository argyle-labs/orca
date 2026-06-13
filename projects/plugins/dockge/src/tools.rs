//! Dockge tool surface — REST-shaped 5-verb resource (`dockge.{list,
//! detail, create, update, delete}`) per the REST-verbs-for-tool-
//! surfaces rule. An endpoint is the primary resource; stacks nest
//! into the surface via the `endpoint` arg, and stack lifecycle
//! actions (start/stop/restart) ride on `.update` via an action enum
//! arg per the one-tool-per-resource rule (stack is the resource,
//! `.update` is the verb, action is the transition).
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

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct DockgeListArgs {
    /// Drill into one endpoint. When set, the output's `stacks` is
    /// populated for that endpoint.
    #[arg(long)]
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

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
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
// dockge.create — register a new endpoint (POST semantics)
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct DockgeCreateArgs {
    /// Unique endpoint name (operator-chosen identifier).
    #[arg(long)]
    pub name: String,
    /// Reachable URL of the Dockge instance.
    #[arg(long)]
    pub base_url: String,
    /// Bearer token. Generate one in the Dockge UI.
    #[arg(long)]
    pub token: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DockgeCreateOutput {
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
}

/// [MUTATES STATE] Register a new Dockge endpoint. Errors if `name`
/// is already taken — use `dockge.update` to modify an existing
/// endpoint.
#[orca_tool(domain = "dockge", verb = "create")]
async fn dockge_create(
    args: DockgeCreateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DockgeCreateOutput> {
    let row = db::dockge::EndpointRow {
        name: args.name.clone(),
        base_url: args.base_url.clone(),
        token: args.token,
        enabled: true,
    };
    let conn = db::open_default()?;
    db::dockge::insert(&conn, &row).map_err(|e| {
        let msg = format!("{e:#}");
        if msg.contains("UNIQUE") || msg.contains("PRIMARY") {
            anyhow::anyhow!(
                "dockge endpoint '{}' already exists; use dockge.update",
                row.name
            )
        } else {
            e
        }
    })?;
    Ok(DockgeCreateOutput {
        name: row.name,
        base_url: row.base_url,
        enabled: row.enabled,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// dockge.update — modify endpoint OR run a stack action (PATCH semantics)
// ═══════════════════════════════════════════════════════════════════════════

/// Stack lifecycle action. Anything other than these three is rejected
/// by `dockge.update` — Dockge itself only supports these.
#[derive(clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum DockgeStackAction {
    Start,
    Stop,
    Restart,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DockgeUpdateArgs {
    /// Endpoint modify: `name` + at least one of `base_url`/`token`/`enabled`.
    /// Errors if `name` is not registered.
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub base_url: Option<String>,
    #[arg(long)]
    pub token: Option<String>,
    #[arg(long)]
    pub enabled: Option<bool>,

    /// Stack action: `endpoint` + `stack` + `action`.
    #[arg(long)]
    pub endpoint: Option<String>,
    #[arg(long)]
    pub stack: Option<String>,
    #[arg(long, value_enum)]
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

/// [MUTATES STATE] Modify a registered endpoint, run a stack action,
/// or both. PATCH semantics — endpoint must already exist.
#[orca_tool(domain = "dockge", verb = "update")]
async fn dockge_update(
    args: DockgeUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DockgeUpdateOutput> {
    let mut out = DockgeUpdateOutput::default();

    if let Some(name) = &args.name {
        let conn = db::open_default()?;
        let mut row = db::dockge::get(&conn, name)?.ok_or_else(|| {
            anyhow::anyhow!("dockge endpoint '{name}' not registered; use dockge.create")
        })?;
        if let Some(base_url) = args.base_url.clone() {
            row.base_url = base_url;
        }
        if let Some(token) = args.token.clone() {
            row.token = token;
        }
        if let Some(enabled) = args.enabled {
            row.enabled = enabled;
        }
        let changed = db::dockge::update(&conn, &row)?;
        if !changed {
            anyhow::bail!("dockge endpoint '{name}' update reported no row change");
        }
        out.applied.push(format!("endpoint-updated:{name}"));
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

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
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
