#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Result, anyhow, bail};
use contract::ToolCtx;
use plugin_proto::ToolCall;
use plugin_toolkit::serde_json::{self, Value};
use tokio::sync::Semaphore;

pub const MAX_DEPTH: usize = 4;
pub const MAX_PER_INVOKE: usize = 64;
pub const PER_PLUGIN_CONCURRENCY: usize = 4;
const SECRET_BEARING: &[&str] = &["secrets.", "auth.", "pki."];

pub type CtxFactory = dyn Fn() -> Result<ToolCtx> + Send + Sync;

static CTX_FACTORY: OnceLock<Box<CtxFactory>> = OnceLock::new();

pub fn set_ctx_factory(factory: Box<CtxFactory>) {
    if CTX_FACTORY.set(factory).is_err() {
        tracing::warn!("tool.call context factory already installed; ignoring second install");
    }
}

fn plugin_share(principal: &str) -> Arc<Semaphore> {
    static SHARES: OnceLock<Mutex<HashMap<String, Arc<Semaphore>>>> = OnceLock::new();
    SHARES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(principal.to_string())
        .or_insert_with(|| Arc::new(Semaphore::new(PER_PLUGIN_CONCURRENCY)))
        .clone()
}

pub fn handle(
    args: Value,
    principal: &str,
    inv: Option<&spec::surfaces::Invocation>,
    calls: &mut usize,
) -> Result<Value> {
    *calls += 1;
    if *calls > MAX_PER_INVOKE {
        bail!("tool.call refused: more than {MAX_PER_INVOKE} tool.calls in one invocation");
    }
    let _permit = plugin_share(principal).try_acquire_owned().map_err(|_| {
        anyhow!(
            "tool.call refused: plugin '{principal}' already has {PER_PLUGIN_CONCURRENCY} tool.calls in flight"
        )
    })?;
    let factory = CTX_FACTORY
        .get()
        .ok_or_else(|| anyhow!("tool.call unavailable: this host has no tool context"))?;
    let (tool, args, ctx) = prepare(args, principal, inv, factory()?)?;
    let name = tool.clone();
    let result = crate::capability::run_on_cap_runtime(async move {
        dispatch::dispatch(&tool, args, &ctx).await
    })?;
    result.map_err(|e| {
        let full = format!("{e:#}");
        tracing::warn!(plugin = %principal, tool = %name, error = %full, "tool.call failed");
        anyhow!("{}", utils::scrub::scrub_line(&full))
    })
}

fn declared(tool: &str, tool_calls: &[String]) -> bool {
    tool_calls
        .iter()
        .any(|entry| match entry.strip_suffix(".*") {
            Some(stem) => tool
                .strip_prefix(stem)
                .is_some_and(|rest| rest.starts_with('.') && rest.len() > 1),
            None => entry == tool,
        })
}

fn secret_bearing(tool: &str) -> bool {
    SECRET_BEARING.iter().any(|p| tool.starts_with(p))
}

