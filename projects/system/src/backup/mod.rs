//! Generic backup/restore subsystem — TWO orthogonal axes:
//!
//! * **KIND (WHAT to back up)** — the [`provider`] registry (host, service, …).
//!   A [`BackupProvider`] owns what a kind captures and how to put it back.
//! * **TARGET (WHERE it's stored)** — the [`target`] registry. Core owns exactly
//!   the built-in [`local`] file-path target; every other target kind (nfs, smb,
//!   s3, pbs, git) is plugin-exposed. A [`BackupTargetProvider`] resolves a
//!   target to a filesystem-rooted [`store::BackupStore`].
//!
//! The [`store`] is location-agnostic and owns dating/listing/selection/retention
//! beneath whatever root a target hands it; the [`tools`] surface is the single
//! `backup.*` domain (parameterized by `--kind`) that drives both axes. See the
//! module docs on each for the full contract.

pub mod host;
pub mod local;
pub mod provider;
pub mod service_kind;
pub mod store;
pub mod target;
pub mod tools;

pub use local::LocalTarget;
pub use provider::{BackupOutcome, BackupProvider, register_provider};
pub use store::{BackupSlot, BackupStore};
pub use target::{BackupTargetProvider, register_target};
pub use tools::register_builtin_providers;
