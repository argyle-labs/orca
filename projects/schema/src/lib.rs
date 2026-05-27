//! Schema domain — multi-tab DB schema introspection + per-DB registry tools.
//!
//! Owns the heavy MySQL/Postgres/SQLite/docker introspection paths along
//! with the typed view (`tabs`/`columns`/`foreign_keys`/`domains`) and the
//! 5 `#[orca_tool]`s (`namespace.schema` CRUD + `namespace.schema.view`).

pub mod types;

#[cfg(feature = "native")]
pub mod view;

#[cfg(feature = "native")]
pub mod tools;
