//! Thin cdylib wrapper around `orca_tools_def::wasm`.
//!
//! All the interesting code (the `OrcaClient` JS class and its per-tool
//! methods) is emitted by the `declare_tools!{}` block in
//! `orca-tools-def::lib.rs` under the `wasm` feature. This file exists only
//! to give wasm-pack a `cdylib` target to compile.

pub use orca_tools_def::wasm::OrcaClient;
