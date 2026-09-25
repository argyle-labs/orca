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

/// Crate-wide serialization lock for tests that mutate the process-global
/// `ORCA_DAEMON_URL` / `ORCA_HTTP_PORT` / `ORCA_HOME`. Those vars feed
/// `cli::local_daemon_reachable()`, which is a LIVE, uncached TCP probe — so a
/// test that momentarily drops `ORCA_DAEMON_URL` lets a concurrent test's probe
/// fall through to the compile-time default port, where a real dev daemon is
/// usually listening. Reachability then flips true, `run_unit` posts to the
/// process-global `OnceLock` MockDaemon (which answers `Ok` for any unknown
/// name), and the expected "unknown op" error never happens. Module-private
/// locks do NOT serialize across modules, so every test in this crate touching
/// those three vars must hold THIS single lock, for the whole async body (drive
/// it with `block_on` from a sync `#[test]`, never across an `.await`).
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub mod cli;
pub mod diagnostics_surface;
mod erased;
mod inventory_slice;
pub mod openapi;
mod registry;
pub mod remote_ok;
pub mod tool_roles;
pub mod unit_surface;
pub mod ups_surface;

pub use erased::{ErasedTool, ToolWrapper, value_to_text};
pub use inventory_slice::ToolRegistration;
#[cfg(feature = "server")]
pub use registry::axum_router;
pub use registry::{
    CliArgs, clap_command, cli_dispatch, data_mutation_names, dispatch, dispatch_text,
    dynamic_tool_defs, local_only_names, mcp_definitions, names, remote_ok_names, required_role,
    role_table, set_dynamic_dispatch, take_ambient, tool_exists, tool_manifest_json,
};
