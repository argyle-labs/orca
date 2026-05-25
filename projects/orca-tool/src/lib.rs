//! `orca-tool` — the contract + runtime for OrcaTool.
//!
//! Default feature: wasm-safe metadata traits (`OrcaToolDef`, `OrcaOp`),
//! `JsonAny`, and the `openapi` spec injector (pure JSON Schema munging).
//! `native` feature: full runtime (`OrcaTool`, `ToolCtx`, `ToolRegistry`,
//! `ErasedTool`, axum router, inventory-driven `native_register`).
//! `cli` feature: unified clap-driven CLI surface (`register_op!` macro +
//! `CliOp` inventory + dispatcher). Implies `native`.
//!
//! Every crate that defines `#[orca_tool]`-annotated functions depends on
//! this crate. The registration framework lives here (not in `tools-def`)
//! so content crates carrying their own tools have no cycle through any
//! upstream "all the tools" crate.

mod def;
pub use def::{OrcaOp, OrcaToolDef};

pub mod json_any;
pub use json_any::JsonAny;

pub mod openapi;

#[cfg(feature = "native")]
mod ctx;
#[cfg(feature = "native")]
mod erased;
#[cfg(feature = "native")]
mod inventory_slice;
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
pub use inventory_slice::{ToolRegistration, native_register};
#[cfg(feature = "native")]
pub use registry::{CliArgs, ToolRegistry};
#[cfg(feature = "native")]
pub use tool::OrcaTool;
#[cfg(feature = "native")]
pub use types::{ToolCall, ToolDef, ToolResult};

#[cfg(feature = "cli")]
pub mod cli;
