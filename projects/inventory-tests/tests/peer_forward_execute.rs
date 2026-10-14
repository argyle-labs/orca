//! A gated verb executes on the peer by default; a `dryRun` call plans locally
//! and never forwards. The forwarded args keep `execute: true` so an older
//! peer, whose gate still plans by default, applies too. A dry run of a
//! dry-run-aware verb forwards `dryRun` with its apply opt-ins cleared.

use std::sync::{Arc, Mutex};

use contract::{CallerIdentity, RemoteExec};

use system as _;

/// Records every forwarded call and answers with a fixed body.
#[allow(clippy::disallowed_types)]
struct SpyExec {
    calls: Mutex<Vec<(String, serde_json::Value)>>,
    reply: serde_json::Value,
}

// The trait's wire payload is untyped JSON by contract.
#[allow(clippy::disallowed_types)]
#[async_trait::async_trait]
impl RemoteExec for SpyExec {
    async fn exec(
        &self,
        _peer: &str,
        tool: &str,
        args: serde_json::Value,
        _caller: Option<CallerIdentity>,
        _cid: Option<String>,
    ) -> anyhow::Result<serde_json::Value> {
        self.calls.lock().unwrap().push((tool.to_string(), args));
        Ok(self.reply.clone())
    }
}

#[allow(clippy::disallowed_types)]
fn peered_ctx(reply: serde_json::Value) -> (contract::ToolCtx, Arc<SpyExec>) {
    let cfg = Arc::new(contract::config::Config::load().expect("config"));
    let mut ctx = contract::ToolCtx::new(cfg);
    let spy = Arc::new(SpyExec {
        calls: Mutex::new(Vec::new()),
        reply,
    });
    let svc: Arc<dyn RemoteExec> = spy.clone();
    ctx.register_service(svc);
    ctx.set_peer(Some("peer-under-test".into()));
    (ctx, spy)
}

#[tokio::test]
async fn a_gated_verb_executes_on_the_peer_by_default() {
    let (ctx, spy) = peered_ctx(serde_json::json!({ "removed": true }));

    let out = dispatch::dispatch(
        "config.delete",
        serde_json::json!({ "noun": "orca-gate-probe", "name": "no-such-row" }),
        &ctx,
    )
    .await
    .expect("forwarded call decodes as the verb's output");

    assert_eq!(out, serde_json::json!({ "removed": true }));
    let calls = spy.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "config.delete");
    assert_eq!(
        calls[0].1["execute"],
        serde_json::json!(true),
        "forwarded args lost the legacy opt-in for older peers: {}",
        calls[0].1
    );
    assert!(calls[0].1.get("dryRun").is_none(), "{}", calls[0].1);
}

