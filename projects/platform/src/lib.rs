//! Platform domain — db admin, profile, sweep, config store, engine
//! registry, JSON Schema utilities.

pub mod config;
pub mod db_admin;
pub mod engine;
pub mod json_schema;
pub mod profile;
#[cfg(feature = "native")]
pub mod profile_manager;
pub mod sweep;

#[cfg(all(test, feature = "native"))]
pub(crate) mod test_support;
