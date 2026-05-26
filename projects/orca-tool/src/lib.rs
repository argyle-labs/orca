//! `orca-tool` — native runtime for OrcaTool.
//!
//! The wasm-safe contract (metadata traits, error, JsonAny, protocol types,
//! ToolCtx/OrcaTool/RemoteExec trait anchors) lives in `orca-contract`.
//! This crate adds: the `ToolRegistry` (axum router + MCP/CLI dispatch),
//! the `ErasedTool` object-safe wrapper, the `inventory` slice
//! (`ToolRegistration` + `native_register`), the `openapi` spec injector,
//! and (under `cli`) the unified clap-driven CLI surface (`register_op!`
//! macro + `CliOp` inventory + dispatcher).
//!
//! Every crate that defines `#[orca_tool]`-annotated functions depends on
//! this crate. The registration framework lives here (not in `tools-def`)
//! so content crates carrying their own tools have no cycle through any
//! upstream "all the tools" crate.

pub mod openapi;

#[cfg(feature = "native")]
mod erased;
#[cfg(feature = "native")]
mod inventory_slice;
#[cfg(feature = "native")]
mod registry;

#[cfg(feature = "native")]
pub use erased::{ErasedTool, ToolWrapper, value_to_text};
#[cfg(feature = "native")]
pub use inventory_slice::{ToolRegistration, native_register};
#[cfg(feature = "native")]
pub use registry::{CliArgs, ToolRegistry};

#[cfg(feature = "cli")]
pub mod cli;
