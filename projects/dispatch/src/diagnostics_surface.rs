//! Runtime diagnostics surface — projects the [`contract::diagnostics`] provider
//! registry onto MCP, HTTP (OpenAPI), and CLI as two **fixed** ops:
//!
//! - `diagnostics.diagnose` → fan `diagnose` across all providers, return typed
//!   [`contract::diagnostics::Finding`]s.
//! - `diagnostics.repair`   → run one repair by `{provider, repair_id}`.
//!
//! Unlike [`crate::unit_surface`], the ops here are static — their input/output
//! schemas never change. Only the *findings* vary with the loaded providers
//! (raccoon today; bazzite/cachyos later), so there's no per-provider tool
//! generation: two tools, projected once.

// Assembling schemars schemas into MCP/CLI wire JSON is inherently dynamic JSON,
// same allow (and reason) as registry.rs / unit_surface.rs.
#![allow(clippy::disallowed_types)]

use anyhow::Result;
use serde_json::{Value, json};

use contract::diagnostics::{self, DiagnoseArgs, RepairArgs};

/// Canonical MCP/REST tool names. The CLI exposes these as `orca diagnostics
/// diagnose` / `orca diagnostics repair`.
pub const DIAGNOSE_TOOL: &str = "diagnostics.diagnose";
pub const REPAIR_TOOL: &str = "diagnostics.repair";

fn schema_value<T: schemars::JsonSchema>() -> Value {
    let mut v: Value = serde_json::to_value(schemars::schema_for!(T))
        .unwrap_or_else(|_| json!({ "type": "object" }));
    if let Some(m) = v.as_object_mut() {
        m.remove("$schema");
        m.remove("title");
    }
    v
}

/// True iff `name` is one of the fixed diagnostics ops.
pub fn diagnostics_owns(name: &str) -> bool {
    name == DIAGNOSE_TOOL || name == REPAIR_TOOL
}

/// The two diagnostics ops as MCP `tools/list` entries, merged by
/// [`crate::registry::mcp_definitions`].
pub fn diagnostics_mcp_defs() -> Vec<Value> {
    vec![
        json!({
            "name": DIAGNOSE_TOOL,
            "description": "Run every diagnostics provider and return typed findings (health + how to repair).",
            "inputSchema": schema_value::<DiagnoseArgs>(),
        }),
        json!({
            "name": REPAIR_TOOL,
            "description": "Run one repair by {provider, repair_id} (taken from a finding's repair spec).",
            "inputSchema": schema_value::<RepairArgs>(),
        }),
    ]
}

/// Route a `diagnostics.*` call back through [`contract::diagnostics`]. Returns
/// `None` when `name` isn't a diagnostics op (so the caller falls through).
pub async fn diagnostics_dispatch(name: &str, args: &Value) -> Option<Result<Value>> {
    match name {
        DIAGNOSE_TOOL => Some(run_diagnose(args).await),
        REPAIR_TOOL => Some(run_repair(args).await),
        _ => None,
    }
}

async fn run_diagnose(args: &Value) -> Result<Value> {
    let a: DiagnoseArgs = if args.is_null() {
        DiagnoseArgs::default()
    } else {
        serde_json::from_value(args.clone())
            .map_err(|e| anyhow::anyhow!("invalid diagnose args: {e}"))?
    };
    let findings = diagnostics::diagnose(a).await;
    serde_json::to_value(&findings).map_err(|e| anyhow::anyhow!("encode findings: {e}"))
}

async fn run_repair(args: &Value) -> Result<Value> {
    let a: RepairArgs = serde_json::from_value(args.clone())
        .map_err(|e| anyhow::anyhow!("invalid repair args: {e}"))?;
    let outcome = diagnostics::repair(a).await?;
    serde_json::to_value(&outcome).map_err(|e| anyhow::anyhow!("encode outcome: {e}"))
}

/// The top-level `orca diagnostics` clap command (static — two subcommands).
pub fn diagnostics_cli_command() -> clap::Command {
    let json_arg = |name: &'static str| {
        clap::Arg::new(name)
            .long(name)
            .value_name("VALUE")
            .help("value (parsed as JSON, else string)")
    };
    clap::Command::new("diagnostics")
        .about("Diagnose subsystems and run repairs (plugin-driven findings)")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            clap::Command::new("diagnose")
                .about("Run all providers and print typed findings")
                .arg(json_arg("provider").help("restrict to one provider (e.g. raccoon)")),
        )
        .subcommand(
            clap::Command::new("repair")
                .about("Run one repair by provider + repair_id")
                .arg(
                    clap::Arg::new("provider")
                        .long("provider")
                        .required(true)
                        .value_name("NAME"),
                )
                .arg(
                    clap::Arg::new("repair_id")
                        .long("repair-id")
                        .required(true)
                        .value_name("ID"),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::BoxFuture;
    use contract::diagnostics::{
        DiagnosticsProvider, Finding, RepairArgs, RepairOutcome, Severity, register_provider,
    };
    use std::sync::Arc;

    struct TestProvider {
        name: String,
    }

    impl DiagnosticsProvider for TestProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn diagnose(&self, _args: DiagnoseArgs) -> BoxFuture<'_, Result<Vec<Finding>>> {
            let name = self.name.clone();
            Box::pin(async move {
                Ok(vec![Finding {
                    id: "check-a".into(),
                    provider: name,
                    severity: Severity::Warn,
                    title: "A drifted".into(),
                    detail: "detail".into(),
                    repair: None,
                }])
            })
        }
        fn repair(&self, args: RepairArgs) -> BoxFuture<'_, Result<RepairOutcome>> {
            let name = self.name.clone();
            Box::pin(async move {
                Ok(RepairOutcome {
                    id: args.repair_id,
                    provider: name,
                    ok: true,
                    message: "fixed".into(),
                })
            })
        }
    }

    #[test]
    fn owns_only_the_two_ops() {
        assert!(diagnostics_owns(DIAGNOSE_TOOL));
        assert!(diagnostics_owns(REPAIR_TOOL));
        assert!(!diagnostics_owns("diagnostics.nope"));
    }

    #[test]
    fn mcp_defs_carry_typed_schemas() {
        let defs = diagnostics_mcp_defs();
        assert_eq!(defs.len(), 2);
        let diag = defs.iter().find(|d| d["name"] == DIAGNOSE_TOOL).unwrap();
        assert_eq!(diag["inputSchema"]["type"], "object");
    }

    #[tokio::test]
    async fn diagnose_fans_out_and_repair_routes() {
        register_provider(Arc::new(TestProvider {
            name: "dsurf-test".into(),
        }));
        let out = diagnostics_dispatch(DIAGNOSE_TOOL, &json!({ "provider": "dsurf-test" }))
            .await
            .expect("is a diagnostics op")
            .expect("ok");
        assert!(out.as_array().unwrap().iter().any(|f| f["id"] == "check-a"));

        let r = diagnostics_dispatch(
            REPAIR_TOOL,
            &json!({ "provider": "dsurf-test", "repair_id": "check-a" }),
        )
        .await
        .expect("is a diagnostics op")
        .expect("ok");
        assert_eq!(r["ok"], true);
        assert!(contract::diagnostics::deregister_provider("dsurf-test"));
    }

    #[tokio::test]
    async fn unknown_name_returns_none() {
        assert!(
            diagnostics_dispatch("diagnostics.nope", &json!({}))
                .await
                .is_none()
        );
    }
}
