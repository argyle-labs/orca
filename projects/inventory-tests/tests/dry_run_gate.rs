//! Conformance tests for the dry-run gate — execute by default, `dryRun`
//! opt-in (orca#636, #877).
//!
//! These live here, not in `dispatch`, for a reason worth stating: `dispatch`'s
//! own test binary links no buckets, so `execute_gated_names()` is EMPTY there
//! and any assertion iterating it passes vacuously. This crate links every
//! bucket, so the assertions below run against the real registry.

use dispatch::ToolRegistration;

// Side-effect imports — link the buckets so their inventory::submit! statics
// are pulled into this test binary.
use agents as _;
use auth as _;
use files as _;
use model as _;
use notifications as _;
use orca_inventory as _;
use plugins as _;
use system as _;

/// The gate is useless if nothing is gated. This guards against the exact
/// failure mode that made the dispatch-level tests meaningless: a conformance
/// suite that iterates an empty set and reports success.
#[test]
fn the_gated_set_is_not_empty() {
    let gated = dispatch::execute_gated_names();
    assert!(
        !gated.is_empty(),
        "no tool is execute_gated — either the inventory is not linked or the \
         attribute stopped being emitted; every assertion in this file would \
         otherwise pass vacuously"
    );
}

/// The destructive backup verbs must be gated. `backup.restore` overwrites live
/// app data in place, `backup.run` writes to datastores — neither should ever
/// apply on an un-opted-in call.
#[test]
fn destructive_backup_verbs_are_gated() {
    let gated = dispatch::execute_gated_names();
    for must in ["backup.run", "backup.restore"] {
        assert!(
            gated.contains(&must),
            "{must} is not execute_gated; gated set = {gated:?}"
        );
    }
}

/// `system.profile` changes how a plugin's next process is launched
/// (`MALLOC_CONF` injection), so a read-only identity must not reach it.
/// `identity.privilege.audit` only reads sudo/doas grants off disk, but the list
/// is sensitive, so it requires the `read` role rather than `any`.
#[test]
fn profile_is_a_gated_admin_write_and_privilege_audit_is_a_read() {
    let gated = dispatch::execute_gated_names();
    let mutations = dispatch::data_mutation_names();

    assert!(gated.contains(&"system.profile"), "gated set = {gated:?}");
    assert!(!mutations.contains(&"system.profile"));
    assert_eq!(dispatch::required_role("system.profile"), Some("admin"));

    assert!(!gated.contains(&"identity.privilege.audit"));
    assert!(!mutations.contains(&"identity.privilege.audit"));
    assert_eq!(
        dispatch::required_role("identity.privilege.audit"),
        Some("read")
    );
}

/// Crontabs routinely carry secrets, so even reading one is admin-only, and
/// none of the verbs is a data_mutation, so `can_mutate` never reaches them.
/// Only `update` applies changes.
#[test]
fn cron_verbs_are_admin_and_only_update_is_gated() {
    let gated = dispatch::execute_gated_names();
    for tool in ["system.cron.list", "system.cron.diff", "system.cron.update"] {
        assert_eq!(dispatch::required_role(tool), Some("admin"), "{tool}");
    }
    let mutations = dispatch::data_mutation_names();
    for tool in ["system.cron.list", "system.cron.diff", "system.cron.update"] {
        assert!(
            !mutations.contains(&tool),
            "{tool} must not be a data_mutation"
        );
        assert!(
            !dispatch::tool_roles::authorize(
                "member",
                true,
                dispatch::required_role(tool).unwrap_or("any"),
                mutations.contains(&tool),
            ),
            "{tool}: a non-admin with can_mutate must be refused"
        );
    }
    assert!(gated.contains(&"system.cron.update"));
    assert!(!gated.contains(&"system.cron.list"));
    assert!(!gated.contains(&"system.cron.diff"));
}

/// `model.backends_check` only probes reachability, so it is a read; it needs
/// the `read` role because it lists every configured backend endpoint.
#[test]
fn model_backends_check_is_an_ungated_read() {
    assert!(!dispatch::execute_gated_names().contains(&"model.backends_check"));
    assert!(!dispatch::data_mutation_names().contains(&"model.backends_check"));
    assert_eq!(
        dispatch::required_role("model.backends_check"),
        Some("read")
    );
}

/// Core verbs a non-admin `can_mutate` caller may invoke. `data_mutation` is
/// opt-in, so anything else that writes stays admin-only.
const ALLOWED_DATA_MUTATIONS: &[&str] = &[
    "backup.check",
    "backup.restore",
    "backup.run",
    "backup.sync",
    "storage.share.repair-permissions",
];

/// A verb that applies changes must be restricted in WHO may call it — its
/// default execute and its dry run alike.
#[test]
fn every_gated_verb_is_admin_only() {
    for name in dispatch::execute_gated_names() {
        assert_eq!(
            dispatch::required_role(name),
            Some("admin"),
            "{name} is execute_gated but not role=admin — mutation without authorization"
        );
    }
}

