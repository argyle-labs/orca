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

#[cfg(feature = "native")]
pub mod diagnostic;

// Subcommand handler modules. These currently mix clap action enums + impl;
// the next slice converts each public entry into a `#[orca_tool]` and lets the
// macro emit the CLI surface, killing the action enums.
#[cfg(feature = "native")]
pub mod daemon;
#[cfg(feature = "cli")]
pub mod dev_serve;
#[cfg(feature = "cli")]
pub mod hook;
#[cfg(feature = "native")]
pub mod package;
#[cfg(feature = "native")]
pub mod sysadmin;
#[cfg(feature = "native")]
pub mod update_cmd;
