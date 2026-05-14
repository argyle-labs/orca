//! Home Assistant tool defs + native impls.
// HaServiceCallArgs.data uses Map<String, Value> — HA service data is free-form by spec.
#![allow(clippy::disallowed_types)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[allow(clippy::disallowed_types)]
use serde_json::{Map, Value};

use crate::orca_tool;

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaEntityListArgs {
    /// Name of a Home Assistant endpoint registered via add_home_assistant_endpoint
    pub endpoint: String,
    /// Optional domain filter (e.g. "light", "sensor", "switch")
    pub domain: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaEntityStateArgs {
    pub endpoint: String,
    /// Entity ID (e.g. "light.living_room")
    pub entity_id: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaAutomationListArgs {
    pub endpoint: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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
    #[cfg_attr(feature = "wasm", tsify(type = "Record<string, unknown> | null"))]
    pub data: Option<Map<String, Value>>,
}

#[cfg(feature = "native")]
fn make_client(name: &str) -> anyhow::Result<orca_integrations::homeassistant::Client> {
    use anyhow::Context;
    let conn = orca_db::open_default()?;
    let row = orca_db::home_assistant::get(&conn, name)?.with_context(|| {
        format!("home assistant endpoint '{name}' not registered (use add_home_assistant_endpoint)")
    })?;
    if !row.enabled {
        anyhow::bail!("home assistant endpoint '{name}' is disabled");
    }
    let cfg = orca_integrations::homeassistant::Config::new(row.base_url, row.token);
    Ok(orca_integrations::homeassistant::Client::new(cfg))
}

/// List Home Assistant entities for a registered endpoint, optionally filtered by domain.
#[orca_tool(domain = "ha", verb = "entity-list")]
async fn home_assistant_entity_list(
    args: HaEntityListArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<crate::JsonAny> {
    let client = make_client(&args.endpoint)?;
    Ok(client.entity_list(args.domain.as_deref()).await?.into())
}

/// Fetch the current state of a single Home Assistant entity.
#[orca_tool(domain = "ha", verb = "entity-state")]
async fn home_assistant_entity_state(
    args: HaEntityStateArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<crate::JsonAny> {
    let client = make_client(&args.endpoint)?;
    Ok(client.entity_state(&args.entity_id).await?.into())
}

/// List Home Assistant automations for a registered endpoint.
#[orca_tool(domain = "ha", verb = "automation-list")]
async fn home_assistant_automation_list(
    args: HaAutomationListArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<crate::JsonAny> {
    let client = make_client(&args.endpoint)?;
    Ok(client.automation_list().await?.into())
}

/// [MUTATES STATE] Invoke a Home Assistant service (e.g. light.turn_on, switch.toggle). Returns the list of changed entity states.
#[orca_tool(domain = "ha", verb = "service-call")]
async fn home_assistant_service_call(
    args: HaServiceCallArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<crate::JsonAny> {
    let client = make_client(&args.endpoint)?;
    let call = orca_integrations::homeassistant::ServiceCall {
        domain: args.domain,
        service: args.service,
        entity_id: args.entity_id,
        data: args.data.unwrap_or_default(),
    };
    Ok(client.service_call(&call).await?.into())
}
