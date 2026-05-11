//! Shared, broadly-reusable utilities for orca. Each submodule was its
//! own crate prior to consolidation; merging cut binary count and
//! keeps the dep graph shallow. Modules are independent except where
//! noted (graphql uses http, tool uses config).

pub mod config;
pub mod fs;
pub mod git;
pub mod graphql;
pub mod http;
pub mod state;
pub mod tool;
