//! Conformance tests for the execute gate — dry-run by default (orca#636).
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

/// Consent without authorization is not a gate: if a verb applies changes, it
/// must also be restricted in WHO may call it, or any authenticated caller can
/// apply changes simply by passing `execute: true`.
#[test]
fn every_gated_verb_is_also_an_admin_data_mutation() {
    let mutations = dispatch::data_mutation_names();
    for name in dispatch::execute_gated_names() {
        assert_eq!(
            dispatch::required_role(name),
            Some("admin"),
            "{name} is execute_gated but not role=admin — consent without authorization"
        );
        assert!(
            mutations.contains(&name),
            "{name} is execute_gated but not data_mutation — the two must agree"
        );
    }
}

/// The gate injects `execute` into a gated verb's advertised schema and strips
/// it before typed deserialization. A verb declaring its OWN `execute` field
/// would have it silently swallowed, so fail loudly at test time instead.
///
/// `system.update`, `plugin.update` and `storage.share.repair-permissions` are
/// deliberately NOT `execute_gated`: each already implements dry-run by default
/// and returns a richer answer than `ExecutionPlan::generic` could, so gating
/// them would replace a real plan with an empty one. They still spell the
/// opt-in `execute`. `storage.mount.create`'s `force` is NOT a dry-run gate at
/// all — it overrides the multi-mount collision guard, a different concept that
/// must not be folded into this one.
#[test]
fn no_gated_verb_declares_its_own_execute_field() {
    let gated = dispatch::execute_gated_names();
    for entry in inventory::iter::<ToolRegistration> {
        if !gated.contains(&entry.name) {
            continue;
        }
        let schema = (entry.make_erased)().input_schema();
        let desc = schema["properties"]["execute"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            desc.contains("ExecutionPlan"),
            "{} appears to declare its own `execute` field — the gate would \
             swallow it. Migrate the verb or drop execute_gated.",
            entry.name
        );
    }
}

/// Every gated verb advertises the opt-in, so callers can discover it from the
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
            schema["properties"]["execute"]["type"],
            serde_json::json!("boolean"),
            "{} does not advertise an `execute` boolean: {schema}",
            entry.name
        );
    }
}

/// The invariant the whole phase exists for: a gated verb invoked WITHOUT the
/// opt-in must change nothing and say what it would have done.
///
/// The schema assertions above prove the gate is advertised; only this proves
/// it is enforced. They are different claims, and the first passing while the
/// second failed is precisely how a gate becomes decorative.
#[tokio::test]
async fn a_gated_verb_called_without_the_opt_in_returns_a_plan_and_applies_nothing() {
    let cfg = std::sync::Arc::new(contract::config::Config::load().expect("config"));
    let ctx = contract::ToolCtx::new(cfg);

    // `config.delete` is representative: write-shaped, no bespoke dry-run, and
    // harmless to plan against a row that does not exist.
    let out = dispatch::dispatch(
        "config.delete",
        serde_json::json!({ "noun": "orca-gate-probe", "name": "no-such-row" }),
        &ctx,
    )
    .await
    .expect("dispatch succeeds");

    assert_eq!(
        out["dryRun"],
        serde_json::json!(true),
        "a gated verb with no opt-in must report a dry run: {out}"
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
            .contains("execute"),
        "the plan says how to apply it: {out}"
    );
}

/// The gate is only real if an operator can satisfy it. rc.8 shipped with the
/// opt-in advertised in the JSON schema and NO way to pass it from the CLI, so
/// every gated verb planned, returned an `ExecutionPlan`, and the CLI failed to
/// decode it as the verb's own `Output`:
///
/// ```text
/// Error: decode storage.mount.update output: data did not match any variant
/// of untagged enum StorageMountUpdateOutput
/// ```
///
/// A completed-looking command that changed nothing, on every mutating verb,
/// across the whole fleet (#665).
#[test]
fn every_gated_verb_can_actually_be_executed_from_the_cli() {
    let root = dispatch::cli::build_root(clap::Command::new("orca"));

    let mut checked = 0usize;
    for name in dispatch::execute_gated_names() {
        let Some(cmd) = find_leaf(&root, name) else {
            // Not every tool is on the CLI tree (plugin verbs, aliases).
            continue;
        };
        assert!(
            cmd.get_arguments()
                .any(|a| a.get_long() == Some(contract::plan::EXECUTE_FIELD)),
            "gated verb `{name}` has no --execute flag, so its gate cannot be satisfied"
        );
        checked += 1;
    }
    assert!(checked > 0, "no gated verbs were reachable on the CLI tree");
}

/// Walk `domain.sub.verb` down the command tree.
fn find_leaf<'a>(root: &'a clap::Command, tool: &str) -> Option<&'a clap::Command> {
    let mut cur = root;
    for seg in tool.split('.') {
        cur = cur.get_subcommands().find(|c| c.get_name() == seg)?;
    }
    Some(cur)
}
