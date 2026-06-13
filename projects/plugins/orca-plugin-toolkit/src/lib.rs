//! `orca-plugin-toolkit` — higher-level primitives for plugin authors.
//!
//! Not a re-export shim. The toolkit COMBINES the underlying macros so a
//! plugin expresses maximum functionality with minimum boilerplate (see
//! [[feedback-plugin-toolkit-max-power-min-boilerplate]]). One toolkit
//! macro emits db table + REST verb tools + endpoint resolution + serde +
//! clap + MCP + REST + schemars in one shot.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use orca_plugin_toolkit::prelude::*;
//!
//! endpoint_resource! {
//!     plugin: "dockge",
//!     fields: {
//!         base_url: String,
//!         token:    String,
//!     }
//! }
//! ```
//!
//! Generates `dockge.{list,detail,create,update,delete}` over a
//! `dockge_endpoints` SQLite table — every surface (CLI / MCP / REST) is
//! automatic. No further code is needed for the registry layer; plugin
//! authors only hand-write upstream API logic and surface-extension tools
//! (e.g. stack lifecycle).

pub mod prelude;
pub mod runtime;

// `endpoint_resource!` is a function-like proc-macro defined in the
// `derive` crate (alongside `#[orca_tool]` and `#[derive(Replicated)]`).
// Re-exported here so plugin authors only depend on the toolkit crate.
pub use derive::endpoint_resource;
