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
pub mod auth;
#[cfg(feature = "native")]
pub mod db_admin;
#[cfg(feature = "native")]
pub mod docker;
#[cfg(feature = "native")]
pub mod docs;
#[cfg(feature = "native")]
pub mod infra;
#[cfg(feature = "native")]
pub mod lifecycle;
#[cfg(feature = "native")]
pub mod mgmt;
#[cfg(feature = "native")]
pub mod pki;
#[cfg(feature = "native")]
pub mod plugin_runtime;
#[cfg(feature = "native")]
pub mod plugins;
#[cfg(feature = "native")]
pub mod profile;
#[cfg(feature = "native")]
pub mod secrets;
#[cfg(feature = "native")]
pub mod spec_registry;
#[cfg(feature = "native")]
pub mod system;
