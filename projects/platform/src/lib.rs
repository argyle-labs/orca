//! Platform domain — db admin, profile, sweep, config store, engine
//! registry, JSON Schema utilities.

pub mod config;
pub mod db_admin;
pub mod engine;
pub mod json_schema;
pub mod profile;
pub mod profile_manager;
pub mod profile_native;
pub mod sweep;

#[cfg(test)]
pub(crate) mod test_support;
