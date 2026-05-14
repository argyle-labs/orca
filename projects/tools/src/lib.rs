//! Native tool enrollment shim — forwards to `orca_tools_def::native_register`.
//!
//! Tools are declared via `#[orca_tool]` annotations in `orca-tools-def` and
//! collected through the `inventory` slice. This crate exists so server code
//! can write `orca_tools::register_all(reg)` without knowing about feature flags.

pub fn register_all(reg: &mut orca_utils::tool::ToolRegistry) {
    orca_tools_def::native_register(reg);
}
