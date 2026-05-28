//! Platform domain — db admin, sweep, config store, engine registry,
//! JSON Schema utilities. Profile/namespace moved to `namespace` crate
//! (slice 3 of crate-topology-v2, 2026-05-27).

pub mod config;
pub mod db_admin;
pub mod engine;
pub mod json_schema;
pub mod sweep;

#[cfg(test)]
pub(crate) mod test_support;
