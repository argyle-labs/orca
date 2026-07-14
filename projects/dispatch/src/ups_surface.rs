//! Runtime UPS surface — projects the [`contract::ups`] provider registry onto
//! MCP, HTTP (OpenAPI), and CLI as three **fixed** ops:
//!
//! - `ups.state`     → fan `state` across all providers, return typed [`UpsState`]s.
//! - `ups.config`    → fan `config_get` across all providers, return [`UpsConfig`]s.
//! - `ups.configure` → apply one [`UpsConfig`] via the owning provider (admin).
//!
//! Like [`crate::diagnostics_surface`], the ops are static — their input/output
//! schemas never change; only the reported UPSes vary with the loaded providers
//! (nut, unraid, …). Two providers, one surface.

// Assembling schemars schemas into MCP/CLI wire JSON is inherently dynamic JSON,
// same allow (and reason) as diagnostics_surface.rs / unit_surface.rs.
#![allow(clippy::disallowed_types)]

use anyhow::Result;
use serde_json::{Value, json};

use contract::ups::{self, UpsConfigSetArgs, UpsQueryArgs};

/// Canonical MCP/REST tool names. The CLI exposes these as `orca ups state` /
/// `orca ups config` / `orca ups configure`.
pub const STATE_TOOL: &str = "ups.state";
pub const CONFIG_TOOL: &str = "ups.config";
pub const CONFIGURE_TOOL: &str = "ups.configure";

fn schema_value<T: schemars::JsonSchema>() -> Value {
    let mut v: Value = serde_json::to_value(schemars::schema_for!(T))
        .unwrap_or_else(|_| json!({ "type": "object" }));
    if let Some(m) = v.as_object_mut() {
        m.remove("$schema");
        m.remove("title");
    }
    v
}

/// True iff `name` is one of the fixed UPS ops.
pub fn ups_owns(name: &str) -> bool {
    name == STATE_TOOL || name == CONFIG_TOOL || name == CONFIGURE_TOOL
}

/// Static `(tool, required_role)` gating. `ups.configure` mutates a managed
/// system (thresholds, kill-power) so it requires `admin`; the reads stay `any`.
pub fn ups_role_pairs() -> Vec<(&'static str, &'static str)> {
    vec![(CONFIGURE_TOOL, "admin")]
}

/// The UPS ops that are data mutations (write against a managed system). Only
/// `ups.configure` mutates.
pub fn ups_mutation_names() -> Vec<&'static str> {
    vec![CONFIGURE_TOOL]
}

/// The three UPS ops as MCP `tools/list` entries, merged by
/// [`crate::registry::mcp_definitions`].
pub fn ups_mcp_defs() -> Vec<Value> {
    vec![
        json!({
            "name": STATE_TOOL,
            "description": "Read live UPS state (battery charge/runtime, load, on-battery) across every provider.",
            "inputSchema": schema_value::<UpsQueryArgs>(),
        }),
        json!({
            "name": CONFIG_TOOL,
            "description": "Read UPS power/shutdown config (thresholds, kill-power) across every provider.",
            "inputSchema": schema_value::<UpsQueryArgs>(),
        }),
        json!({
            "name": CONFIGURE_TOOL,
            "description": "Apply a UPS config (thresholds, kill-power) to one UPS via its provider.",
            "inputSchema": schema_value::<UpsConfigSetArgs>(),
        }),
    ]
}

/// Route a `ups.*` call back through [`contract::ups`]. Returns `None` when
/// `name` isn't a UPS op (so the caller falls through).
pub async fn ups_dispatch(name: &str, args: &Value) -> Option<Result<Value>> {
    match name {
        STATE_TOOL => Some(run_query(args, true).await),
        CONFIG_TOOL => Some(run_query(args, false).await),
        CONFIGURE_TOOL => Some(run_configure(args).await),
        _ => None,
    }
}

async fn run_query(args: &Value, live: bool) -> Result<Value> {
    let a: UpsQueryArgs = if args.is_null() {
        UpsQueryArgs::default()
    } else {
        serde_json::from_value(args.clone())
            .map_err(|e| anyhow::anyhow!("invalid ups query args: {e}"))?
    };
    if live {
        let states = ups::state(a).await;
        serde_json::to_value(&states).map_err(|e| anyhow::anyhow!("encode ups state: {e}"))
    } else {
        let cfgs = ups::config_get(a).await;
        serde_json::to_value(&cfgs).map_err(|e| anyhow::anyhow!("encode ups config: {e}"))
    }
}

