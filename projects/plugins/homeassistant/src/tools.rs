//! Home Assistant tool surface — flat 4-tool surface
//! (`home-assistant.{list, detail, update, delete}`). An endpoint is the
//! primary resource; entities, automations, and service calls nest into
//! the surface — no sub-resource flag.
// service data is upstream-defined free-form JSON per HA spec.
#![allow(clippy::disallowed_types)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json as sj;

use derive::orca_tool;

use anyhow::Context;
use contract::JsonAny;

use crate::{Client, Config, ServiceCall};

// ── Row shapes ──────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HaEndpointEntry {
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
}

fn make_client(name: &str) -> anyhow::Result<Client> {
    let conn = db::open_default()?;
    let row = db::home_assistant::get(&conn, name)?
        .with_context(|| format!("home assistant endpoint '{name}' not registered"))?;
    if !row.enabled {
        anyhow::bail!("home assistant endpoint '{name}' is disabled");
    }
    Ok(Client::new(Config::new(row.base_url, row.token)))
}

// ═══════════════════════════════════════════════════════════════════════════
// home-assistant.list — endpoints; with `endpoint`, drill in to entities+automations
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct HaListArgs {
    /// Drill into one endpoint. When set, the output's `entities` +
    /// `automations` are populated for that endpoint.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Optional HA domain filter for entities (light, sensor, switch, …).
    #[arg(long)]
    pub domain: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct HaListOutput {
    /// Always populated when `endpoint` is omitted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<HaEndpointEntry>,
    /// Populated when `endpoint` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entities: Option<JsonAny>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automations: Option<JsonAny>,
}

/// List HA resources. No args → registered endpoints. `endpoint` → entities
/// (optionally `domain`-filtered) and automations for that endpoint.
#[orca_tool(domain = "home-assistant", verb = "list")]
async fn ha_list(args: HaListArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<HaListOutput> {
    let mut out = HaListOutput::default();
    match args.endpoint.as_deref() {
        None => {
            let conn = db::open_default()?;
            out.endpoints = db::home_assistant::list(&conn)?
                .into_iter()
                .map(|r| HaEndpointEntry {
                    name: r.name,
                    base_url: r.base_url,
                    enabled: r.enabled,
                })
                .collect();
        }
        Some(ep) => {
            let client = make_client(ep)?;
            out.entities = Some(client.entity_list(args.domain.as_deref()).await?.into());
            out.automations = Some(client.automation_list().await?.into());
        }
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// home-assistant.detail — single entity state
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct HaDetailArgs {
    #[arg(long)]
    pub endpoint: String,
    /// Entity ID (e.g. "light.living_room").
    #[arg(long)]
    pub entity_id: String,
}

/// Fetch the current state of a single HA entity.
#[orca_tool(domain = "home-assistant", verb = "detail")]
async fn ha_detail(args: HaDetailArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<JsonAny> {
    let client = make_client(&args.endpoint)?;
    Ok(client.entity_state(&args.entity_id).await?.into())
}

// ═══════════════════════════════════════════════════════════════════════════
// home-assistant.create — register a new endpoint
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct HaCreateArgs {
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub base_url: String,
    #[arg(long)]
    pub token: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaCreateOutput {
    pub endpoint: HaEndpointEntry,
}

/// [MUTATES STATE] Register a new Home Assistant endpoint. Errors if `name` is
/// already taken — use home-assistant.update to modify an existing endpoint.
#[orca_tool(domain = "home-assistant", verb = "create")]
async fn ha_create(args: HaCreateArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<HaCreateOutput> {
    let row = db::home_assistant::EndpointRow {
        name: args.name.clone(),
        base_url: args.base_url.clone(),
        token: args.token.clone(),
        enabled: true,
    };
    let conn = db::open_default()?;
    db::home_assistant::insert(&conn, &row).map_err(|e| {
        if e.to_string().contains("UNIQUE") {
            anyhow::anyhow!(
                "home-assistant endpoint '{}' already exists — use home-assistant.update to modify it",
                row.name
            )
        } else {
            e
        }
    })?;
    Ok(HaCreateOutput {
        endpoint: HaEndpointEntry {
            name: row.name,
            base_url: row.base_url,
            enabled: row.enabled,
        },
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// home-assistant.update — PATCH endpoint fields OR invoke a service
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct HaUpdateArgs {
    /// Endpoint to patch or target for a service call.
    #[arg(long)]
    pub name: String,

    // PATCH fields
    #[arg(long)]
    pub base_url: Option<String>,
    #[arg(long)]
    pub token: Option<String>,
    #[arg(long)]
    pub enabled: Option<bool>,

    // Service invocation
    /// HA service domain (light, switch, automation, …).
    #[arg(long)]
    pub service_domain: Option<String>,
    /// HA service name (turn_on, toggle, …).
    #[arg(long)]
    pub service_name: Option<String>,
    #[arg(long)]
    pub entity_id: Option<String>,
    /// Opaque free-form service-data — upstream-defined.
    #[arg(skip)]
    pub service_data: Option<sj::Map<String, sj::Value>>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct HaUpdateOutput {
    pub applied: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_result: Option<JsonAny>,
}

/// [MUTATES STATE] Patch an existing HA endpoint's fields or invoke a service.
/// Endpoint must already exist.
#[orca_tool(domain = "home-assistant", verb = "update")]
async fn ha_update(args: HaUpdateArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<HaUpdateOutput> {
    let conn = db::open_default()?;
    let mut row = db::home_assistant::get(&conn, &args.name)?
        .with_context(|| format!("home-assistant endpoint '{}' not registered", args.name))?;
    let mut out = HaUpdateOutput::default();

    // PATCH fields
    let mut changed = Vec::new();
    if let Some(v) = args.base_url {
        row.base_url = v;
        changed.push("base_url");
    }
    if let Some(v) = args.token {
        row.token = v;
        changed.push("token");
    }
    if let Some(v) = args.enabled {
        row.enabled = v;
        changed.push("enabled");
    }
    if !changed.is_empty() {
        db::home_assistant::update(&conn, &row)?;
        out.applied.extend(changed.iter().map(|s| s.to_string()));
    }
    drop(conn);

    // Service invocation
    match (args.service_domain.clone(), args.service_name.clone()) {
        (Some(d), Some(s)) => {
            let client = make_client(&args.name)?;
            let call = ServiceCall {
                domain: d,
                service: s.clone(),
                entity_id: args.entity_id.clone(),
                data: args.service_data.clone().unwrap_or_default(),
            };
            out.service_result = Some(client.service_call(&call).await?.into());
            out.applied.push(format!("service:{}", s));
        }
        (None, None) => {}
        _ => anyhow::bail!("service invocation requires both service_domain and service_name"),
    }

    if out.applied.is_empty() {
        anyhow::bail!("no home-assistant.update operation specified");
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// home-assistant.delete — remove a registered endpoint
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct HaDeleteArgs {
    #[arg(long)]
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaDeleteOutput {
    pub name: String,
    pub changed: bool,
}

#[orca_tool(domain = "home-assistant", verb = "delete")]
async fn ha_delete(args: HaDeleteArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<HaDeleteOutput> {
    let conn = db::open_default()?;
    let changed = db::home_assistant::remove(&conn, &args.name)?;
    Ok(HaDeleteOutput {
        name: args.name,
        changed,
    })
}
