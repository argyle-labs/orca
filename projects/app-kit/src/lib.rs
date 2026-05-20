//! Orca app-kit — in-process embedding surface for native UIs.
//!
//! See `Cargo.toml` for the surface-emission relationship to tools-def.
//!
//! **Hard rule:** this file contains NO hand-written `#[uniffi::export]`. Every
//! UniFFI symbol that ends up in the cdylib comes from the `#[orca_tool]`
//! macro, which is the single declaration point for all four surfaces (REST,
//! MCP, CLI, UniFFI). See `feedback_four_surface_parity.md`.
//!
//! Foundation slice scope:
//!   1. `uniffi::setup_scaffolding!()` registers the FFI symbol table that
//!      Swift/Kotlin bindings hook into.
//!   2. Re-exporting `orca_tools_def` ensures every `#[orca_tool]` is linked
//!      into this cdylib so the macro's UniFFI emission (forthcoming) lands
//!      here.
//!
//! Pipeline verification deferred until the macro emits the first real
//! UniFFI wrapper (task #5, blocked on `OrcaAppKit::init()` lifecycle task #4).

uniffi::setup_scaffolding!();

// Anchor the link so the tools-def crate (and every #[orca_tool] inside it,
// plus its native impls) is pulled into this cdylib. Without a use-site
// reference, the linker would drop the crate.
#[allow(unused_imports)]
use orca_tools_def as _;

pub mod lifecycle;
pub use lifecycle::{AppKitConfig, OrcaAppKit};
