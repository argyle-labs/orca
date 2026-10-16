use std::sync::Arc;

use contract::BoxFuture;
use contract::unit::{
    ActionDecl, ActionOutcome, KindDeclaration, UnitDescriptor, UnitProvider, Verb, VerbArgs,
    VerbDecl, VerbOutcome,
};

use agents as _;
use auth as _;
use containers as _;
use notifications as _;
use plugins as _;
use system as _;

struct Dockerman;

impl UnitProvider for Dockerman {
    fn name(&self) -> &str {
        "csurf-unraid"
    }
    fn declarations(&self) -> Vec<KindDeclaration> {
        vec![KindDeclaration::new(
            "container",
            vec![
                VerbDecl::list(),
                VerbDecl::detail(),
                VerbDecl::update(
                    ["start", "stop", "update"]
                        .into_iter()
                        .map(ActionDecl::new)
                        .collect(),
                ),
                VerbDecl::delete(),
            ],
        )]
    }
    fn units(&self) -> BoxFuture<'_, anyhow::Result<Vec<UnitDescriptor>>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn invoke(&self, _args: VerbArgs) -> BoxFuture<'_, anyhow::Result<VerbOutcome>> {
        Box::pin(async { Ok(VerbOutcome::Action(ActionOutcome::default())) })
    }
}

#[tokio::test]
async fn reconcile_actions_and_provider_verbs_share_one_container_surface() {
    let home = tempfile::tempdir().expect("tempdir");
    // SAFETY: the only test in this binary, and set before any thread reads
    // the env.
    unsafe {
        std::env::set_var("ORCA_HOME", home.path());
        std::env::set_var("HOME", home.path());
        std::env::set_var("ORCA_DB_PATH", home.path().join("orca.db"));
    }
    contract::unit::register_provider(Arc::new(Dockerman));

    let ops = dispatch::unit_surface::unit_ops();
    let names: Vec<&str> = ops.iter().map(|o| o.name.as_str()).collect();
    for unit_op in ["container.start", "container.stop", "container.delete"] {
        assert!(names.contains(&unit_op), "{unit_op} missing: {names:?}");
    }
    for core in ["container.list", "container.detail", "container.update"] {
        assert!(!names.contains(&core), "{core} must stay core: {names:?}");
        assert!(!dispatch::unit_surface::unit_owns(core), "{core}");
    }
    assert!(
        ops.iter()
            .find(|o| o.name == "container.delete")
            .is_some_and(|o| o.verb == Verb::Delete)
    );

    let root = dispatch::cli::build_root(clap::Command::new("orca"));
    let (root, routes) = dispatch::unit_surface::register_unit_commands(root, ops);
    let container = root.find_subcommand("container").expect("container");
    let leaves: Vec<&str> = container.get_subcommands().map(|c| c.get_name()).collect();
    for leaf in ["list", "detail", "update", "start", "stop", "delete"] {
        assert!(leaves.contains(&leaf), "{leaf} missing: {leaves:?}");
    }
    assert!(routes.contains(&"container.start".to_string()));
    assert!(!routes.contains(&"container.update".to_string()));

    let cfg = Arc::new(contract::config::Config::load().expect("config"));
    let ctx = contract::ToolCtx::new(cfg);
    let err = dispatch::dispatch("container.update", serde_json::json!({}), &ctx)
        .await
        .expect_err("core handler requires an action");
    assert!(
        err.to_string()
            .contains("reconcile|reconcile_dry|unhold|unwedge"),
        "{err}"
    );
}
