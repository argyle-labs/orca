//! Native tool enrollment shim — forwards to `orca_tools_def::native_register`.
//!
//! The canonical tool list lives in `orca-tools-def::lib.rs`'s
//! `declare_tools!{}` block. This crate exists so server code can write
//! `orca_tools::register_all(reg)` without knowing about feature flags.

pub fn register_all(reg: &mut orca_utils::tool::ToolRegistry) {
    orca_tools_def::native_register(reg);
}
