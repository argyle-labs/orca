//! `orca-dispatch` — runtime for OrcaTool.
//!
//! The contract (metadata traits, error, JsonAny, protocol types,
//! ToolCtx/OrcaTool/RemoteExec trait anchors) lives in `orca-contract`. The
//! proc-macro that emits per-tool scaffolding is `orca-macro`. This crate
//! provides:
//!
//! - The `ErasedTool` object-safe wrapper (`erased`)
//! - The `inventory` slice (`ToolRegistration`) that every `#[orca_tool]`
//!   submits into at linker time (`inventory_slice`)
//! - Free-function dispatchers that walk that slice — `mcp_definitions`,
//!   `dispatch`, `axum_router`, `clap_command`, `cli_dispatch` (`registry`)
//! - The OpenAPI spec injector (`openapi`)
//! - The unified clap-driven CLI surface — `register_op!` macro + `CliOp`
//!   inventory + dispatcher (`cli`)
//!
//! There is no `ToolRegistry` struct: all dispatch walks `inventory::iter`
//! directly, with results cached behind a `OnceLock`.

pub mod cli;
mod erased;
mod inventory_slice;
pub mod openapi;
mod registry;
pub mod remote_ok;
pub mod tool_roles;

pub use erased::{ErasedTool, ToolWrapper, value_to_text};
pub use inventory_slice::ToolRegistration;
pub use registry::{
    CliArgs, axum_router, clap_command, cli_dispatch, dispatch, dispatch_text, mcp_definitions,
    names, remote_ok_names, required_role, role_table,
};
