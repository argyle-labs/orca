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
/// The four legacy ad-hoc gates are deliberately not yet `execute_gated`:
/// `system.update` and `plugin.update` (`execute`),
/// `storage.share.repair-permissions` (`apply`), `storage.mount.create`
/// (`force`). They migrate onto the shared field separately.
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
