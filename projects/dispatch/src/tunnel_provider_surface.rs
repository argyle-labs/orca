//! Runtime tunnel-provider surface (VPN side) — projects the
//! [`contract::tunnel_provider`] registry onto MCP, HTTP (OpenAPI), and CLI as
//! two fixed ops, each routed to the named provider:
//!
//! - `tunnel_provider.session`      → open a session for a client pubkey.
//! - `tunnel_provider.port_forward` → request a forwarded port.
//!
//! Both write to the provider's external account and honour `dryRun`: the
//! named provider is resolved and not called.

// Assembling schemars schemas into MCP/CLI wire JSON is inherently dynamic JSON,
// same allow (and reason) as ups_surface.rs.
#![allow(clippy::disallowed_types)]

use anyhow::Result;
use serde_json::{Value, json};

use contract::tunnel_provider::{self, PortForwardArgs, SessionArgs};

pub const SESSION_TOOL: &str = "tunnel_provider.session";
pub const PORT_FORWARD_TOOL: &str = "tunnel_provider.port_forward";

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
                "description": "Describe the request without making it: resolves the named provider and does not call it."
            }),
        );
    }
    v
}

pub fn tunnel_provider_owns(name: &str) -> bool {
    name == SESSION_TOOL || name == PORT_FORWARD_TOOL
}

/// Both ops spend the provider's stored account credentials, so they require
/// `admin`.
pub fn tunnel_provider_role_pairs() -> Vec<(&'static str, &'static str)> {
    vec![(SESSION_TOOL, "admin"), (PORT_FORWARD_TOOL, "admin")]
}

pub fn tunnel_provider_mutation_names() -> Vec<&'static str> {
    vec![SESSION_TOOL, PORT_FORWARD_TOOL]
}

/// The ops that honour `dryRun` in this surface.
pub fn tunnel_provider_dry_run_ok(name: &str) -> bool {
    tunnel_provider_owns(name)
}

pub fn tunnel_provider_mcp_defs() -> Vec<Value> {
    vec![
        json!({
            "name": SESSION_TOOL,
            "description": "Open a VPN tunnel session for a client public key via the named provider; returns endpoint, server key and addresses.",
            "inputSchema": mutation_schema::<SessionArgs>(),
        }),
        json!({
            "name": PORT_FORWARD_TOOL,
            "description": "Request a forwarded port on a session's server via the named provider.",
            "inputSchema": mutation_schema::<PortForwardArgs>(),
        }),
    ]
}

