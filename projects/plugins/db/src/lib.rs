//! `database` plugin — multi-tab DB schema introspection + types.
//! Tool surface (`schema.*` / `schema.view.*`) lives in the `spec` crate
//! and calls into this plugin's `types` + `view` modules.

pub mod types;
pub mod view;
