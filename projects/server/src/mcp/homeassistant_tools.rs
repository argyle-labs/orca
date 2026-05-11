//! MCP data tools for Home Assistant endpoints registered in orca.db.
//!
//! Mgmt CRUD lives in `mgmt_tools.rs`. This module exposes Home Assistant
//! REST operations: list entities, fetch entity state, list automations,
//! and invoke services.

use anyhow::{Context, Result};
use async_trait::async_trait;
use orca_integrations::homeassistant::{Client, Config, ServiceCall};
use orca_utils::tool::{OrcaTool, ToolCtx};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

fn make_client(name: &str) -> Result<Client> {
    let conn = db::open_default()?;
    let row = db::home_assistant::get(&conn, name)?.with_context(|| {
        format!("home assistant endpoint '{name}' not registered (use add_home_assistant_endpoint)")
    })?;
    if !row.enabled {
        anyhow::bail!("home assistant endpoint '{name}' is disabled");
    }
    let cfg = Config::new(row.base_url, row.token);
    Ok(Client::new(cfg))
}

// ── entity_list ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct HaEntityListArgs {
    /// Name of a Home Assistant endpoint registered via add_home_assistant_endpoint
    pub endpoint: String,
    /// Optional domain filter (e.g. "light", "sensor", "switch")
    pub domain: Option<String>,
}
pub struct HaEntityList;
#[async_trait]
impl OrcaTool for HaEntityList {
    const NAME: &'static str = "home_assistant_entity_list";
    const DESCRIPTION: &'static str =
        "List Home Assistant entities for a registered endpoint, optionally filtered by domain.";
    type Args = HaEntityListArgs;
    async fn run(args: HaEntityListArgs, _: &ToolCtx) -> Result<String> {
        let client = make_client(&args.endpoint)?;
        let v = client.entity_list(args.domain.as_deref()).await?;
        Ok(serde_json::to_string_pretty(&v)?)
    }
}

// ── entity_state ───────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct HaEntityStateArgs {
    pub endpoint: String,
    /// Entity ID (e.g. "light.living_room")
    pub entity_id: String,
}
pub struct HaEntityState;
#[async_trait]
impl OrcaTool for HaEntityState {
    const NAME: &'static str = "home_assistant_entity_state";
    const DESCRIPTION: &'static str = "Fetch the current state of a single Home Assistant entity.";
    type Args = HaEntityStateArgs;
    async fn run(args: HaEntityStateArgs, _: &ToolCtx) -> Result<String> {
        let client = make_client(&args.endpoint)?;
        let v = client.entity_state(&args.entity_id).await?;
        Ok(serde_json::to_string_pretty(&v)?)
    }
}

// ── automation_list ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct HaAutomationListArgs {
    pub endpoint: String,
}
pub struct HaAutomationList;
#[async_trait]
impl OrcaTool for HaAutomationList {
    const NAME: &'static str = "home_assistant_automation_list";
    const DESCRIPTION: &'static str = "List Home Assistant automations for a registered endpoint.";
    type Args = HaAutomationListArgs;
    async fn run(args: HaAutomationListArgs, _: &ToolCtx) -> Result<String> {
        let client = make_client(&args.endpoint)?;
        let v = client.automation_list().await?;
        Ok(serde_json::to_string_pretty(&v)?)
    }
}

// ── service_call ───────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct HaServiceCallArgs {
    pub endpoint: String,
    /// Service domain (e.g. "light", "switch", "automation")
    pub domain: String,
    /// Service name (e.g. "turn_on", "toggle")
    pub service: String,
    /// Optional entity_id target (e.g. "light.living_room")
    pub entity_id: Option<String>,
    /// Optional service data payload merged into the request body
    pub data: Option<Map<String, Value>>,
}
pub struct HaServiceCall;
#[async_trait]
impl OrcaTool for HaServiceCall {
    const NAME: &'static str = "home_assistant_service_call";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Invoke a Home Assistant service \
         (e.g. light.turn_on, switch.toggle). Returns the list of changed entity states.";
    type Args = HaServiceCallArgs;
    async fn run(args: HaServiceCallArgs, _: &ToolCtx) -> Result<String> {
        let client = make_client(&args.endpoint)?;
        let call = ServiceCall {
            domain: args.domain,
            service: args.service,
            entity_id: args.entity_id,
            data: args.data.unwrap_or_default(),
        };
        let v = client.service_call(&call).await?;
        Ok(serde_json::to_string_pretty(&v)?)
    }
}

// ── register ───────────────────────────────────────────────────────────────────

pub fn register(reg: &mut orca_utils::tool::ToolRegistry) {
    reg.register::<HaEntityList>()
        .register::<HaEntityState>()
        .register::<HaAutomationList>()
        .register::<HaServiceCall>();
}
