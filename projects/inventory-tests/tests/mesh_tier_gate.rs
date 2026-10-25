use contract::CallerIdentity;
use system as _;

const TOOL: &str = "system.mesh.tier.update";

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

#[test]
fn tier_update_is_local_only_and_grant_delivery_is_not() {
    assert!(dispatch::local_only_names().contains(&TOOL));
    assert!(!dispatch::local_only_names().contains(&"system.mesh.update"));
}

#[tokio::test]
async fn super_admin_without_a_second_factor_is_forbidden() {
    let err = dispatch::dispatch(
        TOOL,
        serde_json::json!({ "action": "demote", "dryRun": true }),
        &ctx("super-admin", false),
    )
    .await
    .expect_err("no second factor");
    let oe = err
        .downcast_ref::<contract::OrcaError>()
        .unwrap_or_else(|| panic!("{err:#}"));
    assert_eq!(oe.kind, contract::ErrorKind::Forbidden, "{err:#}");
    assert!(err.to_string().contains("second factor"), "{err:#}");
}

#[tokio::test]
async fn admin_is_forbidden() {
    let err = dispatch::dispatch(
        TOOL,
        serde_json::json!({ "action": "demote", "dryRun": true }),
        &ctx("admin", true),
    )
    .await
    .expect_err("admin");
    assert!(err.to_string().contains("super-admin"), "{err:#}");
}

#[tokio::test]
async fn owner_cli_passes_without_a_second_factor() {
    let mut ctx = ctx("super-admin", false);
    ctx.set_owner_cli(true);
    let out = dispatch::dispatch(
        TOOL,
        serde_json::json!({ "action": "demote", "dryRun": true }),
        &ctx,
    )
    .await
    .expect("plan");
    assert_eq!(out["dryRun"], serde_json::json!(true), "{out}");
}

#[tokio::test]
async fn super_admin_with_a_second_factor_passes_the_gate() {
    let out = dispatch::dispatch(
        TOOL,
        serde_json::json!({ "action": "demote", "dryRun": true }),
        &ctx("super-admin", true),
    )
    .await
    .expect("plan");
    assert_eq!(out["dryRun"], serde_json::json!(true), "{out}");
}
