//! A gated verb forwarded to a peer must carry the caller's opt-in. The local
//! gate strips `execute` before `run`, so without re-asserting it the peer's
//! gate plans instead of applying, and the plan fails to decode as the verb's
//! output.

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
async fn an_opted_in_gated_verb_forwards_execute_to_the_peer() {
    let (ctx, spy) = peered_ctx(serde_json::json!({ "removed": true }));

    let out = dispatch::dispatch(
        "config.delete",
        serde_json::json!({ "noun": "orca-gate-probe", "name": "no-such-row", "execute": true }),
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
        "forwarded args lost the opt-in: {}",
        calls[0].1
    );
}

#[tokio::test]
async fn a_gated_verb_without_the_opt_in_plans_locally_and_never_forwards() {
    let (ctx, spy) = peered_ctx(serde_json::json!({ "removed": true }));

    let out = dispatch::dispatch(
        "config.delete",
        serde_json::json!({ "noun": "orca-gate-probe", "name": "no-such-row" }),
        &ctx,
    )
    .await
    .expect("dispatch succeeds");

    assert_eq!(out["dryRun"], serde_json::json!(true), "{out}");
    assert!(spy.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unauthorized_opt_in_is_refused_locally_and_never_forwards() {
    let (mut ctx, spy) = peered_ctx(serde_json::json!({ "removed": true }));
    ctx.set_caller(Some(CallerIdentity {
        user_id: "u_member".into(),
        username: "member".into(),
        role: "member".into(),
        can_mutate: false,
    }));

    let err = dispatch::dispatch(
        "config.delete",
        serde_json::json!({ "noun": "orca-gate-probe", "name": "no-such-row", "execute": true }),
        &ctx,
    )
    .await
    .expect_err("a member may not execute config.delete");

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
/// It is gated by verb name, not by action, so `invoke` must forward the opt-in.
#[tokio::test]
async fn plugin_create_invoke_forwards_execute_to_the_peer() {
    let gated = dispatch::execute_gated_names();
    assert!(gated.contains(&"plugin.create"));
    assert!(!gated.contains(&"plugin.update"));

    let (ctx, spy) = peered_ctx(serde_json::json!({ "tool": "docker.list", "result": [] }));

    let out = dispatch::dispatch(
        "plugin.create",
        serde_json::json!({ "action": "invoke", "tool": "docker.list", "args": {}, "execute": true }),
        &ctx,
    )
    .await
    .expect("forwarded call decodes as PluginCreateOutput");

    assert_eq!(out["tool"], serde_json::json!("docker.list"));
    let calls = spy.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["action"], serde_json::json!("invoke"));
    assert_eq!(
        calls[0].1["execute"],
        serde_json::json!(true),
        "{}",
        calls[0].1
    );
}
