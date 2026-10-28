use auth as _;
use contract::CallerIdentity;

fn ctx(role: &str, second_factor: bool) -> contract::ToolCtx {
    let cfg = std::sync::Arc::new(contract::config::Config::load().expect("config"));
    let mut ctx = contract::ToolCtx::new(cfg);
    ctx.set_caller(Some(CallerIdentity {
        user_id: format!("u_{role}"),
        username: role.into(),
        role: role.into(),
        can_mutate: false,
    }));
    ctx.set_second_factor(second_factor);
    ctx
}

#[tokio::test]
async fn super_admin_without_a_second_factor_passes_the_gate() {
    // These tools don't opt into `second_factor = true` — there's no 2FA
    // enrollment yet, so requiring it would lock every super-admin out.
    for (tool, args) in [
        (
            "secrets.put",
            serde_json::json!({ "name": "orca-gate-probe", "value": "v", "dryRun": true }),
        ),
        (
            "secrets.restore",
            serde_json::json!({ "name": "orca-gate-probe", "dryRun": true }),
        ),
        (
            "secrets.upsert",
            serde_json::json!({ "name": "orca-gate-probe", "value": "v", "dryRun": true }),
        ),
        (
            "secrets.delete",
            serde_json::json!({ "name": "orca-gate-probe", "dryRun": true }),
        ),
    ] {
        let out = dispatch::dispatch(tool, args, &ctx("super-admin", false))
            .await
            .expect(tool);
        assert_eq!(out["dryRun"], serde_json::json!(true), "{tool}: {out}");
    }
}

#[tokio::test]
async fn admin_is_forbidden_on_secret_writes() {
    for tool in [
        "secrets.put",
        "secrets.restore",
        "secrets.upsert",
        "secrets.delete",
    ] {
        let err = dispatch::dispatch(
            tool,
            serde_json::json!({ "name": "orca-gate-probe", "value": "v", "dryRun": true }),
            &ctx("admin", true),
        )
        .await
        .expect_err("admin");
        assert!(err.to_string().contains("super-admin"), "{tool}: {err:#}");
    }
}

#[tokio::test]
async fn super_admin_with_a_second_factor_passes_the_gate() {
    let out = dispatch::dispatch(
        "secrets.put",
        serde_json::json!({ "name": "orca-gate-probe", "value": "v", "dryRun": true }),
        &ctx("super-admin", true),
    )
    .await
    .expect("plan");
    assert_eq!(out["dryRun"], serde_json::json!(true), "{out}");
}
