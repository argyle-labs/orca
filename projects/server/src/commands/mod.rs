//! Orca commands — server-side helpers reachable through OrcaOps (service-trait
//! impls in `mcp/*_service.rs`) and the legacy clap subcommands that haven't
//! been migrated yet.
//!
//! Most former `cmd_*` shims are gone — `orca <domain> <verb>` dispatches via
//! the OrcaOp inventory. What remains:
//! - `oauth` — GitHub/Atlassian OAuth flows used by `AuthService`.
//! - `daemon` — daemon lifecycle (not yet migrated; long-running supervisor).
//! - `hook_cmd` — Claude Code hook handlers (stdin-driven; different shape).
//! - `spec` — disk-spec scaffold (`spec add`) + repo scanner (`spec sync`)
//!   not yet migrated. Most spec verbs already go through OrcaOp.
//! - `update` — `check_for_update` / `apply_update` / `startup_update_check`
//!   used by `LifecycleService` and the daemon startup banner.

// Slash command prompts embedded at build time.
include!(concat!(env!("OUT_DIR"), "/embedded_commands.rs"));

/// List all embedded slash commands as `/name` strings.
pub fn list_embedded_commands() -> Vec<String> {
    embedded_command_names()
        .iter()
        .map(|name| format!("/{name}"))
        .collect()
}

pub mod daemon;
pub mod dev_serve;
pub mod hook_cmd;
pub mod package;
pub mod pod;
pub mod spec;
pub mod system;
pub mod update;

pub use daemon::{DaemonAction, cmd_daemon};
pub use hook_cmd::{HookAction, cmd_hook};
pub use package::{PackageAction, cmd_package};
pub use spec::{SpecAction, cmd_spec};
pub use system::{SystemAction, cmd_system};
pub use update::startup_update_check;