#[tokio::test]
async fn a_dry_run_plans_locally_and_never_forwards() {
    let (ctx, spy) = peered_ctx(serde_json::json!({ "removed": true }));

    let out = dispatch::dispatch(
        "config.delete",
        serde_json::json!({ "noun": "orca-gate-probe", "name": "no-such-row", "dryRun": true }),
        &ctx,
    )
    .await
    .expect("dispatch succeeds");

    assert_eq!(out["dryRun"], serde_json::json!(true), "{out}");
    assert!(spy.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unauthorized_call_is_refused_locally_and_never_forwards() {
    let (mut ctx, spy) = peered_ctx(serde_json::json!({ "removed": true }));
    ctx.set_caller(Some(CallerIdentity {
        user_id: "u_member".into(),
        username: "member".into(),
        role: "member".into(),
        can_mutate: false,
    }));

    let err = dispatch::dispatch(
        "config.delete",
        serde_json::json!({ "noun": "orca-gate-probe", "name": "no-such-row" }),
        &ctx,
    )
    .await
    .expect_err("a member may not execute config.delete");
    assert!(err.to_string().contains("requires role"), "{err}");

    let err = dispatch::dispatch(
        "config.delete",
        serde_json::json!({ "noun": "orca-gate-probe", "name": "no-such-row", "dryRun": true }),
        &ctx,
    )
    .await
    .expect_err("a member may not dry-run config.delete either");
    assert!(err.to_string().contains("requires role"), "{err}");

    assert!(spy.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_ungated_verb_forwards_without_execute() {
    let (ctx, spy) = peered_ctx(serde_json::json!({ "rows": [] }));

    // Only the forwarded args matter; the canned reply need not decode.
    drop(dispatch::dispatch("config.list", serde_json::json!({}), &ctx).await);

    let calls = spy.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].1.get("execute").is_none(),
        "ungated verb grew an execute field: {}",
        calls[0].1
    );
}

/// `plugin.create action=invoke` is the mesh seam for peer-loaded plugin verbs.
/// It is not generically gated (#802): a read verb runs without an outer
/// `execute`, and the call is forwarded rather than planned locally.
#[tokio::test]
async fn plugin_create_invoke_runs_a_read_verb_without_execute() {
    let gated = dispatch::execute_gated_names();
    assert!(!gated.contains(&"plugin.create"));
    assert!(!gated.contains(&"plugin.update"));

    let (ctx, spy) = peered_ctx(serde_json::json!({ "tool": "docker.list", "result": [] }));

    let out = dispatch::dispatch(
        "plugin.create",
        serde_json::json!({ "action": "invoke", "tool": "docker.list", "args": {} }),
        &ctx,
    )
    .await
    .expect("forwarded call decodes as PluginCreateOutput");

    assert_eq!(out["tool"], serde_json::json!("docker.list"));
    assert!(out.get("dryRun").is_none(), "{out}");
    let calls = spy.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["action"], serde_json::json!("invoke"));
}

/// A mutating plugin verb's own opt-in rides inside `args` untouched, so the
/// plugin's gate (not orca's) decides between plan and apply.
#[tokio::test]
async fn plugin_create_invoke_forwards_the_inner_args_verbatim() {
    let (ctx, spy) = peered_ctx(serde_json::json!({ "tool": "docker.stack_allow", "result": {} }));

    dispatch::dispatch(
        "plugin.create",
        serde_json::json!({
            "action": "invoke",
            "tool": "docker.stack_allow",
            "args": { "stack": "s", "execute": true },
        }),
        &ctx,
    )
    .await
    .expect("forwarded");

    let calls = spy.calls.lock().unwrap();
    assert_eq!(
        calls[0].1["args"],
        serde_json::json!({ "stack": "s", "execute": true })
    );
}

/// `install` still applies only on an explicit opt-in: without it the peer is
/// asked for a plan (`execute: false`) and the plan decodes as the output.
#[tokio::test]
async fn plugin_create_install_without_execute_asks_the_peer_for_a_plan() {
    let (ctx, spy) = peered_ctx(serde_json::json!({
        "dryRun": true, "tool": "plugin.create", "summary": "s", "detailed": false,
        "inputs": {}, "howToExecute": "re-invoke plugin.create with `execute: true` to apply",
    }));

    let out = dispatch::dispatch(
        "plugin.create",
        serde_json::json!({ "action": "install", "name": "docker" }),
        &ctx,
    )
    .await
    .expect("dispatch succeeds");

    assert_eq!(out["dryRun"], serde_json::json!(true), "{out}");
    let calls = spy.calls.lock().unwrap();
    assert_eq!(
        calls[0].1["execute"],
        serde_json::json!(false),
        "{}",
        calls[0].1
    );
}

/// Without a peer, `install` with no opt-in plans locally and touches nothing.
#[tokio::test]
async fn plugin_create_install_without_execute_plans_locally() {
    let cfg = Arc::new(contract::config::Config::load().expect("config"));
    let ctx = contract::ToolCtx::new(cfg);

    let out = dispatch::dispatch(
        "plugin.create",
        serde_json::json!({ "action": "install", "name": "no-such-plugin" }),
        &ctx,
    )
    .await
    .expect("plans rather than fetching");

    assert_eq!(out["dryRun"], serde_json::json!(true), "{out}");
    assert_eq!(out["tool"], serde_json::json!("plugin.create"));
}

#[tokio::test]
async fn plugin_create_install_with_execute_forwards_the_opt_in() {
    let (ctx, spy) = peered_ctx(serde_json::json!({
        "software": "docker", "version": "1", "tools": [],
        "installedPath": "/x", "loadedLive": true,
    }));

    dispatch::dispatch(
        "plugin.create",
        serde_json::json!({ "action": "install", "name": "docker", "execute": true }),
        &ctx,
    )
    .await
    .expect("forwarded");

    let calls = spy.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].1["execute"],
        serde_json::json!(true),
        "{}",
        calls[0].1
    );
}

/// A dry run of a dry-run-aware verb forwards `dryRun: true` for a current
/// peer and cleared apply opt-ins for one that ignores `dryRun`.
#[tokio::test]
async fn a_dry_run_forwards_dry_run_and_neutralized_args() {
    let cases = [
        (
            "system.update",
            serde_json::json!({ "version": "9.9.9", "execute": true, "dryRun": true }),
        ),
        (
            "plugin.update",
            serde_json::json!({ "name": "docker", "execute": true, "dryRun": true }),
        ),
        (
            "storage.share.repair-permissions",
            serde_json::json!({ "path": "/mnt/none", "apply": true, "dryRun": true }),
        ),
    ];
    for (tool, args) in cases {
        let (ctx, spy) = peered_ctx(serde_json::json!({}));
        // Only the forwarded args matter; the canned reply need not decode.
        drop(dispatch::dispatch(tool, args, &ctx).await);

        let calls = spy.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "{tool}");
        let fwd = &calls[0].1;
        assert_eq!(fwd["dryRun"], serde_json::json!(true), "{tool}: {fwd}");
        for opt_in in ["execute", "apply"] {
            assert!(
                !fwd.get(opt_in).and_then(|v| v.as_bool()).unwrap_or(false),
                "{tool} forwarded {opt_in}: {fwd}"
            );
        }
        assert!(
            fwd.get("version").is_none_or(|v| v.is_null()),
            "{tool}: {fwd}"
        );
    }
}

/// The plan is answered by the role check first: a non-admin gets neither a
/// plan nor a forward.
#[tokio::test]
async fn plugin_create_dry_run_is_admin_only() {
    let (mut ctx, spy) = peered_ctx(serde_json::json!({}));
    ctx.set_caller(Some(CallerIdentity {
        user_id: "u_member".into(),
        username: "member".into(),
        role: "member".into(),
        can_mutate: false,
    }));

    let err = dispatch::dispatch(
        "plugin.create",
        serde_json::json!({ "action": "install", "name": "docker" }),
        &ctx,
    )
    .await
    .expect_err("a member may not plan plugin.create");

    assert!(err.to_string().contains("requires role"), "{err}");
    assert!(spy.calls.lock().unwrap().is_empty());
}
