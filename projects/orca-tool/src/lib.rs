//! `orca-tool` — the contract + runtime for OrcaTool.
//!
//! Default feature: wasm-safe metadata traits (`OrcaToolDef`, `OrcaOp`).
//! `native` feature: full runtime (`OrcaTool`, `ToolCtx`, `ToolRegistry`,
//! `ErasedTool`, axum router, CLI dispatcher).
//!
//! Every crate that defines `#[orca_tool]`-annotated functions depends on
//! this crate. There is no separate "tool-trait" crate — one home.

mod def;
pub use def::{OrcaOp, OrcaToolDef};

#[cfg(feature = "native")]
mod ctx;
#[cfg(feature = "native")]
mod erased;
#[cfg(feature = "native")]
mod registry;
#[cfg(feature = "native")]
mod tool;
#[cfg(feature = "native")]
mod types;

#[cfg(feature = "native")]
pub use ctx::ToolCtx;
#[cfg(feature = "native")]
pub use erased::{ErasedTool, ToolWrapper, value_to_text};
#[cfg(feature = "native")]
pub use registry::{CliArgs, ToolRegistry};
#[cfg(feature = "native")]
pub use tool::OrcaTool;
#[cfg(feature = "native")]
pub use types::{ToolCall, ToolDef, ToolResult};
