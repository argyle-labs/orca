//! Tools exposed by the db crate.
//!
//! Each submodule defines `#[orca_tool]`-annotated functions whose bodies
//! call into the db's own modules directly. Registration is automatic via
//! the `inventory` slice that `orca-tool` walks at startup — no central
//! enrollment list to edit when adding a tool.

pub mod schedule;
