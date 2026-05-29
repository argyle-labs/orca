//! Inventory-slice registration for every `#[orca_tool]`-annotated function.
//!
//! The proc-macro emits one `inventory::submit!(ToolRegistration { ... })`
//! per tool, into the slice this module collects. Dispatchers in
//! `registry` walk the slice directly to build their lookup tables — no
//! central enrollment list to edit when adding a tool.

use crate::erased::ErasedTool;

/// One entry per `#[orca_tool]`-annotated function. Walking
/// `inventory::iter::<ToolRegistration>` yields every tool linked into
/// the binary; calling `make_erased()` constructs the trait-object wrapper
/// that dispatch routes through.
pub struct ToolRegistration {
    pub name: &'static str,
    pub make_erased: fn() -> Box<dyn ErasedTool>,
}

inventory::collect!(ToolRegistration);
