//! ABI-stable cdylib plugin boundary — re-exported from the standalone
//! [`plugin_abi`] crate.
//!
//! The wire contract (`PluginModRef` / `ToolDef` / `BackendDef` + the
//! compat/version header) was extracted into its own crate so consumers that
//! need only the FFI seam (`plugin-loader`, a thin plugin) link just
//! `abi_stable` + `serde` + `schemars` and none of orca core. Plugin authors
//! keep reaching it as `plugin_toolkit::abi::*`; this module is a transparent
//! re-export so no source changes.
pub use ::plugin_abi::*;
