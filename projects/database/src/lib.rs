//! `database` core crate — multi-tab DB schema introspection + types.
//! Tool surface (`schema.*` / `schema.view.*`) lives in the `spec` crate
//! and calls into this crate's `types` + `view` modules.
//!
//! Relocated out of `projects/plugins/db` (2026-06-27): `database` is core
//! infrastructure consumed as a library by `spec`, not a sideloadable plugin.

pub mod types;
pub mod view;
