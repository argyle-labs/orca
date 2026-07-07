//! Developer tooling for orca — code that only runs from a workspace
//! checkout. Holds:
//!
//! - [`mode`] — `orca dev enable / disable / sync` cargo-watch supervisor.
//! - [`dev_serve`] — HTTP server that streams workspace-built binaries to
//!   peers configured to fetch from a `dev_source` URL.
//! - [`sweep`] — workspace audits (cargo-machete / cargo-deny).
//!
//! Dev tooling MAY call into the `system` and `pod` crates (or anywhere
//! else it needs to). Those crates do NOT call back into `dev` — that's
//! how we keep production peers from carrying the dev surface.

pub mod dev_serve;
pub mod mode;
pub mod sweep;
