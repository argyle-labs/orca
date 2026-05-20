//! Abstract service traits injected into `ToolCtx` so server-coupled tool
//! `run` bodies can live in this crate.
//!
//! The trait *signatures* are (no server-internal types leak
//! through). The *implementations* live in `projects/server/` and call the
//! real internal modules.
//!
//! # Service-registration convention (locked 2026-05-19)
//!
//! Each `services::foo` module exposes three items:
//!
//! 1. `pub trait FooService` — the abstract service the tool body calls into.
//! 2. `pub trait ProvideFoo` — embedder-implemented; yields an
//!    `Arc<dyn FooService>`. One small trait per service, standalone — no
//!    aggregate supertrait.
//! 3. `pub fn register_foo(ctx: &mut ToolCtx, p: &impl ProvideFoo)` — free
//!    function the embedder calls to inject the service.
//!
//! Embedders (orca-server, orca-app-kit, integration tests) pick which
//! `register_*` calls they make — there is no compile-time "you forgot
//! one" guard. Tools whose bodies fetch a missing service fail at dispatch
//! with `no service registered for <type>`. This is deliberate: app-kit
//! intentionally exposes a subset of tools that don't need server-side
//! impls until those impls grow wasm-safe alternatives.
//!
//! Adding a new service:
//!   - Define the service trait alongside its impl module.
//!   - Add the `ProvideFoo` trait + `register_foo` fn in the same module.
//!   - Each embedder that wants the tool to work adds one `register_foo`
//!     call to its own service-wiring path.

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
