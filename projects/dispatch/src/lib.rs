//! `dispatch` — runtime for OrcaTool. Paired with the `derive` proc-macro
//! crate (which emits inventory entries at compile time) — they form the
//! macro+runtime split (like `serde-derive`+`serde`) forced by Rust's
//! proc-macro crate restrictions.
//!
//! **NOT mesh-dispatch.** Sending a command to another peer over the pod mesh
//! lives in `pod` (caller_token, remote_exec, `RemoteExec` trait). This crate
//! only routes tool calls within a single process.
//!
//! The contract (metadata traits, error, JsonAny, protocol types,
//! ToolCtx/OrcaTool/RemoteExec trait anchors) lives in `contract`. The
//! proc-macro that emits per-tool scaffolding is `derive`. This crate
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
pub mod unit_surface;

pub use erased::{ErasedTool, ToolWrapper, value_to_text};
pub use inventory_slice::ToolRegistration;
pub use registry::{
    CliArgs, axum_router, clap_command, cli_dispatch, dispatch, dispatch_text, dynamic_tool_defs,
    mcp_definitions, names, remote_ok_names, required_role, role_table, set_dynamic_dispatch,
    take_ambient, tool_exists, tool_manifest_json,
};
