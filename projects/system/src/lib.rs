//! System domain — installation lifecycle, runtime/system snapshot, and
//! profile management. Leaf crate: tools call `db::*` / `utils::*`
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

pub mod host;
pub mod host_identity;
pub mod host_status;
pub mod system_info_types;
pub mod topology;
pub mod update_state;

#[cfg(test)]
pub(crate) mod test_support;

pub mod update;

pub mod dev;

pub mod install;

pub mod commands;

pub mod install_status;

pub mod system_info;

pub mod system;

pub mod periodic;

pub mod scheduler;

pub mod diagnostic;

// Subcommand handler modules. These currently mix clap action enums + impl;
// the next slice converts each public entry into a `#[orca_tool]` and lets the
// macro emit the CLI surface, killing the action enums.
pub mod daemon;
#[cfg(feature = "cli")]
pub mod dev_serve;
#[cfg(feature = "cli")]
pub mod hook;
pub mod package;
pub mod sysadmin;

// Docker-compose service listing + test runner (system.infra.*) and the
// liveness probe (system.health). Absorbed from the dissolved `fleet` crate.
pub mod infra;
pub mod meta;

// Moved 2026-05-29 from dissolved `platform` crate.
pub mod engine;
pub mod sweep;