/// Route a `tunnel_provider.*` call. Returns `None` when `name` isn't one.
pub async fn tunnel_provider_dispatch(name: &str, args: &Value) -> Option<Result<Value>> {
    match name {
        SESSION_TOOL => Some(run_session(args).await),
        PORT_FORWARD_TOOL => Some(run_port_forward(args).await),
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

async fn run_session(args: &Value) -> Result<Value> {
    let (args, dry_run) = args_without_dry_run(args)?;
    let a: SessionArgs = crate::erased::parse_named_args(SESSION_TOOL, args)?;
    if dry_run {
        let owner = tunnel_provider::provider_named(&a.provider)?;
        return Ok(json!({
            "dryRun": true,
            "changed": null,
            "would_request": { "provider": owner.name(), "session": a.session },
            "message": format!("dry run: would open a session for client key '{}' on {}", a.session.client_pubkey, owner.name()),
        }));
    }
    let s = tunnel_provider::session(a).await?;
    serde_json::to_value(&s).map_err(|e| anyhow::anyhow!("encode tunnel session: {e}"))
}

async fn run_port_forward(args: &Value) -> Result<Value> {
    let (args, dry_run) = args_without_dry_run(args)?;
    let a: PortForwardArgs = crate::erased::parse_named_args(PORT_FORWARD_TOOL, args)?;
    if dry_run {
        let owner = tunnel_provider::provider_named(&a.provider)?;
        return Ok(json!({
            "dryRun": true,
            "changed": null,
            "would_request": { "provider": owner.name(), "port_forward": a.port_forward },
            "message": format!("dry run: would request a forwarded port on {}", owner.name()),
        }));
    }
    let pf = tunnel_provider::port_forward(a).await?;
    serde_json::to_value(&pf).map_err(|e| anyhow::anyhow!("encode port forward: {e}"))
}

/// The top-level `orca tunnel_provider` clap command.
pub fn tunnel_provider_cli_command() -> clap::Command {
    let body = clap::Arg::new("args")
        .long("args")
        .required(true)
        .value_name("JSON");
    let dry_run = clap::Arg::new("dry-run")
        .long("dry-run")
        .action(clap::ArgAction::SetTrue)
        .help("resolve the named provider and do not call it");
    clap::Command::new("tunnel_provider")
        .about("Issue VPN tunnel settings (plugin-driven providers)")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            clap::Command::new("session")
                .about("Open a session (JSON body: {provider, session})")
                .arg(body.clone())
                .arg(dry_run.clone()),
        )
        .subcommand(
            clap::Command::new("port-forward")
                .about("Request a forwarded port (JSON body: {provider, port_forward})")
                .arg(body)
                .arg(dry_run),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::BoxFuture;
    use contract::tunnel_provider::{
        PortForward, PortForwardReq, Session, SessionReq, TunnelSessionProvider, register_provider,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    static CALLS: AtomicU32 = AtomicU32::new(0);

    struct TestVpn;
    impl TunnelSessionProvider for TestVpn {
        fn name(&self) -> &str {
            "tpsurf-test"
        }
        fn session(&self, req: SessionReq) -> BoxFuture<'_, Result<Session>> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(Session {
                    endpoint_host: "h".into(),
                    endpoint_port: 1337,
                    server_pubkey: format!("for-{}", req.client_pubkey),
                    local_address: "10.0.9.2/32".into(),
                    dns: vec![],
                    server_vip: "10.0.9.1".into(),
                    server_cn: "cn".into(),
                    expires_at: None,
                })
            })
        }
        fn port_forward(&self, _r: PortForwardReq) -> BoxFuture<'_, Result<PortForward>> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(PortForward {
                    port: 40000,
                    expires_at: None,
                    refresh_secs: 900,
                })
            })
        }
    }

    #[tokio::test]
    async fn bad_args_are_invalid_naming_the_field() {
        let err = tunnel_provider_dispatch(SESSION_TOOL, &json!({ "provider": 7 }))
            .await
            .unwrap()
            .unwrap_err();
        crate::erased::assert_invalid(&err, "provider");
    }

    #[test]
    fn owns_only_the_two_ops() {
        assert!(tunnel_provider_owns(SESSION_TOOL));
        assert!(tunnel_provider_owns(PORT_FORWARD_TOOL));
        assert!(!tunnel_provider_owns("tunnel_provider.nope"));
    }

    #[test]
    fn both_ops_are_mutations_advertising_dry_run() {
        assert_eq!(
            tunnel_provider_mutation_names(),
            vec![SESSION_TOOL, PORT_FORWARD_TOOL]
        );
        for d in tunnel_provider_mcp_defs() {
            assert_eq!(d["inputSchema"]["properties"]["dryRun"]["type"], "boolean");
        }
    }

    #[tokio::test]
    async fn ops_route_to_named_provider() {
        register_provider(Arc::new(TestVpn));
        let r = tunnel_provider_dispatch(
            SESSION_TOOL,
            &json!({ "provider": "tpsurf-test", "session": { "client_pubkey": "cpk" }, "dryRun": true }),
        )
        .await
        .unwrap()
        .expect("dry");
        assert_eq!(r["dryRun"], true);
        assert!(r["changed"].is_null());
        assert_eq!(r["would_request"]["session"]["client_pubkey"], "cpk");
        let r = tunnel_provider_dispatch(
            PORT_FORWARD_TOOL,
            &json!({ "provider": "tpsurf-test", "port_forward": { "server_cn": "cn", "server_vip": "10.0.9.1" }, "dryRun": true }),
        )
        .await
        .unwrap()
        .expect("dry");
        assert_eq!(r["dryRun"], true);
        assert!(r["changed"].is_null());
        assert_eq!(r["would_request"]["port_forward"]["server_cn"], "cn");
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            0,
            "a dry run called the provider"
        );

        let s = tunnel_provider_dispatch(
            SESSION_TOOL,
            &json!({ "provider": "tpsurf-test", "session": { "client_pubkey": "cpk" } }),
        )
        .await
        .expect("is a tunnel_provider op")
        .expect("ok");
        assert_eq!(s["server_pubkey"], "for-cpk");

        let pf = tunnel_provider_dispatch(
            PORT_FORWARD_TOOL,
            &json!({ "provider": "tpsurf-test", "port_forward": { "server_cn": "cn", "server_vip": "10.0.9.1" } }),
        )
        .await
        .unwrap()
        .expect("ok");
        assert_eq!(pf["port"], 40000);
        assert!(contract::tunnel_provider::deregister_provider(
            "tpsurf-test"
        ));
    }

    #[tokio::test]
    async fn unknown_name_returns_none() {
        assert!(
            tunnel_provider_dispatch("tunnel_provider.nope", &json!({}))
                .await
                .is_none()
        );
    }
}
