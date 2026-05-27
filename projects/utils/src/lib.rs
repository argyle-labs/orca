//! Shared, broadly-reusable utilities for orca. Each submodule was its
//! own crate prior to consolidation; merging cut binary count and
//! keeps the dep graph shallow. Modules are independent except where
//! noted (graphql uses http).

pub mod config;
pub mod fs;
pub mod git;
pub mod graphql;
pub mod http;
pub mod markdown;
pub mod state;