#[test]
fn only_allowed_verbs_are_data_mutations() {
    for name in dispatch::data_mutation_names() {
        assert!(
            ALLOWED_DATA_MUTATIONS.contains(&name),
            "`{name}` is a data_mutation, so a non-admin `can_mutate` token can call it"
        );
    }
}

/// The gate injects `dryRun` and the deprecated `execute` into a gated verb's
/// advertised schema and strips both before typed deserialization. A verb
/// declaring its OWN field of either name would have it silently swallowed, so
/// fail loudly at test time instead.
///
/// `system.update`, `plugin.update` and `storage.share.repair-permissions` are
/// deliberately NOT `execute_gated`: each already implements dry-run by default
/// and returns a richer answer than `ExecutionPlan::generic` could, so gating
/// them would replace a real plan with an empty one. They still spell the
/// opt-in `execute`. `plugin.create` gates only `action=install` itself, since
/// `invoke` reaches plugin reads and plugins that gate their own verbs. `storage.mount.create`'s `force` is NOT a dry-run gate at
/// all — it overrides the multi-mount collision guard, a different concept that
/// must not be folded into this one.
#[test]
fn no_gated_verb_declares_its_own_gate_field() {
    let gated = dispatch::execute_gated_names();
    for entry in inventory::iter::<ToolRegistration> {
        if !gated.contains(&entry.name) {
            continue;
        }
        let schema = (entry.make_erased)().input_schema();
        for (field, marker) in [("dryRun", "ExecutionPlan"), ("execute", "Deprecated no-op")] {
            let desc = schema["properties"][field]["description"]
                .as_str()
                .unwrap_or_default();
            assert!(
                desc.contains(marker),
                "{} appears to declare its own `{field}` field — the gate would \
                 swallow it. Migrate the verb or drop execute_gated.",
                entry.name
            );
        }
    }
}

/// Every gated verb advertises `dryRun`, so callers can discover it from the
/// schema rather than from documentation or source.
#[test]
fn every_gated_verb_advertises_the_opt_in() {
    let gated = dispatch::execute_gated_names();
    for entry in inventory::iter::<ToolRegistration> {
        if !gated.contains(&entry.name) {
            continue;
        }
        let schema = (entry.make_erased)().input_schema();
        assert_eq!(
            schema["properties"]["dryRun"]["type"],
            serde_json::json!("boolean"),
            "{} does not advertise a `dryRun` boolean: {schema}",
            entry.name
        );
    }
}

/// A gated verb invoked with `dryRun: true` must change nothing and say what
/// it would have done.
///
/// The schema assertions above prove the gate is advertised; only this proves
/// it is enforced. They are different claims, and the first passing while the
/// second failed is precisely how a gate becomes decorative.
#[tokio::test]
async fn a_gated_verb_called_with_dry_run_returns_a_plan_and_applies_nothing() {
    let cfg = std::sync::Arc::new(contract::config::Config::load().expect("config"));
    let ctx = contract::ToolCtx::new(cfg);

    // `config.delete` is representative: write-shaped, no bespoke dry-run, and
    // harmless to plan against a row that does not exist.
    let out = dispatch::dispatch(
        "config.delete",
        serde_json::json!({ "noun": "orca-gate-probe", "name": "no-such-row", "dryRun": true }),
        &ctx,
    )
    .await
    .expect("dispatch succeeds");

    assert_eq!(
        out["dryRun"],
        serde_json::json!(true),
        "a gated verb called with dryRun must report a dry run: {out}"
    );
    assert_eq!(out["tool"], serde_json::json!("config.delete"));
    assert!(
        out.get("removed").is_none(),
        "the verb body must not have run: {out}"
    );
    // Honest about what it does NOT know: an empty change list is not a claim
    // that nothing would change.
    assert_eq!(out["detailed"], serde_json::json!(false));
    assert!(
        out["howToExecute"]
            .as_str()
            .unwrap_or_default()
            .contains("without `dryRun`"),
        "the plan says how to apply it: {out}"
    );
}

/// Every gated verb on the CLI offers `--dry-run` to plan, applies on a bare
/// call, and still accepts `--execute` as a hidden no-op.
#[test]
fn every_gated_verb_offers_dry_run_on_the_cli() {
    let root = dispatch::cli::build_root(clap::Command::new("orca"));

    let mut checked = 0usize;
    for name in dispatch::execute_gated_names() {
        let Some(cmd) = find_leaf(&root, name) else {
            // Not every tool is on the CLI tree (plugin verbs, aliases).
            continue;
        };
        let dry_run = cmd
            .get_arguments()
            .find(|a| a.get_long() == Some(dispatch::cli::DRY_RUN_FLAG))
            .unwrap_or_else(|| panic!("gated verb `{name}` has no --dry-run flag"));
        assert_eq!(dry_run.get_id(), contract::plan::DRY_RUN_FIELD, "{name}");
        let execute = cmd
            .get_arguments()
            .find(|a| a.get_long() == Some(contract::plan::EXECUTE_FIELD))
            .unwrap_or_else(|| panic!("gated verb `{name}` dropped the --execute no-op"));
        assert!(execute.is_hide_set(), "{name}: --execute must be hidden");
        checked += 1;
    }
    assert!(checked > 0, "no gated verbs were reachable on the CLI tree");
}

