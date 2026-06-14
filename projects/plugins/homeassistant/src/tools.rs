//! Home Assistant tool surface.
//!
//! Endpoint registry: `home-assistant.{list, detail, create, update, delete}`
//! — generated wholesale by `#[endpoint_resource]`. Hand-written tools:
//!   - `home-assistant.entities`   list entities (optionally domain-filtered)
//!   - `home-assistant.entity`     single entity state
//!   - `home-assistant.automations`
//!   - `home-assistant.service`    invoke an HA service
//!
//! Imports flow through `plugin_toolkit::prelude::*` only.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;
use serde_json as sj;

use crate::{Client, Config, ServiceCall};

// ═══════════════════════════════════════════════════════════════════════════
// home-assistant.{list,detail,create,update,delete} — endpoint registry CRUD.
// ═══════════════════════════════════════════════════════════════════════════

#[endpoint_resource(plugin = "home-assistant", table = "homeassistant_endpoints")]
pub struct HaEndpoint {
    pub name: String,
    pub base_url: String,
    #[secret]
    pub token: String,
    pub enabled: bool,
}

// ── HTTP client helper ─────────────────────────────────────────────────────

fn make_client(name: &str) -> Result<Client> {
    let conn = runtime::open_db()?;
    let row = endpoint_db::get(&conn, name)?
        .with_context(|| format!("home assistant endpoint '{name}' not registered"))?;
    if !row.enabled {
        bail!("home assistant endpoint '{name}' is disabled");
    }
    Ok(Client::new(Config::new(row.base_url, row.token)))
}

// ═══════════════════════════════════════════════════════════════════════════
// home-assistant.entities — list entities (optionally domain-filtered)
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct HaEntitiesArgs {
    #[arg(long)]
    pub endpoint: String,
    /// Optional HA domain filter (light, sensor, switch, …).
    #[arg(long)]
    pub domain: Option<String>,
}

#[orca_tool(domain = "home-assistant", verb = "entities")]
async fn ha_entities(args: HaEntitiesArgs, _ctx: &ToolCtx) -> Result<JsonAny> {
    let client = make_client(&args.endpoint)?;
    Ok(client.entity_list(args.domain.as_deref()).await?.into())
}

// ═══════════════════════════════════════════════════════════════════════════
// home-assistant.entity — single entity state
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct HaEntityArgs {
    #[arg(long)]
    pub endpoint: String,
    /// Entity ID (e.g. "light.living_room").
    #[arg(long)]
    pub entity_id: String,
}

#[orca_tool(domain = "home-assistant", verb = "entity")]
async fn ha_entity(args: HaEntityArgs, _ctx: &ToolCtx) -> Result<JsonAny> {
    let client = make_client(&args.endpoint)?;
    Ok(client.entity_state(&args.entity_id).await?.into())
}

// ═══════════════════════════════════════════════════════════════════════════
// home-assistant.automations — list automations
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct HaAutomationsArgs {
    #[arg(long)]
    pub endpoint: String,
}

#[orca_tool(domain = "home-assistant", verb = "automations")]
async fn ha_automations(args: HaAutomationsArgs, _ctx: &ToolCtx) -> Result<JsonAny> {
    let client = make_client(&args.endpoint)?;
    Ok(client.automation_list().await?.into())
}

// ═══════════════════════════════════════════════════════════════════════════
// home-assistant.service — invoke an HA service
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct HaServiceArgs {
    #[arg(long)]
    pub endpoint: String,
    /// HA service domain (light, switch, automation, …).
    #[arg(long)]
    pub service_domain: String,
    /// HA service name (turn_on, toggle, …).
    #[arg(long)]
    pub service_name: String,
    #[arg(long)]
    pub entity_id: Option<String>,
    /// Opaque free-form service-data — upstream-defined.
    #[arg(skip)]
    pub service_data: Option<sj::Map<String, sj::Value>>,
}

/// [MUTATES STATE] Invoke a Home Assistant service.
#[orca_tool(domain = "home-assistant", verb = "service", role = "admin")]
async fn ha_service(args: HaServiceArgs, _ctx: &ToolCtx) -> Result<JsonAny> {
    let client = make_client(&args.endpoint)?;
    let call = ServiceCall {
        domain: args.service_domain,
        service: args.service_name,
        entity_id: args.entity_id,
        data: args.service_data.unwrap_or_default(),
    };
    Ok(client.service_call(&call).await?.into())
}
