//! Thin cdylib wrapper around `orca_tools_def::wasm`.
//!
//! All the interesting code (the `OrcaClient` JS class and its per-tool
//! methods) is emitted by `#[orca_tool]` annotations in `orca-tools-def`
//! under the `wasm` feature. This file exists only to give wasm-pack a
//! `cdylib` target to compile.

pub use orca_tools_def::wasm::OrcaClient;
