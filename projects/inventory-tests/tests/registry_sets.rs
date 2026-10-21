use std::collections::HashSet;

use agents as _;
use auth as _;
use files as _;
use model as _;
use notifications as _;
use orca_inventory as _;
use plugins as _;
use system as _;

fn all_names() -> HashSet<&'static str> {
    let names: HashSet<&'static str> = dispatch::names().into_iter().collect();
    assert!(
        !names.is_empty(),
        "no tools linked; every assertion here would be vacuous"
    );
    names
}

#[test]
fn execute_gated_names_are_a_subset_of_names() {
    let all = all_names();
    let gated = dispatch::execute_gated_names();
    assert!(!gated.is_empty());
    for n in gated {
        assert!(all.contains(n), "execute_gated name {n} not in names()");
    }
}

#[test]
fn remote_ok_names_are_a_subset_of_names() {
    let all = all_names();
    let remote = dispatch::remote_ok_names();
    assert!(!remote.is_empty());
    for n in remote {
        assert!(all.contains(n), "remote_ok name {n} not in names()");
    }
}

#[test]
fn local_only_and_remote_ok_partition_names() {
    let all = all_names();
    let remote: HashSet<_> = dispatch::remote_ok_names().into_iter().collect();
    let local: HashSet<_> = dispatch::local_only_names().into_iter().collect();
    assert!(!local.is_empty());
    assert!(remote.is_disjoint(&local));
    assert_eq!(remote.union(&local).count(), all.len());
}