fn forbidden(principal: &str, tool: &str, required: &str, why: &str) -> anyhow::Error {
    tracing::warn!(plugin = %principal, tool = %tool, required = %required, "tool.call refused: {why}");
    contract::OrcaError::forbidden(format!("requires role {required}")).into()
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
    if !declared(&call.tool, inv.tool_calls()) {
        bail!(
            "tool.call {} refused: plugin '{principal}' does not declare it in tool_calls",
            call.tool
        );
    }
    if secret_bearing(&call.tool) {
        bail!(
            "tool.call {} refused: secret-bearing tools are not reachable through tool.call",
            call.tool
        );
    }
    let required = dispatch::tool_roles::required_role(&call.tool);
    if required == "super-admin" {
        bail!(
            "tool.call {} refused: super-admin tools are not reachable through tool.call",
            call.tool
        );
    }
    let caller = inv.caller().clone();
    if let Err(required) =
        dispatch::tool_roles::may_invoke(&call.tool, Some(&caller.role), caller.can_mutate)
    {
        return Err(forbidden(
            principal,
            &call.tool,
            required,
            &format!("caller '{}' has role '{}'", caller.username, caller.role),
        ));
    }
    let (mut args, peer, correlation_id) = dispatch::take_ambient(call.args)?;
    if peer.is_some() || args.get(dispatch::SYSTEM_ID_KEY).is_some() {
        bail!(
            "tool.call {} refused: remote routing (peer/{}) is not available through tool.call",
            call.tool,
            dispatch::SYSTEM_ID_KEY
        );
    }
    if !inv.child_may_execute() {
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
    ctx.set_peer(None);
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

    fn ctx() -> ToolCtx {
        ToolCtx::new(Arc::new(contract::config::Config::load().unwrap()))
    }

    fn inv_with(
        role: &str,
        chain: &[&str],
        gated: bool,
        args: &Value,
        tool_calls: &[&str],
    ) -> spec::surfaces::Invocation {
        let mut c = ctx();
        c.set_verified_caller(Some(contract::CallerIdentity {
            user_id: "u-alice".into(),
            username: "alice".into(),
            role: role.into(),
            can_mutate: false,
        }));
        c.set_call_chain(chain.iter().map(|s| s.to_string()).collect());
        let tool_calls: Vec<String> = tool_calls.iter().map(|s| s.to_string()).collect();
        spec::surfaces::Invocation::from_ctx(&c)
            .unwrap()
            .for_plugin_invoke(gated, role, &[], args)
            .with_tool_calls(&tool_calls)
    }

    fn inv(role: &str, chain: &[&str], gated: bool, args: &Value) -> spec::surfaces::Invocation {
        inv_with(
            role,
            chain,
            gated,
            args,
            &["tcall.*", "secrets.detail", "tcall2.root"],
        )
    }

    fn executing() -> spec::surfaces::Invocation {
        inv("admin", &[], true, &json!({ "execute": true }))
    }

    fn call(tool: &str, args: Value) -> Value {
        json!({ "tool": tool, "args": args })
    }

    fn refusal(r: Result<(String, Value, ToolCtx)>) -> String {
        r.err().expect("refused").to_string()
    }

    fn install_roles() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            dispatch::tool_roles::install([
                ("tcall.detail", "read"),
                ("tcall.update", "admin"),
                ("tcall2.root", "super-admin"),
                ("secrets.detail", "admin"),
            ]);
        });
    }

    #[test]
    fn identity_is_the_callers_not_the_plugins() {
        install_roles();
        let (tool, _, ctx) = prepare(
            call("tcall.detail", json!({})),
            "mylar3",
            Some(&executing()),
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
        let err = refusal(prepare(
            call("tcall.update", json!({ "execute": true })),
            "mylar3",
            Some(&parent),
            ctx(),
        ));
        assert_eq!(err, "forbidden: requires role admin");
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
    fn undeclared_tools_are_refused() {
        install_roles();
        let none = inv_with("admin", &[], true, &json!({ "execute": true }), &[]);
        let err = refusal(prepare(
            call("tcall.detail", json!({})),
            "p",
            Some(&none),
            ctx(),
        ));
        assert!(err.contains("does not declare"), "{err}");
        let exact = inv_with("admin", &[], false, &json!({}), &["tcall.detail"]);
        assert!(prepare(call("tcall.detail", json!({})), "p", Some(&exact), ctx()).is_ok());
        let err = refusal(prepare(
            call("tcall.update", json!({})),
            "p",
            Some(&exact),
            ctx(),
        ));
        assert!(err.contains("does not declare"), "{err}");
    }

    #[test]
    fn globs_match_only_their_prefix() {
        let globs = vec!["tcall.*".to_string()];
        assert!(declared("tcall.detail", &globs));
        assert!(declared("tcall.a.b", &globs));
        assert!(!declared("tcall", &globs));
        assert!(!declared("tcall.", &globs));
        assert!(!declared("tcallx.detail", &globs));
    }

    #[test]
    fn secret_bearing_and_super_admin_tools_are_refused() {
        install_roles();
        let err = refusal(prepare(
            call("secrets.detail", json!({})),
            "p",
            Some(&executing()),
            ctx(),
        ));
        assert!(err.contains("secret-bearing"), "{err}");
        let err = refusal(prepare(
            call("tcall2.root", json!({})),
            "p",
            Some(&executing()),
            ctx(),
        ));
        assert!(err.contains("super-admin"), "{err}");
    }

    #[test]
    fn a_dry_run_parent_forces_a_dry_run_child() {
        install_roles();
        let parent = inv(
            "admin",
            &[],
            true,
            &json!({ "dryRun": true, "execute": true }),
        );
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
    fn an_ungated_read_parent_cannot_execute_an_admin_child() {
        install_roles();
        let parent = inv("admin", &[], false, &json!({ "execute": true }));
        let (_, args, ctx) = prepare(
            call("tcall.update", json!({ "execute": true })),
            "mylar3",
            Some(&parent),
            ctx(),
        )
        .unwrap();
        assert_eq!(args, json!({ "dryRun": true }));
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
    fn a_confirmed_gated_parent_passes_child_args_through() {
        install_roles();
        let (_, args, ctx) = prepare(
            call("tcall.update", json!({ "execute": true, "targetId": "t" })),
            "mylar3",
            Some(&executing()),
            ctx(),
        )
        .unwrap();
        assert_eq!(args, json!({ "execute": true, "targetId": "t" }));
        assert!(!ctx.dry_run());
    }

    #[test]
    fn remote_routing_is_refused() {
        install_roles();
        let id = "00000000-0000-4000-8000-000000000001";
        for args in [json!({ "systemId": id }), json!({ "peer": id })] {
            let err = refusal(prepare(
                call("tcall.detail", args),
                "mylar3",
                Some(&executing()),
                ctx(),
            ));
            assert!(err.contains("remote routing"), "{err}");
        }
    }

    #[test]
    fn depth_is_limited() {
        install_roles();
        let chain = ["a", "b", "c", "d"];
        let parent = inv("admin", &chain[..MAX_DEPTH - 1], false, &json!({}));
        assert!(prepare(call("tcall.detail", json!({})), "e", Some(&parent), ctx()).is_ok());
        let parent = inv("admin", &chain, false, &json!({}));
        let err = refusal(prepare(
            call("tcall.detail", json!({})),
            "e",
            Some(&parent),
            ctx(),
        ));
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn a_plugin_cannot_call_into_its_own_chain() {
        let chain: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        assert!(refuse_loop("c.x", &chain, Some("c")).is_ok());
        assert!(refuse_loop("core.x", &chain, None).is_ok());
        for owner in ["a", "b"] {
            let err = refuse_loop("t.x", &chain, Some(owner))
                .unwrap_err()
                .to_string();
            assert!(err.contains("already in the call chain"), "{err}");
        }
    }

    #[test]
    fn plugin_invoke_refuses_a_principal_already_in_the_chain() {
        let mut c = ctx();
        c.set_call_chain(vec!["a".into(), "b".into()]);
        assert!(crate::refuse_reentry(&c, "c", "c.x").is_ok());
        let err = crate::refuse_reentry(&c, "a", "a.x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already in the tool.call chain"), "{err}");
    }

    #[test]
    fn calls_per_invocation_are_capped() {
        let mut calls = MAX_PER_INVOKE;
        let err = handle(call("tcall.detail", json!({})), "capped", None, &mut calls)
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than"), "{err}");
    }

    #[test]
    fn concurrency_per_plugin_is_capped() {
        let held = plugin_share("busy")
            .try_acquire_many_owned(PER_PLUGIN_CONCURRENCY as u32)
            .unwrap();
        let mut calls = 0;
        let err = handle(call("tcall.detail", json!({})), "busy", None, &mut calls)
            .unwrap_err()
            .to_string();
        assert!(err.contains("in flight"), "{err}");
        drop(held);
        let err = handle(call("tcall.detail", json!({})), "busy", None, &mut calls)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("in flight"), "{err}");
    }

    #[test]
    fn refused_without_a_caller() {
        let err = refusal(prepare(
            call("tcall.detail", json!({})),
            "mylar3",
            None,
            ctx(),
        ));
        assert!(err.contains("no authenticated caller"), "{err}");
        let system = spec::surfaces::Invocation::system();
        let err = refusal(prepare(
            call("tcall.detail", json!({})),
            "mylar3",
            Some(&system),
            ctx(),
        ));
        assert!(err.contains("no authenticated caller"), "{err}");
    }

    #[test]
    fn a_malformed_payload_is_an_error() {
        let parent = inv("admin", &[], false, &json!({}));
        let err = refusal(prepare(
            json!({ "nope": 1 }),
            "mylar3",
            Some(&parent),
            ctx(),
        ));
        assert!(err.contains("tool.call: bad payload"), "{err}");
    }
}
