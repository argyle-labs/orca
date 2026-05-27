//! System domain — installation lifecycle, runtime/system snapshot, and
//! profile management. Leaf crate: tools call `db::*` / `orca_utils::*`
//! directly inside the fn body. No service traits.
//!
//! Module migration plan:
//! - `system`    — install/uninstall lifecycle + system-detail snapshot
//!   (formerly fleet::system + server::services::system).
//! - `lifecycle` — orca runtime, update, agents/profile detail
//!   (formerly fleet::lifecycle + server::services::lifecycle).
//! - `profile`   — orca profile CRUD
//!   (formerly platform::profile + server::services::profile).
//!
//! Modules will be filled in by subsequent slices.

pub mod system_info_types;
pub mod update_state;

#[cfg(feature = "native")]
pub mod update;

#[cfg(feature = "native")]
pub mod dev;

#[cfg(feature = "native")]
pub mod install;

#[cfg(feature = "native")]
pub mod commands;

#[cfg(feature = "native")]
pub mod install_status;

#[cfg(feature = "native")]
pub mod system_info;

pub mod system;

#[cfg(feature = "native")]
pub mod periodic;

#[cfg(feature = "native")]
pub mod scheduler;
