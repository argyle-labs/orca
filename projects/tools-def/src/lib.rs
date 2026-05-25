//! Definitions for OrcaTool — metadata + Args/Output types.
//!
//! Every tool is annotated with `#[orca_tool(domain = "...", verb = "...")]`
//! in its module. The proc-macro emits, in the same crate as the function:
//!   - The ZST + `OrcaToolDef` + `OrcaOp` impls.
//!   - `#[cfg(feature = "native")]` `OrcaTool::run` thunk + an
//!     `inventory::submit!` into the `ORCA_TOOLS` slice.
//!   - An `OpenApiToolRegistration` inventory entry so the spec endpoint can
//!     hoist every tool path automatically.
//!   - `#[cfg(feature = "cli")]` a `register_op!` CLI entry (skippable via
//!     `cli = manual` / `cli = skip`).
//!
//! The registration framework itself (`ToolRegistration` inventory slice,
//! `native_register`, `OpenApiToolRegistration`, the `register_op!` macro,
//! `JsonAny`) lives in `orca-tool` so any crate can define its own tools
//! without depending on this "kitchen-sink" crate. This crate is now just
//! one of many content crates that carry `#[orca_tool]` annotations — the
//! `tools/schedule.rs` migration into `db` is the proof-of-shape for that.

// The `#[orca_tool]` proc-macro emits absolute paths like
// `::orca_tools_def::ToolRegistration`. Inside this crate the implicit name
// is `crate`, so add a self-alias to resolve those absolute paths during
// in-crate macro invocations.
extern crate self as orca_tools_def;

pub use orca_tool::{OrcaOp, OrcaToolDef};

/// `#[orca_tool(domain = "...", verb = "...")]` proc-macro re-export.
///
/// Tools annotated with `#[orca_tool]` flow into the `ToolRegistration`
/// inventory slice and are picked up by `native_register` at startup.
pub use orca_tools_macro::orca_tool;

// ── Back-compat re-exports of the registration framework ────────────────────
//
// The framework lives in `orca-tool`; these re-exports keep existing macro
// invocations and server consumers (`orca_tools_def::native_register`,
// `orca_tools_def::openapi::inject_tool_paths`, etc.) working unchanged.

#[allow(clippy::disallowed_types)]
// Re-export of the escape hatch itself for downstream tools (homeassistant, proxmox) with genuinely free-form upstream payloads.
pub use orca_tool::JsonAny;
pub use orca_tool::openapi;

#[cfg(feature = "native")]
pub use orca_tool::{ToolRegistration, native_register};

#[cfg(feature = "cli")]
pub use orca_tool::cli;
#[cfg(feature = "cli")]
pub use orca_tool::register_op;

pub mod agent_backend;
pub mod agents;
pub mod config;
pub mod docker;
pub mod docs;
pub mod engine;
pub mod homeassistant;
pub mod host;
pub mod host_status;
pub mod infra;
pub mod json_schema;
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
pub mod sweep;
pub mod system;

#[cfg(all(test, feature = "native"))]
pub(crate) mod test_support;

#[cfg(all(test, feature = "native"))]
mod inventory_tests {
    //! Inventory-slice smoke test for the `#[orca_tool]` proof-of-shape.
    //! Asserts that the migrated host + pod tools land in the
    //! `ToolRegistration` slice and that `native_register` enrolls them
    //! into a `ToolRegistry`.
    use super::*;

    #[test]
    fn host_tools_present_in_inventory_slice() {
        let names: Vec<&'static str> = inventory::iter::<ToolRegistration>
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(
            names.contains(&"system.host.detail"),
            "missing system.host.detail: {names:?}"
        );
        assert!(
            names.contains(&"system.host.set"),
            "missing system.host.set: {names:?}"
        );
        assert!(
            names.contains(&"system.host.refresh"),
            "missing system.host.refresh: {names:?}"
        );
    }

    #[test]
    fn native_register_enrolls_host_tools() {
        let mut reg = orca_tool::ToolRegistry::new();
        native_register(&mut reg);
        let names = reg.names();
        assert!(names.contains(&"system.host.detail"));
        assert!(names.contains(&"system.host.set"));
        assert!(names.contains(&"system.host.refresh"));
    }

    #[test]
    fn pod_tools_present_in_inventory_slice() {
        let names: Vec<&'static str> = inventory::iter::<ToolRegistration>
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(
            names.contains(&"system.peer.list"),
            "missing system.peer.list: {names:?}"
        );
        assert!(
            names.contains(&"system.peer.create"),
            "missing system.peer.create: {names:?}"
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
