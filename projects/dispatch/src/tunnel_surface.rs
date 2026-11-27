//! Runtime tunnel surface (firewall side) — projects the [`contract::tunnel`]
//! provider registry onto MCP, HTTP (OpenAPI), and CLI as three fixed ops:
//!
//! - `tunnel.list`           → fan `list` across all providers.
//! - `tunnel.apply`          → apply settings via the named provider (admin).
//! - `tunnel.port_alias_set` → set a port alias via the named provider (admin).
//!
//! Both mutations honour `dryRun`: the named provider is resolved and nothing
//! is applied.

// Assembling schemars schemas into MCP/CLI wire JSON is inherently dynamic JSON,
// same allow (and reason) as ups_surface.rs.
#![allow(clippy::disallowed_types)]

use anyhow::Result;
use serde_json::{Value, json};

use contract::tunnel::{self, PortAliasSetArgs, TunnelApply, TunnelQuery};

pub const LIST_TOOL: &str = "tunnel.list";
pub const APPLY_TOOL: &str = "tunnel.apply";
pub const PORT_ALIAS_SET_TOOL: &str = "tunnel.port_alias_set";

fn schema_value<T: schemars::JsonSchema>() -> Value {
    let mut v: Value = serde_json::to_value(schemars::schema_for!(T))
        .unwrap_or_else(|_| json!({ "type": "object" }));
    if let Some(m) = v.as_object_mut() {
        m.remove("$schema");
        m.remove("title");
    }
    v
}

fn mutation_schema<T: schemars::JsonSchema>() -> Value {
    let mut v = schema_value::<T>();
    if let Some(props) = v.get_mut("properties").and_then(Value::as_object_mut) {
        props.insert(
            contract::plan::DRY_RUN_FIELD.into(),
            json!({
                "type": "boolean",
                "default": false,
                "description": "Describe the change without applying it: resolves the owning provider and changes nothing."
            }),
        );
    }
    v
}

/// True iff `name` is one of the fixed tunnel ops.
pub fn tunnel_owns(name: &str) -> bool {
    name == LIST_TOOL || name == APPLY_TOOL || name == PORT_ALIAS_SET_TOOL
}

/// Both mutations rewrite firewall config, so they require `admin`.
pub fn tunnel_role_pairs() -> Vec<(&'static str, &'static str)> {
    vec![(APPLY_TOOL, "admin"), (PORT_ALIAS_SET_TOOL, "admin")]
}

pub fn tunnel_mutation_names() -> Vec<&'static str> {
    vec![APPLY_TOOL, PORT_ALIAS_SET_TOOL]
}

/// The ops that honour `dryRun` in this surface.
pub fn tunnel_dry_run_ok(name: &str) -> bool {
    name == APPLY_TOOL || name == PORT_ALIAS_SET_TOOL
}

pub fn tunnel_mcp_defs() -> Vec<Value> {
    vec![
        json!({
            "name": LIST_TOOL,
            "description": "List firewall-side tunnels (device, address, peers, handshakes) across every provider; providers that failed are listed in `errors`.",
            "inputSchema": schema_value::<TunnelQuery>(),
        }),
        json!({
            "name": APPLY_TOOL,
            "description": "Apply tunnel settings (local address, peer, DNS) to `tunnel_id` on the named provider.",
            "inputSchema": mutation_schema::<TunnelApply>(),
        }),
        json!({
            "name": PORT_ALIAS_SET_TOOL,
            "description": "Set a firewall port alias to an exact list of ports via the named provider.",
            "inputSchema": mutation_schema::<PortAliasSetArgs>(),
        }),
    ]
}

/// Route a `tunnel.*` call. Returns `None` when `name` isn't a tunnel op.
pub async fn tunnel_dispatch(name: &str, args: &Value) -> Option<Result<Value>> {
    match name {
        LIST_TOOL => Some(run_list(args).await),
        APPLY_TOOL => Some(run_apply(args).await),
        PORT_ALIAS_SET_TOOL => Some(run_port_alias_set(args).await),
        _ => None,
    }
}