async fn run_configure(args: &Value) -> Result<Value> {
    let a: UpsConfigSetArgs = serde_json::from_value(args.clone())
        .map_err(|e| anyhow::anyhow!("invalid ups configure args: {e}"))?;
    let outcome = ups::config_set(a).await?;
    serde_json::to_value(&outcome).map_err(|e| anyhow::anyhow!("encode ups outcome: {e}"))
}

/// The top-level `orca ups` clap command (static — three subcommands).
pub fn ups_cli_command() -> clap::Command {
    let provider_arg = clap::Arg::new("provider")
        .long("provider")
        .value_name("NAME")
        .help("restrict to one provider (e.g. nut, unraid)");
    clap::Command::new("ups")
        .about("Inspect and configure UPS power (plugin-driven providers)")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            clap::Command::new("state")
                .about("Read live UPS state across providers")
                .arg(provider_arg.clone())
                .arg(clap::Arg::new("id").long("id").value_name("UPS_ID")),
        )
        .subcommand(
            clap::Command::new("config")
                .about("Read UPS power/shutdown config across providers")
                .arg(provider_arg.clone())
                .arg(clap::Arg::new("id").long("id").value_name("UPS_ID")),
        )
        .subcommand(
            clap::Command::new("configure")
                .about("Apply a UPS config to one UPS (JSON body: {provider, config})")
                .arg(
                    clap::Arg::new("provider")
                        .long("provider")
                        .required(true)
                        .value_name("NAME"),
                )
                .arg(
                    clap::Arg::new("config")
                        .long("config")
                        .required(true)
                        .value_name("JSON")
                        .help("UpsConfig as JSON"),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::BoxFuture;
    use contract::ups::{UpsConfig, UpsConfigOutcome, UpsProvider, UpsState, register_provider};
    use std::sync::Arc;

    struct TestUps;
    impl UpsProvider for TestUps {
        fn name(&self) -> &str {
            "usurf-test"
        }
        fn state(&self, _a: UpsQueryArgs) -> BoxFuture<'_, Result<Vec<UpsState>>> {
            Box::pin(async move {
                Ok(vec![UpsState {
                    provider: "usurf-test".into(),
                    id: "default".into(),
                    model: None,
                    battery_charge: Some(80.0),
                    battery_runtime_ms: Some(900_000),
                    input_voltage: None,
                    load_percent: None,
                    status: "OL".into(),
                    on_battery: false,
                    low_battery: false,
                }])
            })
        }
        fn config_get(&self, _a: UpsQueryArgs) -> BoxFuture<'_, Result<Vec<UpsConfig>>> {
            Box::pin(async move { Ok(vec![UpsConfig::default()]) })
        }
        fn config_set(&self, config: UpsConfig) -> BoxFuture<'_, Result<UpsConfigOutcome>> {
            Box::pin(async move {
                Ok(UpsConfigOutcome {
                    id: config.id,
                    provider: "usurf-test".into(),
                    ok: true,
                    message: "ok".into(),
                    restart_required: false,
                })
            })
        }
    }

    #[test]
    fn owns_only_the_three_ops() {
        assert!(ups_owns(STATE_TOOL));
        assert!(ups_owns(CONFIG_TOOL));
        assert!(ups_owns(CONFIGURE_TOOL));
        assert!(!ups_owns("ups.nope"));
    }

    #[tokio::test]
    async fn state_fans_out_and_configure_routes() {
        register_provider(Arc::new(TestUps));
        let out = ups_dispatch(STATE_TOOL, &json!({ "provider": "usurf-test" }))
            .await
            .expect("is a ups op")
            .expect("ok");
        assert!(out.as_array().unwrap().iter().any(|u| u["id"] == "default"));

        let r = ups_dispatch(
            CONFIGURE_TOOL,
            &json!({ "provider": "usurf-test", "config": { "id": "default", "kill_power": true } }),
        )
        .await
        .expect("is a ups op")
        .expect("ok");
        assert_eq!(r["ok"], true);
        assert!(contract::ups::deregister_provider("usurf-test"));
    }

    #[tokio::test]
    async fn unknown_name_returns_none() {
        assert!(ups_dispatch("ups.nope", &json!({})).await.is_none());
    }
}
