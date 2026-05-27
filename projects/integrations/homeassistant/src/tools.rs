//! Home Assistant OrcaTool surface. Tools live in the crate that owns the
//! work; the `#[orca_tool]` macro is the only tool-definition surface.
// HaServiceCallArgs.data uses Map<String, Value> — HA service data is
// free-form by spec.
#![allow(clippy::disallowed_types)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use orca_macro::orca_tool;

#[cfg(feature = "native")]
use anyhow::Context;
#[cfg(feature = "native")]
use orca_contract::JsonAny;

#[cfg(feature = "native")]
use crate::{Client, Config, ServiceCall};

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaEntityListArgs {
    /// Name of a Home Assistant endpoint registered via add_home_assistant_endpoint
    pub endpoint: String,
    /// Optional domain filter (e.g. "light", "sensor", "switch")
    pub domain: Option<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaEntityStateArgs {
    pub endpoint: String,
    /// Entity ID (e.g. "light.living_room")
    pub entity_id: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaAutomationListArgs {
    pub endpoint: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaServiceCallArgs {
    pub endpoint: String,
    /// Service domain (e.g. "light", "switch", "automation")
    pub domain: String,
    /// Service name (e.g. "turn_on", "toggle")
    pub service: String,
    /// Optional entity_id target (e.g. "light.living_room")
    pub entity_id: Option<String>,
    /// Optional service data payload merged into the request body.
    /// Shape is service-defined — HA does not publish a typed schema per service.
    #[allow(clippy::disallowed_types)]
    pub data: Option<Map<String, Value>>,
}

#[cfg(feature = "native")]
fn make_client(name: &str) -> anyhow::Result<Client> {
    let conn = orca_db::open_default()?;
    let row = orca_db::home_assistant::get(&conn, name)?.with_context(|| {
        format!("home assistant endpoint '{name}' not registered (use add_home_assistant_endpoint)")
    })?;
    if !row.enabled {
        anyhow::bail!("home assistant endpoint '{name}' is disabled");
    }
    Ok(Client::new(Config::new(row.base_url, row.token)))
}

/// List Home Assistant entities for a registered endpoint, optionally filtered by domain.
#[orca_tool(domain = "ha.entity", verb = "list")]
async fn ha_entity_list(
    args: HaEntityListArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<JsonAny> {
    let client = make_client(&args.endpoint)?;
    Ok(client.entity_list(args.domain.as_deref()).await?.into())
}

/// Fetch the current state of a single Home Assistant entity.
#[orca_tool(domain = "ha.entity", verb = "detail")]
async fn ha_entity_detail(
    args: HaEntityStateArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<JsonAny> {
    let client = make_client(&args.endpoint)?;
    Ok(client.entity_state(&args.entity_id).await?.into())
}

/// List Home Assistant automations for a registered endpoint.
#[orca_tool(domain = "ha.automation", verb = "list")]
async fn ha_automation_list(
    args: HaAutomationListArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<JsonAny> {
    let client = make_client(&args.endpoint)?;
    Ok(client.automation_list().await?.into())
}

/// [MUTATES STATE] Invoke a Home Assistant service (e.g. light.turn_on, switch.toggle). Returns the list of changed entity states.
#[orca_tool(domain = "ha.service", verb = "update")]
async fn ha_service_update(
    args: HaServiceCallArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<JsonAny> {
    let client = make_client(&args.endpoint)?;
    let call = ServiceCall {
        domain: args.domain,
        service: args.service,
        entity_id: args.entity_id,
        data: args.data.unwrap_or_default(),
    };
    Ok(client.service_call(&call).await?.into())
}

// ── Home Assistant endpoints (registered in orca.db) ────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HaEndpointEntry {
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListHomeAssistantEndpointsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListHomeAssistantEndpointsOutput {
    pub endpoints: Vec<HaEndpointEntry>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddHomeAssistantEndpointArgs {
    pub name: String,
    pub base_url: String,
    pub token: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaMutationResult {
    pub name: String,
    pub changed: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveHomeAssistantEndpointArgs {
    pub name: String,
}

/// List all Home Assistant endpoints registered in orca.db (tokens are redacted).
#[orca_tool(domain = "ha.endpoint", verb = "list")]
async fn ha_endpoint_list(
    _args: ListHomeAssistantEndpointsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ListHomeAssistantEndpointsOutput> {
    let conn = orca_db::open_default()?;
    let endpoints = orca_db::home_assistant::list(&conn)?
        .into_iter()
        .map(|r| HaEndpointEntry {
            name: r.name,
            base_url: r.base_url,
            enabled: r.enabled,
        })
        .collect();
    Ok(ListHomeAssistantEndpointsOutput { endpoints })
}

/// [MUTATES STATE] Register or update a Home Assistant endpoint in orca.db. Auth uses a long-lived access token (Bearer header).
#[orca_tool(domain = "ha.endpoint", verb = "create")]
async fn ha_endpoint_create(
    args: AddHomeAssistantEndpointArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<HaMutationResult> {
    let row = orca_db::home_assistant::EndpointRow {
        name: args.name.clone(),
        base_url: args.base_url,
        token: args.token,
        enabled: true,
    };
    let conn = orca_db::open_default()?;
    orca_db::home_assistant::upsert(&conn, &row)?;
    Ok(HaMutationResult {
        name: args.name,
        changed: true,
    })
}

/// [MUTATES STATE] Remove a Home Assistant endpoint from orca.db by name.
#[orca_tool(domain = "ha.endpoint", verb = "delete")]
async fn ha_endpoint_delete(
    args: RemoveHomeAssistantEndpointArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<HaMutationResult> {
    let conn = orca_db::open_default()?;
    let changed = orca_db::home_assistant::remove(&conn, &args.name)?;
    Ok(HaMutationResult {
        name: args.name,
        changed,
    })
}
