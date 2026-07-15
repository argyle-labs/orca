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

pub mod autofs;
pub mod capability;
pub mod capability_tools;
pub mod host;
pub mod host_identity;
pub mod managed_mounts;
pub mod service_tools;
pub mod source_election;
pub mod storage_selfheal;
pub mod storage_tools;
pub mod system_info_types;
pub mod topology;
pub mod unit_identity;
pub mod update_state;

pub mod update;

pub mod dev;

pub mod install;

pub mod commands;

pub mod install_status;

pub mod system_info;

pub mod system;
pub mod system_detail_view;

pub mod periodic;

pub mod maintenance;

pub mod scheduler;

pub mod diagnostic;

pub mod notify_bridge;
pub mod notify_ingest;
pub mod notify_tools;

pub mod daemon;
pub mod hook;
pub mod package;
pub mod sysadmin;

// Tool surfaces relocated from `db` (db is a primitive, not a tool host —
// see [[feedback-plugin-toolkit-is-the-gateway]]).
pub mod config_tools;

pub mod db_admin;
pub mod plugin_fetch;
pub mod plugin_manager;
pub mod retention_tools;
pub mod schedule_tools;

// Relocated 2026-06-01:
// - `engine` (LLM backend registry) → `projects/model/src/engine.rs`.
// - `sweep` (workspace cargo-machete/deny) → `projects/dev/src/sweep.rs`.
// - `dev_serve` (HTTP server for dev-source binaries) → `projects/dev/src/dev_serve.rs`.
// - The `cmd_dev_*` supervisor functions from `dev.rs` → `projects/dev/src/mode.rs`.
//   The remaining `dev.rs` holds the URL-fetch path used by `system.update`.
