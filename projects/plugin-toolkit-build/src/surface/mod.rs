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
//! Read operations (GET / GraphQL `query`) surface at the default role. Write
//! operations (POST/PUT/DELETE / GraphQL `mutation`) are classified
//! `#[orca_tool(..., data_mutation = true)]` and default to `role = "admin"` —
//! so a non-admin identity can invoke them only with the `can_mutate` opt-in
//! (see `dispatch::tool_roles`).
//!
//! ## Per-operation exception
//!
//! A specific mutation can be made user-callable without the opt-in by marking
//! it at the source:
//! - OpenAPI: `x-orca-user-callable: true` on the operation object.
//! - GraphQL: a `# @orca:user-callable` comment line in the operation's
//!   `.graphql` file.
//!
//! A marked mutation still carries `data_mutation = true` (for classification /
//! auditing) but is emitted with `role = "read"`, so any read identity may call
//! it.

pub mod graphql;
pub mod openapi;

pub(crate) mod common;
