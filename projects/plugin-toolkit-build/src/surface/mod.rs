//! Orca tool-surface generators — turn a codegen'd typed client into one
//! `#[orca_tool]` per operation, so a plugin author never hand-wraps a
//! capability.
//!
//! Two shapes, one contract:
//! - [`openapi`] pairs each generated progenitor `impl Client` method back to
//!   its spec operation and emits a wrapper calling it through
//!   `crate::tools::make_client`.
//! - [`graphql`] walks the generated `graphql_client` query modules and emits
//!   a wrapper dispatching through `crate::Client::query` /
//!   `crate::tools::surface_client`.
//!
//! Both run from a plugin `build.rs` *after* the matching codegen pass
//! (`openapi::generate_all*` / `graphql::generate`) has written its output into
//! `OUT_DIR`.
//!
//! ## Authorization
//!
//! Read operations (GET / GraphQL `query`) surface at the default role. Every
//! write (any non-GET method / GraphQL `mutation`) is emitted `role = "admin"`.
//!
//! ## Per-operation markers
//!
//! Set at the source — OpenAPI `x-orca-*: true` on the operation object,
//! GraphQL a `# @orca:*` comment line in the operation's `.graphql` file:
//! - `admin` — force `role = "admin"`; suppresses any opt-in.
//! - `no-tool` — emit no tool.
//! - `user-callable` / `data-mutation` — a write gains `data_mutation = true`,
//!   callable by a non-admin identity only with the `can_mutate` opt-in (see
//!   `dispatch::tool_roles`). It stays `role = "admin"`.

pub mod graphql;
pub mod openapi;

pub(crate) mod common;
