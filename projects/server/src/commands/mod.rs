//! Orca commands — server-side helpers reachable through OrcaOps (service-trait
//! impls in `mcp/*_service.rs`) and the legacy clap subcommands that haven't
//! been migrated yet.
//!
//! Most former `cmd_*` shims are gone — `orca <domain> <verb>` dispatches via
//! the OrcaOp inventory. What remains:
//! - `oauth` — GitHub/Atlassian OAuth flows used by `AuthService`.
//! - `daemon` — daemon lifecycle (not yet migrated; long-running supervisor).
//! - `hook_cmd` — Claude Code hook handlers (stdin-driven; different shape).
//! - `install` — install/uninstall report builders + REST status snapshot;
//!   shared by `LifecycleService` and `/api/system`.
//! - `mcp_cmd` — `mcp_sync_server` helper (shared with REST + service trait).
//! - `plugin_cmd` — `install_plugin` / `remove_plugin` helpers (shared).
//! - `creds_cmd` — `sync_plugin_creds` helper (shared).
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

pub mod creds_cmd;
pub mod daemon;
pub mod hook_cmd;
pub mod install;
pub mod mcp_cmd;
pub mod oauth;
pub mod plugin_cmd;
pub mod spec;
pub mod update;

pub use daemon::{DaemonAction, cmd_daemon};
pub use hook_cmd::{HookAction, cmd_hook};
pub use install::{InstallReport, install_status};
pub use mcp_cmd::mcp_sync_server;
pub use oauth::{
    cmd_logout_atlassian, cmd_logout_github, cmd_oauth_atlassian, cmd_oauth_github,
    load_atlassian_access_token, load_github_token,
};
pub use plugin_cmd::{install_plugin, remove_plugin};
pub use spec::{SpecAction, cmd_spec};
pub use update::startup_update_check;
