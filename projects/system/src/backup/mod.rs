//! Generic backup/restore subsystem.
//!
//! A location-agnostic [`store`] of dated backups, a [`provider`] registry that
//! domains and plugins register their "how" against, and a [`tools`] surface
//! (the single `backup.*` domain, parameterized by `--kind`). The store owns
//! dating/listing/selection/retention; a provider is one KIND (host, service, …)
//! that owns what it captures. See the module docs on each for the full contract.

pub mod host;
pub mod provider;
pub mod service_kind;
pub mod store;
pub mod tools;

pub use provider::{BackupOutcome, BackupProvider, register_provider};
pub use store::{BackupSlot, BackupStore};
pub use tools::register_builtin_providers;
