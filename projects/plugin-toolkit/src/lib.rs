//! `orca-plugin-toolkit` — higher-level primitives for plugin authors.
//!
//! Not a re-export shim. The toolkit COMBINES the underlying macros so a
//! plugin expresses maximum functionality with minimum boilerplate (see
//! [[feedback-plugin-toolkit-max-power-min-boilerplate]]). One toolkit
//! macro emits db table + REST verb tools + endpoint resolution + serde +
//! clap + MCP + REST + schemars in one shot.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use plugin_toolkit::prelude::*;
//!
//! endpoint_resource! {
//!     plugin: "dockge",
//!     fields: {
//!         base_url: String,
//!         token:    String,
//!     }
//! }
//! ```
//!
//! Generates `dockge.{list,detail,create,update,delete}` over a
//! `dockge_endpoints` SQLite table — every surface (CLI / MCP / REST) is
//! automatic. No further code is needed for the registry layer; plugin
//! authors only hand-write upstream API logic and surface-extension tools
//! (e.g. stack lifecycle).

pub mod abi;
pub mod address;
pub mod api_client;
pub mod logging;
pub mod prelude;
pub mod runtime;

// `endpoint_resource!` is a function-like proc-macro defined in the
// `derive` crate (alongside `#[orca_tool]` and `#[derive(Replicated)]`).
// Re-exported here so plugin authors only depend on the toolkit crate.
pub use derive::endpoint_resource;

// ── Macro-emission landing pads ────────────────────────────────────────
//
// `#[orca_tool]` / `endpoint_resource!` / dispatch's `register_op!` emit
// absolute paths like `::plugin_toolkit::contract::OrcaTool`,
// `::plugin_toolkit::inventory::submit!`, `::plugin_toolkit::serde_json::*`.
// Those resolve through the re-exports below, so a consumer crate only
// needs `plugin-toolkit` as a direct dep — never `contract`, `inventory`,
// `serde_json`, `anyhow`, `clap`, `schemars`, `async_trait`, `tokio`,
// `dispatch`, `derive`, `db`, or `rusqlite`. See [[feedback-plugin-toolkit-is-the-gateway]]
// and task #29.

pub use ::abi_stable;
pub use ::anyhow;
pub use ::async_trait;
pub use ::clap;
pub use ::contract;
pub use ::db;
pub use ::derive;
pub use ::dispatch;
pub use ::inventory;
pub use ::rusqlite;
pub use ::schemars;
pub use ::serde;
pub use ::serde_json;
pub use ::thiserror;
pub use ::tokio;

// GraphQL query trait + derive. The build-time codegen
// (`plugin_toolkit_build::graphql`) rewrites its emitted `graphql_client::`
// paths to `::plugin_toolkit::graphql_client::*`, so plugins never dep the
// crate directly.
pub use ::graphql_client;

// OpenAPI / progenitor codegen runtime. The build-time codegen
// (`plugin_toolkit_build::openapi`) rewrites the progenitor-emitted crate
// paths to `::plugin_toolkit::*`, so an OpenAPI plugin needs none of these as
// direct deps.
pub use ::{bytes, chrono, futures_core, progenitor_client, regress, reqwest, uuid};

// Macro-runtime registration target types (re-exported so endpoint_resource!
// emissions resolve through plugin_toolkit, not macro_runtime directly).
pub use ::macro_runtime::{ReplicatedRegistration, SchemaFragment};
pub use ::tracing;

// ── Runtime primitives ──────────────────────────────────────────────────
//
// Per [[feedback-plugin-toolkit-is-the-gateway]], plugins reach every
// orca-side capability through the toolkit. These submodules re-export the
// underlying crates so a plugin's only orca-side import is
// `use plugin_toolkit::prelude::*;` — `http`, `graphql`, `openapi`
// are then in scope as namespaced modules.

/// HTTP transport. Re-export of `utils::http` so HTTP bug fixes propagate
/// to every plugin from one place.
pub mod http {
    pub use utils::http::*;
}

/// GraphQL client + envelope types. Re-export of the `graphql` crate so
/// plugins talk GraphQL transport without importing the crate directly.
pub mod graphql {
    pub use ::graphql::*;
    // The query trait the build-time codegen implements for each operation.
    // Re-exported here so a plugin's generic bounds read `graphql::GraphQLQuery`
    // — the plugin never names the backing `graphql_client` crate.
    pub use ::graphql_client::GraphQLQuery;
}

/// OpenAPI spec parsing + normalization helpers. Re-export of the
/// `openapi` crate. Typed-client codegen (progenitor) runs in plugin
/// build scripts — a build-time helper for the codegen pipeline is the
/// next slice on top of this primitive.
pub mod openapi {
    pub use ::openapi::*;
}

// ── Domain registration crates ──────────────────────────────────────────
//
// Per [[feedback-plugin-toolkit-only-no-exceptions]]: every orca capability,
// including the domain contracts plugins register with, reaches plugins ONLY
// through this gateway. Third-party plugin authors write
// `use plugin_toolkit::prelude::*;` + `use plugin_toolkit::<domain>::*;`
// and never direct-dep on a domain crate. Cycles are broken by relocating
// `#[orca_tool]` sites OUT of the domain crate into a sibling/system crate
// (see `system::notify_send` for the pattern). Domain crates here are
// pure plumbing: model + trait + dispatcher.
pub mod notifications {
    pub use ::notifications::*;
}
pub mod containers {
    pub use ::containers::*;
}

/// Hashing helpers. Wraps `sha2` so plugins compute digests without depending
/// on the crate directly — if the backing hash lib ever changes, callers don't
/// know the difference.
pub mod hash {
    use sha2::{Digest, Sha256};

    /// Hex-encoded SHA-256 of `bytes`.
    pub fn sha256_hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let digest = Sha256::digest(bytes);
        let mut s = String::with_capacity(digest.len() * 2);
        for b in digest {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0xf) as usize] as char);
        }
        s
    }
}
