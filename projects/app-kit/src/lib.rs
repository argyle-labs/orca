//! Orca app-kit — in-process embedding surface for native UIs.
//!
//! See `Cargo.toml` for the surface-emission relationship to the domain crates.
//!
//! **Hard rule:** this file contains NO hand-written `#[uniffi::export]`. Every
//! UniFFI symbol that ends up in the cdylib comes from the `#[orca_tool]`
//! macro, which is the single declaration point for all four surfaces (REST,
//! MCP, CLI, UniFFI). See `feedback_four_surface_parity.md`.
//!
//! Foundation slice scope:
//!   1. `uniffi::setup_scaffolding!()` registers the FFI symbol table that
//!      Swift/Kotlin bindings hook into.
//!   2. Re-exporting the runtime + every domain crate ensures every
//!      `#[orca_tool]` is linked into this cdylib so the macro's UniFFI
//!      emission (forthcoming) lands here.
//!
//! Pipeline verification deferred until the macro emits the first real
//! UniFFI wrapper (task #5, blocked on `OrcaAppKit::init()` lifecycle task #4).

uniffi::setup_scaffolding!();

// Link anchors — every domain crate must be referenced so the linker keeps
// its `inventory::submit!` registrations in this cdylib. Without these the
// linker would drop unused crates and lose `#[orca_tool]` entries.
#[allow(unused_imports)]
use {agents as _, auth as _, dispatch as _, plugins as _, system as _};

pub mod lifecycle;
pub use lifecycle::{AppKitConfig, OrcaAppKit};
