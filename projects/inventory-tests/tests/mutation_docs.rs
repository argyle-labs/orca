//! Conformance test for the `[MUTATES STATE]` doc marker.
//!
//! Lives here, not in `dispatch`, for the same reason as `execute_gate.rs`:
//! `dispatch`'s own test binary links no buckets, so anything iterating
//! `data_mutation_names()` there passes vacuously.

use dispatch::ToolRegistration;

// Side-effect imports — link the buckets so their inventory::submit! statics
// are pulled into this test binary. This list must cover every crate that
// hosts an #[orca_tool]; anything missing is silently unguarded.
use agents as _;
use auth as _;
use config_source as _;
use containers as _;
use conversation as _;
use db as _;
use dev as _;
use files as _;
use mcp as _;
use model as _;
use namespace as _;
use notifications as _;
use orca_inventory as _;
use plugin_toolkit as _;
use plugins as _;
use spec as _;
use system as _;

const MARKER: &str = "[MUTATES STATE] ";

/// Verbs that mutate but deliberately do not carry the marker.
///
/// Each entry needs a written reason. The list is not a parking lot for verbs
/// nobody got around to documenting — that is the defect this test exists to
/// catch.
const ALLOWLIST: &[(&str, &str)] = &[
    // `auth.login` writes a session row, but the marker is addressed to an
    // operator deciding whether a call is safe to make, and refusing to sign
    // in is not the decision it is meant to provoke.
    ("auth.login", "signing in is not an operator-visible change"),
];

/// The marker is the only signal in the tool description that a verb applies
/// changes — the OpenAPI renderer strips it into the "mutates" affordance and
/// the CLI/MCP help text carries it verbatim. A mutating verb without it reads
/// as a read in every surface that shows a description.
#[test]
fn every_data_mutation_verb_documents_itself_as_mutating() {
    let mutations = dispatch::data_mutation_names();
    assert!(
        !mutations.is_empty(),
        "no tool is data_mutation — the inventory is not linked and this test \
         would pass vacuously"
    );

    let mut missing: Vec<&str> = Vec::new();
    for entry in inventory::iter::<ToolRegistration> {
        if !mutations.contains(&entry.name) {
            continue;
        }
        if ALLOWLIST.iter().any(|(n, _)| *n == entry.name) {
            continue;
        }
        // A verb with no doc comment gets its own name as the description.
        let description = (entry.make_erased)().description();
        if !description.starts_with(MARKER) {
            missing.push(entry.name);
        }
    }
    missing.sort_unstable();
    assert!(
        missing.is_empty(),
        "{} data-mutation verbs do not start their doc comment with `{MARKER}`: {missing:#?}",
        missing.len()
    );
}

/// An allowlist entry for a verb that is no longer a mutation (or no longer
/// exists) is stale, and a stale exception silently excuses the next verb that
/// reuses the name.
#[test]
fn the_allowlist_has_no_stale_entries() {
    let mutations = dispatch::data_mutation_names();
    for (name, _why) in ALLOWLIST {
        assert!(
            mutations.contains(name),
            "allowlisted `{name}` is not a data_mutation verb — drop the entry"
        );
    }
}
