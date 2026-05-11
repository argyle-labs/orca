//! Abstract service traits injected into `ToolCtx` so server-coupled tool
//! `run` bodies can live in this wasm-safe crate.
//!
//! The trait *signatures* are wasm-safe (no server-internal types leak
//! through). The *implementations* live in `projects/server/` and call the
//! real internal modules.

#[cfg(feature = "native")]
pub mod agent_backend;
#[cfg(feature = "native")]
pub mod agents;
#[cfg(feature = "native")]
pub mod docs;
#[cfg(feature = "native")]
pub mod infra;
#[cfg(feature = "native")]
pub mod plugins;