fn args_without_dry_run(args: &Value) -> Result<(Value, bool)> {
    let dry_run = crate::erased::dry_run_requested(args)?;
    let mut args = args.clone();
    if let Some(obj) = args.as_object_mut() {
        obj.remove(contract::plan::DRY_RUN_FIELD);
    }
    Ok((args, dry_run))
}

fn encode<T: serde::Serialize>(v: &T, what: &str) -> Result<Value> {
    serde_json::to_value(v).map_err(|e| anyhow::anyhow!("encode tunnel {what}: {e}"))
}

async fn run_list(args: &Value) -> Result<Value> {
    let q: TunnelQuery = if args.is_null() {
        TunnelQuery::default()
    } else {
        crate::erased::parse_named_args(LIST_TOOL, args.clone())?
    };
    encode(&tunnel::list(q).await, "list")
}

async fn run_apply(args: &Value) -> Result<Value> {
    let (args, dry_run) = args_without_dry_run(args)?;
    let a: TunnelApply = crate::erased::parse_named_args(APPLY_TOOL, args)?;
    if dry_run {
        let owner = tunnel::provider_named(&a.provider)?;
        return Ok(json!({
            "dryRun": true,
            "changed": false,
            "applied": [],
            "message": format!("dry run: would apply tunnel '{}' on {}", a.tunnel_id, owner.name()),
        }));
    }
    encode(&tunnel::apply(a).await?, "apply outcome")
}

async fn run_port_alias_set(args: &Value) -> Result<Value> {
    let (args, dry_run) = args_without_dry_run(args)?;
    let a: PortAliasSetArgs = crate::erased::parse_named_args(PORT_ALIAS_SET_TOOL, args)?;
    if dry_run {
        let owner = tunnel::provider_named(&a.provider)?;
        return Ok(json!({
            "dryRun": true,
            "changed": false,
            "message": format!("dry run: would set port alias '{}' to {:?} on {}", a.set.alias, a.set.ports, owner.name()),
        }));
    }
    encode(&tunnel::port_alias_set(a).await?, "port alias outcome")
}

