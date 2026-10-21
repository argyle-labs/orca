#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use contract::ToolCtx;
use plugin_proto::ToolCall;
use plugin_toolkit::serde_json::{self, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const MAX_DEPTH: usize = 4;
pub const MAX_PER_REQUEST: usize = 64;
pub const PER_USER_CONCURRENCY: usize = 2;
pub const PER_PLUGIN_CONCURRENCY: usize = 8;
const ACQUIRE_WAIT: Duration = Duration::from_secs(2);

pub type CtxFactory = dyn Fn() -> Result<ToolCtx> + Send + Sync;

static CTX_FACTORY: OnceLock<Box<CtxFactory>> = OnceLock::new();

pub fn set_ctx_factory(factory: Box<CtxFactory>) {
    if CTX_FACTORY.set(factory).is_err() {
        tracing::warn!("tool.call context factory already installed; ignoring second install");
    }
}

fn semaphore(principal: &str, user: Option<&str>) -> Arc<Semaphore> {
    type Key = (String, Option<String>);
    static SEMAPHORES: OnceLock<Mutex<HashMap<Key, Arc<Semaphore>>>> = OnceLock::new();
    let permits = if user.is_some() {
        PER_USER_CONCURRENCY
    } else {
        PER_PLUGIN_CONCURRENCY
    };
    SEMAPHORES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry((principal.to_string(), user.map(str::to_string)))
        .or_insert_with(|| Arc::new(Semaphore::new(permits)))
        .clone()
}

fn acquire(
    principal: &str,
    user: &str,
    wait: Duration,
) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
    let per_user = semaphore(principal, Some(user));
    let per_plugin = semaphore(principal, None);
    let busy = format!(
        "tool.call refused: plugin '{principal}' is at its tool.call concurrency limit; retry shortly"
    );
    crate::capability::run_on_cap_runtime(async move {
        let user_permit = tokio::time::timeout(wait, per_user.acquire_owned())
            .await
            .map_err(|_| anyhow!("{busy}"))??;
        let plugin_permit = tokio::time::timeout(wait, per_plugin.acquire_owned())
            .await
            .map_err(|_| anyhow!("{busy}"))??;
        Ok((user_permit, plugin_permit))
    })?
}

fn consume_budget(inv: &spec::surfaces::Invocation) -> Result<()> {
    if inv.call_budget().fetch_add(1, Ordering::SeqCst) >= MAX_PER_REQUEST {
        bail!("tool.call refused: more than {MAX_PER_REQUEST} tool.calls in one request");
    }
    Ok(())
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
    let inv = inv.ok_or_else(|| anyhow!("tool.call: no invocation"))?;
    consume_budget(inv)?;
    let _permits = acquire(principal, &inv.caller().user_id, ACQUIRE_WAIT)?;
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
    if !dispatch::tool_roles::is_tool_callable(&call.tool) {
        bail!(
            "tool.call {} refused: the tool is not marked tool_callable",
            call.tool
        );
    }
    let Some(required) = dispatch::tool_roles::known_role(&call.tool) else {
        bail!("tool.call {} refused: unknown tool", call.tool);
    };
    if required == "super-admin" {
        bail!(
            "tool.call {} refused: super-admin tools are not reachable through tool.call",
            call.tool
        );
    }
    let caller = inv.caller().clone();
    if !dispatch::tool_roles::authorize(
        &caller.role,
        caller.can_mutate,
        required,
        dispatch::tool_roles::is_data_mutation(&call.tool),
    ) {
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
    ctx.set_call_budget(Some(inv.call_budget().clone()));
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
        inv_with(role, chain, gated, args, &["tcall.*", "tcall2.root"])
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
            dispatch::tool_roles::install([("tcall2.root", "super-admin")]);
            let policy = |name: &str, role: &str, callable: bool| {
                (
                    name.to_string(),
                    dispatch::tool_roles::PluginToolPolicy::from_manifest(
                        name,
                        Some(role),
                        Some(true),
                        Some(true),
                    )
                    .tool_callable(Some(callable)),
                )
            };
            dispatch::tool_roles::install_plugin_tools([
                policy("tcall.detail", "read", true),
                policy("tcall.update", "admin", true),
                policy("tcall.plain", "admin", false),
                policy("tcall2.root", "admin", true),
                policy("secrets.detail", "admin", false),
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
    fn tools_not_marked_tool_callable_are_refused_even_when_declared() {
        install_roles();
        for tool in ["tcall.plain", "secrets.detail"] {
            let parent = inv_with("admin", &[], true, &json!({ "execute": true }), &[tool]);
            let err = refusal(prepare(call(tool, json!({})), "p", Some(&parent), ctx()));
            assert!(err.contains("not marked tool_callable"), "{tool}: {err}");
        }
    }

    #[test]
    fn unknown_and_super_admin_tools_are_refused() {
        install_roles();
        let parent = inv_with(
            "admin",
            &[],
            true,
            &json!({ "execute": true }),
            &["*.x", "nope.*"],
        );
        let err = refusal(prepare(
            call("nope.tool", json!({})),
            "p",
            Some(&parent),
            ctx(),
        ));
        assert!(err.contains("not marked tool_callable"), "{err}");
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
    fn nested_fan_out_shares_one_budget() {
        install_roles();
        let root = executing();
        for _ in 0..MAX_PER_REQUEST / 2 {
            consume_budget(&root).unwrap();
        }
        let (_, _, child_ctx) = prepare(
            call("tcall.detail", json!({})),
            "mylar3",
            Some(&root),
            ctx(),
        )
        .unwrap();
        let child = spec::surfaces::Invocation::from_ctx(&child_ctx).unwrap();
        for _ in 0..MAX_PER_REQUEST / 2 {
            consume_budget(&child).unwrap();
        }
        let err = consume_budget(&child).unwrap_err().to_string();
        assert!(err.contains("more than"), "{err}");
        assert!(consume_budget(&root).is_err());
        let fresh = executing();
        assert!(consume_budget(&fresh).is_ok());
    }

    #[test]
    fn one_user_cannot_starve_another() {
        let wait = Duration::from_millis(50);
        let held: Vec<_> = (0..PER_USER_CONCURRENCY)
            .map(|_| acquire("shared", "u-a", wait).unwrap())
            .collect();
        let err = acquire("shared", "u-a", wait).unwrap_err().to_string();
        assert!(err.contains("concurrency limit"), "{err}");
        let other = acquire("shared", "u-b", wait).unwrap();
        drop(held);
        drop(other);
        assert!(acquire("shared", "u-a", wait).is_ok());
    }

    #[test]
    fn the_plugin_ceiling_bounds_all_users() {
        let wait = Duration::from_millis(50);
        let held: Vec<_> = (0..PER_PLUGIN_CONCURRENCY)
            .map(|i| acquire("ceiling", &format!("u-{i}"), wait).unwrap())
            .collect();
        let err = acquire("ceiling", "u-new", wait).unwrap_err().to_string();
        assert!(err.contains("concurrency limit"), "{err}");
        drop(held);
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