/// The flag the CLI run path reads: `--dry-run` plans, a bare call or
/// `--execute` applies.
#[test]
fn the_dry_run_flag_selects_the_plan_and_execute_does_not() {
    let root = dispatch::cli::build_root(clap::Command::new("orca"));
    let leaf = dispatch::execute_gated_names()
        .into_iter()
        .find_map(|n| find_leaf(&root, n))
        .expect("a gated verb on the CLI tree");
    for (flags, want) in [
        (&["--dry-run"][..], true),
        (&[][..], false),
        (&["--execute"][..], false),
    ] {
        let m = leaf
            .clone()
            .ignore_errors(true)
            .try_get_matches_from(std::iter::once("verb").chain(flags.iter().copied()))
            .unwrap_or_else(|e| panic!("{flags:?}: {e}"));
        assert_eq!(m.get_flag(contract::plan::DRY_RUN_FIELD), want, "{flags:?}");
    }
}

#[test]
fn every_dry_run_aware_verb_offers_dry_run_on_the_cli() {
    let root = dispatch::cli::build_root(clap::Command::new("orca"));
    let aware = dispatch::dry_run_aware_names();
    for name in ["storage.export.update", "storage.export.delete"] {
        assert!(aware.contains(&name), "{name} is not dry_run_aware");
    }
    let mut checked = 0;
    for name in aware {
        let Some(leaf) = find_leaf(&root, name) else {
            continue;
        };
        let m = leaf
            .clone()
            .ignore_errors(true)
            .try_get_matches_from(["verb", "--dry-run"])
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(m.get_flag(contract::plan::DRY_RUN_FIELD), "{name}");
        checked += 1;
    }
    assert!(
        checked > 0,
        "no dry-run-aware verbs were reachable on the CLI tree"
    );
    let base = ["verb", "--provider", "p", "--path", "/x", "--dry-run"];
    for (name, extra) in [
        ("storage.export.update", &["--client", "10.0.0.1"][..]),
        ("storage.export.delete", &[][..]),
    ] {
        let m = find_leaf(&root, name)
            .unwrap_or_else(|| panic!("{name} missing from the CLI tree"))
            .clone()
            .try_get_matches_from(base.iter().chain(extra))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(m.get_flag(contract::plan::DRY_RUN_FIELD), "{name}");
    }
}

/// Walk `domain.sub.verb` down the command tree.
fn find_leaf<'a>(root: &'a clap::Command, tool: &str) -> Option<&'a clap::Command> {
    let mut cur = root;
    for seg in tool.split('.') {
        cur = cur.get_subcommands().find(|c| c.get_name() == seg)?;
    }
    Some(cur)
}

/// A plugin's manifest is how the daemon learns its tools' roles (orca#763):
/// every entry must carry the same role, gate and mutation flags the registry
/// enforces in-process.
#[test]
fn the_plugin_manifest_carries_each_tools_policy() {
    let manifest: Vec<plugin_toolkit::abi::ToolDef> =
        serde_json::from_str(&dispatch::tool_manifest_json()).expect("manifest parses");
    assert!(!manifest.is_empty());
    let gated = dispatch::execute_gated_names();
    let mutations = dispatch::data_mutation_names();
    let local_only = dispatch::local_only_names();
    assert!(manifest.iter().any(|e| e.local_only == Some(true)));
    for e in &manifest {
        let name = e.name.as_str();
        assert_eq!(e.role.as_deref(), dispatch::required_role(name), "{name}");
        assert_eq!(e.execute_gated, Some(gated.contains(&name)), "{name}");
        assert_eq!(e.data_mutation, Some(mutations.contains(&name)), "{name}");
        assert_eq!(e.local_only, Some(local_only.contains(&name)), "{name}");
    }
}

/// Dry-run-aware tools that forward no apply opt-in a pre-`dryRun` peer could
/// act on, so they need no `dry_run_neutralize` hook. Adding a name here is a
/// claim that a peer ignoring `dryRun` still changes nothing.
const AWARE_WITHOUT_NEUTRALIZE: &[&str] = &[];

/// A dry-run-aware tool runs its own body under a dry run, so a peer that
/// predates `dryRun` would apply unless the forwarded args are neutralized.
#[test]
fn every_dry_run_aware_tool_neutralizes_its_forwarded_args() {
    let aware = dispatch::dry_run_aware_names();
    assert!(!aware.is_empty(), "no tool is dry_run_aware");
    let neutralized = dispatch::dry_run_neutralized_names();
    for name in aware {
        assert!(
            neutralized.contains(&name) != AWARE_WITHOUT_NEUTRALIZE.contains(&name),
            "{name} is dry_run_aware: give it a dry_run_neutralize hook, or list it in \
             AWARE_WITHOUT_NEUTRALIZE if its forwarded args cannot apply anything"
        );
    }
}