/// The top-level `orca tunnel` clap command.
pub fn tunnel_cli_command() -> clap::Command {
    let body = clap::Arg::new("args")
        .long("args")
        .required(true)
        .value_name("JSON");
    let dry_run = clap::Arg::new("dry-run")
        .long("dry-run")
        .action(clap::ArgAction::SetTrue)
        .help("resolve the owning provider and change nothing");
    clap::Command::new("tunnel")
        .about("Inspect and configure firewall-side tunnels (plugin-driven providers)")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            clap::Command::new("list")
                .about("List tunnels across providers")
                .arg(clap::Arg::new("id").long("id").value_name("TUNNEL_ID")),
        )
        .subcommand(
            clap::Command::new("apply")
                .about("Apply tunnel settings (JSON body: {provider, tunnel_id, local_address, peer, dns})")
                .arg(body.clone())
                .arg(dry_run.clone()),
        )
        .subcommand(
            clap::Command::new("port-alias-set")
                .about("Set a port alias (JSON body: {provider, set: {alias, ports}})")
                .arg(body)
                .arg(dry_run),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::BoxFuture;
    use contract::tunnel::{
        PortAliasOutcome, PortAliasSet, TunnelApplyOutcome, TunnelProvider, TunnelState,
        register_provider,
    };
    use std::sync::{Arc, Mutex};

    struct TestTunnel {
        applied: Arc<Mutex<u32>>,
    }
    impl TunnelProvider for TestTunnel {
        fn name(&self) -> &str {
            "tsurf-test"
        }
        fn list(&self, _q: TunnelQuery) -> BoxFuture<'_, Result<Vec<TunnelState>>> {
            Box::pin(async move {
                Ok(vec![TunnelState {
                    provider: "tsurf-test".into(),
                    id: "tsurf-wg".into(),
                    device: "wg1".into(),
                    enabled: true,
                    local_address: "10.0.9.2/32".into(),
                    peers: vec![],
                }])
            })
        }
        fn apply(&self, _a: TunnelApply) -> BoxFuture<'_, Result<TunnelApplyOutcome>> {
            *self.applied.lock().unwrap() += 1;
            Box::pin(async move {
                Ok(TunnelApplyOutcome {
                    changed: true,
                    applied: vec!["peer".into()],
                })
            })
        }
        fn port_alias_set(&self, _s: PortAliasSet) -> BoxFuture<'_, Result<PortAliasOutcome>> {
            *self.applied.lock().unwrap() += 1;
            Box::pin(async move { Ok(PortAliasOutcome { changed: true }) })
        }
    }

    fn apply_body() -> Value {
        json!({
            "provider": "tsurf-test",
            "tunnel_id": "tsurf-wg",
            "local_address": "10.0.9.2/32",
            "peer": { "pubkey": "pk", "endpoint_host": "h", "endpoint_port": 1337 },
        })
    }

    #[tokio::test]
    async fn bad_args_are_invalid_naming_the_field() {
        let err = tunnel_dispatch(APPLY_TOOL, &json!({ "tunnel_id": 7 }))
            .await
            .unwrap()
            .unwrap_err();
        crate::erased::assert_invalid(&err, "tunnel_id");
    }

    #[test]
    fn owns_only_the_three_ops() {
        assert!(tunnel_owns(LIST_TOOL));
        assert!(tunnel_owns(APPLY_TOOL));
        assert!(tunnel_owns(PORT_ALIAS_SET_TOOL));
        assert!(!tunnel_owns("tunnel.nope"));
    }

    #[test]
    fn mutations_advertise_dry_run() {
        let defs = tunnel_mcp_defs();
        for d in &defs[1..] {
            assert_eq!(d["inputSchema"]["properties"]["dryRun"]["type"], "boolean");
        }
    }

    #[tokio::test]
    async fn list_fans_out_and_mutations_route_by_owner() {
        let applied = Arc::new(Mutex::new(0));
        register_provider(Arc::new(TestTunnel {
            applied: applied.clone(),
        }));
        let out = tunnel_dispatch(LIST_TOOL, &json!({}))
            .await
            .expect("is a tunnel op")
            .expect("ok");
        assert!(
            out["tunnels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["id"] == "tsurf-wg" && t["provider"] == "tsurf-test")
        );
        assert!(out["errors"].is_array());

        let mut dry = apply_body();
        dry["dryRun"] = json!(true);
        let r = tunnel_dispatch(APPLY_TOOL, &dry)
            .await
            .unwrap()
            .expect("dry");
        assert_eq!(r["dryRun"], true);
        assert_eq!(r["changed"], false);
        let r = tunnel_dispatch(
            PORT_ALIAS_SET_TOOL,
            &json!({ "provider": "tsurf-test", "set": { "alias": "fwd", "ports": [1] }, "dryRun": true }),
        )
        .await
        .unwrap()
        .expect("dry");
        assert_eq!(r["dryRun"], true);
        assert_eq!(*applied.lock().unwrap(), 0);

        let r = tunnel_dispatch(APPLY_TOOL, &apply_body())
            .await
            .unwrap()
            .expect("ok");
        assert_eq!(r["changed"], true);
        let r = tunnel_dispatch(
            PORT_ALIAS_SET_TOOL,
            &json!({ "provider": "tsurf-test", "set": { "alias": "fwd", "ports": [1] } }),
        )
        .await
        .unwrap()
        .expect("ok");
        assert_eq!(r["changed"], true);
        assert_eq!(*applied.lock().unwrap(), 2);
        assert!(contract::tunnel::deregister_provider("tsurf-test"));
    }

    #[tokio::test]
    async fn apply_to_unknown_provider_errors() {
        let mut body = apply_body();
        body["provider"] = json!("tsurf-nope");
        let err = tunnel_dispatch(APPLY_TOOL, &body)
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("no tunnel provider named 'tsurf-nope'")
        );
    }

    #[tokio::test]
    async fn unknown_name_returns_none() {
        assert!(tunnel_dispatch("tunnel.nope", &json!({})).await.is_none());
    }
}
