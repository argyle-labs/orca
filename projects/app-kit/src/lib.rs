//! Orca app-kit — in-process embedding surface for native UIs.
//!
//! See `Cargo.toml` for the surface-emission relationship to tools-def.
//!
//! This file is intentionally minimal for the foundation slice:
//!   1. `uniffi::setup_scaffolding!()` registers the FFI symbol table that
//!      Swift/Kotlin bindings hook into.
//!   2. Re-exporting `orca_tools_def` ensures every `#[orca_tool]` is linked
//!      into this cdylib so future per-tool `#[uniffi::export]` wrappers can
//!      call them.
//!
//! The macro-emitted UniFFI wrappers, the `OrcaAppKit::init()` embedder
//! lifecycle (ToolCtx + DB + integrations), and the `uniffi::Record` derive
//! sweep land in follow-up slices.

uniffi::setup_scaffolding!();

// Anchor the link so the tools-def crate (and every #[orca_tool] inside it,
// plus its native impls) is pulled into this cdylib. Without a use-site
// reference, the linker would drop the crate.
#[allow(unused_imports)]
use orca_tools_def as _;

/// Build version string. Wired up first as a plumbing smoke test — proves
/// `uniffi-bindgen` produces callable Swift + Kotlin symbols from this crate
/// before the per-tool macro emission lands.
#[uniffi::export]
pub fn orca_app_kit_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
