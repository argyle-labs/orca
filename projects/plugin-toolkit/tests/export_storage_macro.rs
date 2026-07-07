//! Proves `export_storage_plugin!` expands and wires a real backend end to end:
//! the macro-emitted `__backends` / `__invoke` ABI fns are exercised directly
//! (the cdylib root module they also build is only reachable via `dlopen`,
//! which the plugin repos cover). One export macro per crate, so this lives in
//! its own integration-test crate.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::abi_stable::std_types::{RErr, ROk, RStr};
use plugin_toolkit::prelude::*;
use plugin_toolkit::storage::{
    Capability, MountOutcome, Share, StorageBackend, StorageError, StorageKind,
};

struct TestBackend;

#[async_trait]
impl StorageBackend for TestBackend {
    fn name(&self) -> &str {
        "test"
    }
    fn kind(&self) -> StorageKind {
        StorageKind::NetworkShare
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::List, Capability::Unmount]
    }
    fn endpoint(&self) -> String {
        "test://local".into()
    }
    async fn list_shares(&self) -> Result<Vec<Share>, StorageError> {
        Ok(vec![Share {
            id: "s1".into(),
            source: "//srv/s1".into(),
            target: None,
            fstype: "cifs".into(),
            mounted: false,
        }])
    }
    async fn unmount(&self, target: &str) -> Result<MountOutcome, StorageError> {
        Ok(MountOutcome {
            target: target.to_string(),
            mounted: false,
            recovered: false,
            detail: None,
        })
    }
}

plugin_toolkit::export_storage_plugin! {
    name: "test",
    target_compat: "any",
    backend: TestBackend,
}

#[test]
fn backends_is_derived_from_the_backend_provider() {
    let json = __backends().into_string();
    // kind/endpoint/capabilities all come from the trait, not a restated literal.
    assert!(json.contains(r#""domain":"storage""#), "{json}");
    assert!(json.contains(r#""name":"test""#), "{json}");
    assert!(json.contains(r#""kind":"network_share""#), "{json}");
    assert!(json.contains(r#""endpoint":"test://local""#), "{json}");
    assert!(
        json.contains(r#""list""#) && json.contains(r#""unmount""#),
        "{json}"
    );
    assert!(
        json.contains(r#""invoke_prefix":"storage.__backend.test""#),
        "{json}"
    );
}

#[test]
fn manifest_and_schemas_are_empty_for_a_pure_backend() {
    assert_eq!(__manifest().as_str(), "[]");
    assert_eq!(__schemas().as_str(), r#"{"namespace":"","tables":[]}"#);
}

#[test]
fn invoke_routes_storage_ops_and_rejects_foreign_names() {
    // list_shares over the storage proxy prefix.
    let r = __invoke(
        RStr::from("storage.__backend.test.list_shares"),
        RStr::from("{}"),
    );
    match r {
        ROk(s) => assert!(s.as_str().contains(r#""id":"s1""#), "{}", s.as_str()),
        RErr(e) => panic!("list_shares errored: {}", e.as_str()),
    }

    // unmount with typed args.
    let r = __invoke(
        RStr::from("storage.__backend.test.unmount"),
        RStr::from(r#"{"target":"/mnt/s1"}"#),
    );
    assert!(matches!(r, ROk(ref s) if s.as_str().contains(r#""target":"/mnt/s1""#)));

    // A name outside this plugin's prefix is refused.
    let r = __invoke(RStr::from("docker.ps"), RStr::from("{}"));
    assert!(matches!(r, RErr(_)));
}
