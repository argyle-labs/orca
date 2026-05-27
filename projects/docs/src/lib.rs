//! Docs domain — doc registry + OpenAPI/GraphQL spec registry.
pub mod doc_registry;
pub mod docs;
pub mod embedded;
#[cfg(feature = "native")]
pub mod mcp_helpers;
#[cfg(feature = "native")]
pub mod native_support_docs;
#[cfg(feature = "native")]
pub mod native_support_specs;
pub mod spec_registry;
#[cfg(feature = "cli")]
pub mod spec_cli;
#[cfg(feature = "native")]
pub mod tree;
