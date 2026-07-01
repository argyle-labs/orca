//! Foreign-database schema introspection (`schema.*` / `schema.view.*` tools).
//!
//! Schema databases (MySQL/Postgres/SQLite) are first-class objects registered
//! in orca.db (see [`crate::schema_databases`]) that assign to a namespace.
//! `types` holds the tool args/outputs; `view` connects to the foreign engine
//! and renders the tabbed introspection view. Consumed by the `spec` crate's
//! tool surface.
//!
//! Folded in from the former standalone `database` crate (2026-07-01): one
//! database crate owns both orca's own store and foreign-schema introspection.

pub mod types;
pub mod view;
