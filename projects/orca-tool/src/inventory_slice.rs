//! Inventory-slice registration for every `#[orca_tool]`-annotated function.
//!
//! The proc-macro emits one `inventory::submit!(ToolRegistration { ... })`
//! per tool, into the slice this module collects. `native_register` walks
//! the slice at startup to populate a `ToolRegistry` — no central
//! enrollment list to edit when adding a tool.
//!
//! Lives in `orca-tool` (not `tools-def`) so any crate that defines tools
//! can depend on `orca-tool` alone — no cycle through `tools-def`.

use crate::registry::ToolRegistry;

/// One entry per `#[orca_tool]`-annotated function. The native registry
/// walks `inventory::iter::<ToolRegistration>` to enroll them all.
pub struct ToolRegistration {
    pub name: &'static str,
    pub register: fn(&mut ToolRegistry),
}

inventory::collect!(ToolRegistration);

/// Walk the `inventory::iter::<ToolRegistration>` slice — populated by every
/// `#[orca_tool]` annotation across all linked crates — and enroll each tool
/// into the supplied `ToolRegistry`. Drives MCP + REST + CLI surface
/// registration at startup. `ToolRegistry::register` panics on duplicates so
/// name collisions surface immediately.
pub fn native_register(reg: &mut ToolRegistry) {
    for entry in inventory::iter::<ToolRegistration> {
        (entry.register)(reg);
    }
}
