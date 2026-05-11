//! Home Assistant tool defs + native impls.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[allow(clippy::disallowed_types)]
use serde_json::{Map, Value};

use crate::OrcaToolDef;

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaEntityListArgs {
    /// Name of a Home Assistant endpoint registered via add_home_assistant_endpoint
    pub endpoint: String,
    /// Optional domain filter (e.g. "light", "sensor", "switch")
    pub domain: Option<String>,
}
pub struct HaEntityList;
#[allow(clippy::disallowed_types)] // Output is opaque HA entity dump — shape varies per entity domain
impl OrcaToolDef for HaEntityList {
    const NAME: &'static str = "home_assistant_entity_list";
    const DESCRIPTION: &'static str =
        "List Home Assistant entities for a registered endpoint, optionally filtered by domain.";
    type Args = HaEntityListArgs;
    type Output = crate::JsonAny;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaEntityStateArgs {
    pub endpoint: String,
    /// Entity ID (e.g. "light.living_room")
    pub entity_id: String,
}
pub struct HaEntityState;
#[allow(clippy::disallowed_types)] // Output is opaque HA entity state blob — shape varies by entity
impl OrcaToolDef for HaEntityState {
    const NAME: &'static str = "home_assistant_entity_state";
    const DESCRIPTION: &'static str = "Fetch the current state of a single Home Assistant entity.";
    type Args = HaEntityStateArgs;
    type Output = crate::JsonAny;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaAutomationListArgs {
    pub endpoint: String,
}
pub struct HaAutomationList;
#[allow(clippy::disallowed_types)] // Output is opaque HA automation list — shape dictated by HA upstream
impl OrcaToolDef for HaAutomationList {
    const NAME: &'static str = "home_assistant_automation_list";
    const DESCRIPTION: &'static str = "List Home Assistant automations for a registered endpoint.";
    type Args = HaAutomationListArgs;
    type Output = crate::JsonAny;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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
pub struct HaServiceCall;
#[allow(clippy::disallowed_types)] // Output is opaque HA changed-states list — shape varies per service
impl OrcaToolDef for HaServiceCall {
    const NAME: &'static str = "home_assistant_service_call";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Invoke a Home Assistant service \
         (e.g. light.turn_on, switch.toggle). Returns the list of changed entity states.";
    type Args = HaServiceCallArgs;
    type Output = crate::JsonAny;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use anyhow::{Context, Result};
    use async_trait::async_trait;
    use orca_db as db;
    use orca_integrations::homeassistant::{Client, Config, ServiceCall};
    use orca_utils::tool::{OrcaTool, ToolCtx};

    fn make_client(name: &str) -> Result<Client> {
        let conn = db::open_default()?;
        let row = db::home_assistant::get(&conn, name)?.with_context(|| {
            format!(
                "home assistant endpoint '{name}' not registered (use add_home_assistant_endpoint)"
            )
        })?;
        if !row.enabled {
            anyhow::bail!("home assistant endpoint '{name}' is disabled");
        }
        let cfg = Config::new(row.base_url, row.token);
        Ok(Client::new(cfg))
    }

    #[async_trait]
    impl OrcaTool for HaEntityList {
        async fn run(args: HaEntityListArgs, _: &ToolCtx) -> Result<crate::JsonAny> {
            let client = make_client(&args.endpoint)?;
            Ok(client.entity_list(args.domain.as_deref()).await?.into())
        }
    }

    #[async_trait]
    impl OrcaTool for HaEntityState {
        async fn run(args: HaEntityStateArgs, _: &ToolCtx) -> Result<crate::JsonAny> {
            let client = make_client(&args.endpoint)?;
            Ok(client.entity_state(&args.entity_id).await?.into())
        }
    }

    #[async_trait]
    impl OrcaTool for HaAutomationList {
        async fn run(args: HaAutomationListArgs, _: &ToolCtx) -> Result<crate::JsonAny> {
            let client = make_client(&args.endpoint)?;
            Ok(client.automation_list().await?.into())
        }
    }

    #[async_trait]
    impl OrcaTool for HaServiceCall {
        async fn run(args: HaServiceCallArgs, _: &ToolCtx) -> Result<crate::JsonAny> {
            let client = make_client(&args.endpoint)?;
            let call = ServiceCall {
                domain: args.domain,
                service: args.service,
                entity_id: args.entity_id,
                data: args.data.unwrap_or_default(),
            };
            Ok(client.service_call(&call).await?.into())
        }
    }
}
