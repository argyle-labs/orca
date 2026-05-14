//! Wasm-safe definitions for OrcaTool — metadata + Args/Output types only.
//!
//! Every tool is annotated with `#[orca_tool(domain = "...", verb = "...")]`
//! in its module. The proc-macro emits, in the same crate as the function:
//!   - The ZST + `OrcaToolDef` + `OrcaOp` impls (unconditional, so wasm builds
//!     keep their typed `OrcaClient` methods).
//!   - `#[cfg(feature = "native")]` `OrcaTool::run` thunk + an
//!     `inventory::submit!` into the `ORCA_TOOLS` slice.
//!   - `#[cfg(feature = "wasm")]` a typed `OrcaClient::<fn_ident>` method.
//!   - `#[cfg(feature = "cli")]` a `register_op!` CLI entry (skippable via
//!     `cli = manual` / `cli = skip`).
//!
//! `native_register` walks `inventory::iter::<ToolRegistration>` to populate
//! the `ToolRegistry` that drives MCP + REST + CLI at startup. Adding a tool
//! is one `#[orca_tool]` annotation — no central enrollment list to edit.

// The `#[orca_tool]` proc-macro emits absolute paths like
// `::orca_tools_def::OrcaToolDef`. Inside this crate the implicit name is
// `crate`, so add a self-alias to resolve those absolute paths during
// in-crate macro invocations.
extern crate self as orca_tools_def;

pub use orca_tool_trait::{OrcaOp, OrcaToolDef};

/// `#[orca_tool(domain = "...", verb = "...")]` proc-macro re-export.
///
/// Tools annotated with `#[orca_tool]` flow into the `ToolRegistration`
/// inventory slice and are picked up by `native_register` at startup.
pub use orca_tools_macro::orca_tool;

/// One entry per `#[orca_tool]`-annotated function. The native registry
/// walks `inventory::iter::<ToolRegistration>` to enroll them all.
/// `register` takes `&mut ToolRegistry` boxed behind the `__private`
/// re-export so this struct stays defined unconditionally — wasm builds
/// carry the type but never register anything.
pub struct ToolRegistration {
    pub name: &'static str,
    #[cfg(feature = "native")]
    pub register: fn(&mut __private::ToolRegistry),
}

inventory::collect!(ToolRegistration);

pub mod agent_backend;
pub mod agents;
pub mod config;
pub mod docker;
pub mod docs;
pub mod engine;
pub mod homeassistant;
pub mod host;
pub mod infra;
pub mod json_any;
pub mod meta;
pub mod mgmt;
pub mod orca_auth;
pub mod orca_db;
pub mod orca_lifecycle;
pub mod orca_pki;
pub mod orca_profile;
pub mod orca_secrets;
pub mod plugin_runtime;
pub mod plugins;
pub mod pod;
pub mod proxmox;
pub mod services;
pub mod spec_registry;
pub mod system;

/// Re-export of the opaque JSON passthrough wrapper — see `json_any` module for policy.
#[allow(clippy::disallowed_types)]
pub use json_any::JsonAny;

#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(feature = "cli")]
pub mod cli;

#[doc(hidden)]
#[cfg(feature = "native")]
pub mod __private {
    pub use orca_utils::tool::ToolRegistry;
}

/// Walk the `inventory::iter::<ToolRegistration>` slice — populated by every
/// `#[orca_tool]` annotation in this crate — and enroll each tool into the
/// supplied `ToolRegistry`. Drives MCP + REST + CLI surface registration at
/// startup. `ToolRegistry::register` panics on duplicates so name collisions
/// surface immediately.
#[cfg(feature = "native")]
pub fn native_register(reg: &mut __private::ToolRegistry) {
    for entry in inventory::iter::<ToolRegistration> {
        (entry.register)(reg);
    }
}

#[cfg(all(test, feature = "native"))]
mod inventory_tests {
    //! Inventory-slice smoke test for the `#[orca_tool]` proof-of-shape.
    //! Asserts that the three migrated host tools land in `ORCA_TOOLS` and
    //! that `native_register` enrolls them into a `ToolRegistry`.
    use super::*;

    #[test]
    fn host_tools_present_in_inventory_slice() {
        let names: Vec<&'static str> = inventory::iter::<ToolRegistration>
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&"host.info"), "missing host.info: {names:?}");
        assert!(names.contains(&"host.set"), "missing host.set: {names:?}");
        assert!(
            names.contains(&"host.refresh"),
            "missing host.refresh: {names:?}"
        );
    }

    #[test]
    fn native_register_enrolls_host_tools() {
        let mut reg = __private::ToolRegistry::new();
        native_register(&mut reg);
        let names = reg.names();
        assert!(names.contains(&"host.info"));
        assert!(names.contains(&"host.set"));
        assert!(names.contains(&"host.refresh"));
    }

    #[test]
    fn pod_tools_present_in_inventory_slice() {
        let names: Vec<&'static str> = inventory::iter::<ToolRegistration>
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&"pod.list"), "missing pod.list: {names:?}");
        assert!(
            names.contains(&"pod.accept"),
            "missing pod.accept: {names:?}"
        );
    }

    #[test]
    fn inventory_slice_has_full_migrated_set() {
        let count = inventory::iter::<ToolRegistration>.into_iter().count();
        assert!(
            count >= 128,
            "expected >=128 tools in inventory slice, got {count}",
        );
    }
}
