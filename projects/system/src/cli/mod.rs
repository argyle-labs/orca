//! CLI subcommand handlers for the `system` domain. These are the
//! human-facing `orca daemon|hook|package|system|update|dev-serve`
//! subcommand bodies. The `OrcaTool` surface for `system.*` is the sibling
//! `crate::commands` module — this is just clap glue.

pub mod daemon;
pub mod dev_serve;
pub mod hook;
pub mod package;
pub mod sysadmin;
pub mod update;

pub use daemon::{DaemonAction, cmd_daemon};
pub use hook::{HookAction, cmd_hook};
pub use package::{PackageAction, cmd_package};
pub use sysadmin::{SystemAction, cmd_system};
pub use update::startup_update_check;
