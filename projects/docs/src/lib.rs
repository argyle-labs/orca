//! Docs domain — doc registry + embedded vault.
pub mod doc_registry;
pub mod docs;
pub mod embedded;
#[cfg(feature = "native")]
pub mod mcp_helpers;
#[cfg(feature = "native")]
pub mod native_support_docs;
#[cfg(feature = "native")]
pub mod tree;
