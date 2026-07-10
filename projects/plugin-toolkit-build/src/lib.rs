//! `orca-plugin-toolkit-build` — build-script helpers for plugin codegen.
//!
//! Pairs with the runtime `plugin_toolkit` crate, but lives separately
//! so plugin lib compiles don't pull in progenitor / graphql_client_codegen.
//!
//! Used from a plugin `build.rs`:
//!
//! ```rust,ignore
//! // build.rs (OpenAPI):
//! fn main() {
//!     plugin_toolkit_build::openapi::generate_all("specs", "arr").unwrap();
//! }
//!
//! // build.rs (GraphQL):
//! fn main() {
//!     plugin_toolkit_build::graphql::generate(
//!         "../unraid/schemas",
//!         "queries",
//!     ).unwrap();
//! }
//! ```
//!
//! Per [[feedback-plugin-toolkit-is-the-gateway]]: the toolkit is the
//! single gateway for plugin capability. Codegen plumbing centralises here
//! so fixes (progenitor version bumps, normalize pass tweaks, codegen
//! options) land once and propagate to every plugin that codegens against
//! an upstream spec.

pub mod descriptor;
pub mod graphql;
pub mod openapi;
pub mod prune;
pub mod surface;
