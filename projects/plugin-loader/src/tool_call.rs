#![allow(clippy::disallowed_types)]

use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail};
use contract::ToolCtx;
use plugin_proto::ToolCall;
use plugin_toolkit::serde_json::{self, Value};

pub const MAX_DEPTH: usize = 4;

pub type CtxFactory = dyn Fn() -> Result<ToolCtx> + Send + Sync;

static CTX_FACTORY: OnceLock<Box<CtxFactory>> = OnceLock::new();

pub fn set_ctx_factory(factory: Box<CtxFactory>) {
    if CTX_FACTORY.set(factory).is_err() {
        tracing::warn!("tool.call context factory already installed; ignoring second install");
    }
}

pub fn handle(
    args: Value,
    principal: &str,
    inv: Option<&spec::surfaces::Invocation>,
) -> Result<Value> {
    let factory = CTX_FACTORY
        .get()
        .ok_or_else(|| anyhow!("tool.call unavailable: this host has no tool context"))?;
    let (tool, args, ctx) = prepare(args, principal, inv, factory()?)?;
    crate::capability::run_on_cap_runtime(
        async move { dispatch::dispatch(&tool, args, &ctx).await },
    )?
}

pub(crate) fn prepare(
    args: Value,
    principal: &str,
    inv: Option<&spec::surfaces::Invocation>,
    mut ctx: ToolCtx,
) -> Result<(String, Value, ToolCtx)> {
    let call: ToolCall =
        serde_json::from_value(args).map_err(|e| anyhow!("tool.call: bad payload: {e}"))?;
    let Some(inv) = inv.filter(|i| !i.is_system()) else {
        bail!(
            "tool.call {} refused: plugin '{principal}' has no authenticated caller to act as",
            call.tool
        );
    };
    let mut chain = inv.call_chain().to_vec();
    chain.push(principal.to_string());
    refuse_loop(&call.tool, &chain, owner(&call.tool).as_deref())?;
    let caller = inv.caller().clone();
    if let Err(required) =
        dispatch::tool_roles::may_invoke(&call.tool, Some(&caller.role), caller.can_mutate)
    {
        return Err(contract::OrcaError::forbidden(format!(
            "tool.call {} requires role '{required}'; caller '{}' has '{}'",
            call.tool, caller.username, caller.role
        ))
        .into());
    }
    let (mut args, peer, correlation_id) = dispatch::take_ambient(call.args)?;
    if inv.plans() {
        let obj = args
            .as_object_mut()
            .ok_or_else(|| anyhow!("tool.call {}: args must be a JSON object", call.tool))?;
        obj.remove(contract::plan::EXECUTE_FIELD);
        obj.insert(contract::plan::DRY_RUN_FIELD.into(), Value::Bool(true));
        ctx.set_dry_run(true);
    }
    ctx.set_verified_caller(Some(caller.clone()));
    ctx.set_caller(Some(caller));
    ctx.set_unbound_role(None);
    ctx.set_second_factor(false);
    ctx.set_local_transport(false);
    ctx.set_peer(peer);
    ctx.set_correlation_id(correlation_id);
    ctx.set_call_chain(chain);
    Ok((call.tool, args, ctx))
}

fn refuse_loop(tool: &str, chain: &[String], owner: Option<&str>) -> Result<()> {
    if chain.len() > MAX_DEPTH {
        bail!(
            "tool.call {tool} refused: call depth {} exceeds {MAX_DEPTH} ({})",
            chain.len(),
            chain.join(" -> ")
        );
    }
    if let Some(owner) = owner
        && chain.iter().any(|p| p == owner)
    {
        bail!(
            "tool.call {tool} refused: plugin '{owner}' is already in the call chain ({})",
            chain.join(" -> ")
        );
    }
    Ok(())
}

