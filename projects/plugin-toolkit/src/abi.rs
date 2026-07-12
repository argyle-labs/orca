//! Plain-serde plugin capability types — re-exported from the standalone
//! [`plugin_abi`] crate.
//!
//! The wire contract (`ToolDef` / `BackendDef` / `DbOp` / `SecretOp` / …) lives
//! in its own crate so consumers that need only the wire types (`plugin-loader`,
//! a thin subprocess plugin) link just `serde` + `schemars` and none of orca
//! core. Plugin authors keep reaching it as `plugin_toolkit::abi::*`; this
//! module is a transparent re-export so no source changes.
pub use ::plugin_abi::*;
