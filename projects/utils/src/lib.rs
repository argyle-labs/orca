//! Shared, broadly-reusable utilities for orca. Each submodule was its
//! own crate prior to consolidation; merging cut binary count and
//! keeps the dep graph shallow. Modules are independent except where
//! noted (graphql uses http).

pub mod embedded;
pub mod fs;
pub mod git;
pub mod hash;
pub mod http;
pub mod markdown;
pub mod search;
pub mod state;
pub mod tree;