fn owner(tool: &str) -> Option<String> {
    crate::backing_for(tool).map(|(_, software)| software)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_toolkit::serde_json::json;
    use std::sync::Arc;

    fn ctx() -> ToolCtx {
        ToolCtx::new(Arc::new(contract::config::Config::load().unwrap()))
    }

    fn inv(role: &str, chain: &[&str], gated: bool, args: &Value) -> spec::surfaces::Invocation {
        let mut c = ctx();
        c.set_verified_caller(Some(contract::CallerIdentity {
            user_id: "u-alice".into(),
            username: "alice".into(),
            role: role.into(),
            can_mutate: false,
        }));
        c.set_call_chain(chain.iter().map(|s| s.to_string()).collect());
        spec::surfaces::Invocation::from_ctx(&c)
            .unwrap()
            .for_plugin_invoke(gated, role, &[], args)
    }

    fn call(tool: &str, args: Value) -> Value {
        json!({ "tool": tool, "args": args })
    }

    fn install_roles() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            dispatch::tool_roles::install([("tcall.detail", "read"), ("tcall.update", "admin")]);
        });
    }

    #[test]
    fn identity_is_the_callers_not_the_plugins() {
        install_roles();
        let parent = inv("admin", &[], false, &json!({}));
        let (tool, _, ctx) = prepare(
            call("tcall.detail", json!({})),
            "mylar3",
            Some(&parent),
            ctx(),
        )
        .unwrap();
        assert_eq!(tool, "tcall.detail");
        let who = ctx.verified_caller().unwrap();
        assert_eq!(who.username, "alice");
        assert_eq!(who.role, "admin");
        assert_eq!(ctx.caller().unwrap().username, "alice");
        assert_eq!(ctx.call_chain(), ["mylar3".to_string()]);
    }

    #[test]
    fn a_read_only_caller_cannot_reach_an_admin_tool() {
        install_roles();
        let parent = inv("read-only", &[], false, &json!({}));
        let err = prepare(
            call("tcall.update", json!({ "execute": true })),
            "mylar3",
            Some(&parent),
            ctx(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            err.contains("requires role 'admin'") && err.contains("alice"),
            "{err}"
        );
        let reader = inv("read", &[], false, &json!({}));
        assert!(
            prepare(
                call("tcall.detail", json!({})),
                "mylar3",
                Some(&reader),
                ctx()
            )
            .is_ok()
        );
    }

    #[test]
    fn a_dry_run_parent_forces_a_dry_run_child() {
        install_roles();
        let parent = inv("admin", &[], false, &json!({ "dryRun": true }));
        let (_, args, ctx) = prepare(
            call("tcall.update", json!({ "execute": true, "tag": "t" })),
            "mylar3",
            Some(&parent),
            ctx(),
        )
        .unwrap();
        assert_eq!(args, json!({ "dryRun": true, "tag": "t" }));
        assert!(ctx.dry_run());
    }

    #[test]
    fn an_unconfirmed_gated_parent_forces_a_dry_run_child() {
        install_roles();
        let parent = inv("admin", &[], true, &json!({}));
        let (_, args, _) = prepare(
            call("tcall.update", json!({ "execute": true })),
            "mylar3",
            Some(&parent),
            ctx(),
        )
        .unwrap();
        assert_eq!(args, json!({ "dryRun": true }));
    }

    #[test]
    fn an_executing_parent_passes_child_args_through() {
        install_roles();
        let parent = inv("admin", &[], true, &json!({ "execute": true }));
        let (_, args, ctx) = prepare(
            call("tcall.update", json!({ "execute": true })),
            "mylar3",
            Some(&parent),
            ctx(),
        )
        .unwrap();
        assert_eq!(args, json!({ "execute": true }));
        assert!(!ctx.dry_run());
    }

    #[test]
    fn depth_is_limited() {
        install_roles();
        let chain = ["a", "b", "c", "d"];
        let parent = inv("admin", &chain[..MAX_DEPTH - 1], false, &json!({}));
        assert!(prepare(call("tcall.detail", json!({})), "e", Some(&parent), ctx()).is_ok());
        let parent = inv("admin", &chain, false, &json!({}));
        let err = prepare(call("tcall.detail", json!({})), "e", Some(&parent), ctx())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn a_plugin_cannot_call_into_its_own_chain() {
        let chain: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        assert!(refuse_loop("c.x", &chain, Some("c")).is_ok());
        assert!(refuse_loop("core.x", &chain, None).is_ok());
        for owner in ["a", "b"] {
            let err = refuse_loop("t.x", &chain, Some(owner))
                .err()
                .unwrap()
                .to_string();
            assert!(err.contains("already in the call chain"), "{err}");
        }
    }

    #[test]
    fn system_targets_pass_through_to_dispatch() {
        install_roles();
        let parent = inv("admin", &[], false, &json!({}));
        let id = "00000000-0000-4000-8000-000000000001";
        let (_, args, _) = prepare(
            call("tcall.detail", json!({ "systemId": id })),
            "mylar3",
            Some(&parent),
            ctx(),
        )
        .unwrap();
        assert_eq!(args, json!({ "systemId": id }));
    }

    #[test]
    fn refused_without_a_caller() {
        let err = prepare(call("tcall.detail", json!({})), "mylar3", None, ctx())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("no authenticated caller"), "{err}");
        let system = spec::surfaces::Invocation::system();
        let err = prepare(
            call("tcall.detail", json!({})),
            "mylar3",
            Some(&system),
            ctx(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("no authenticated caller"), "{err}");
    }

    #[test]
    fn a_malformed_payload_is_an_error() {
        let parent = inv("admin", &[], false, &json!({}));
        let err = prepare(json!({ "nope": 1 }), "mylar3", Some(&parent), ctx())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("tool.call: bad payload"), "{err}");
    }
}
