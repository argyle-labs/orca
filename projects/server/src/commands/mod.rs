//! Orca commands — CLI subcommand handlers.
//!
//! Each module owns one or more `cmd_*` functions called directly from `main.rs`.
//! Commands that require `Session` or `ProjectContext` live in `main.rs` instead
//! (those need the server crate which commands cannot import).
//!
//! Modules:
//! - `agents`   — list available agents
//! - `auth`     — login/logout/auth helpers (called by AuthService impl;
//!   the `orca auth …` CLI itself dispatches via OrcaOp)
//! - `daemon`   — daemon lifecycle: status/stop/park/reclaim/install/uninstall
//! - `doctor`   — validate agent files, symlinks, config, and tool availability
//! - `mcp_cmd`  — MCP registry subcommands: add/remove/list external MCP servers
//! - `projects` — list projects from orca vault memory directory
//! - `spec`     — manage external OpenAPI spec registry (add, remove, list, refresh)
//! - `update`   — self-update from GitHub releases; startup update check
//! - `oauth`    — GitHub device flow + Atlassian PKCE OAuth; token keychain storage
//! - `plugin_cmd` — plugin registry: add/remove/list/enable/disable from orca-plugin.toml
//! - `pki_cmd`   — PKI: init CA, issue plugin certs, list issued certs

// Slash command prompts embedded at build time.
include!(concat!(env!("OUT_DIR"), "/embedded_commands.rs"));

/// List all embedded slash commands as `/name` strings.
pub fn list_embedded_commands() -> Vec<String> {
    embedded_command_names()
        .iter()
        .map(|name| format!("/{name}"))
        .collect()
}

pub mod agents;
pub mod auth;
pub mod creds_cmd;
pub mod daemon;
pub mod db_cmd;
pub mod doctor;
pub mod hook_cmd;
pub mod install;
pub mod mcp_cmd;
pub mod oauth;
pub mod pki_cmd;
pub mod plugin_cmd;
pub mod profile_cmd;
pub mod projects;
pub mod spec;
pub mod update;

pub use agents::cmd_agents;
pub use auth::{cmd_auth, cmd_login, cmd_logout};
pub use daemon::{DaemonAction, cmd_daemon};
pub use db_cmd::{DbAction, cmd_db};
pub use doctor::cmd_doctor;
pub use hook_cmd::{HookAction, cmd_hook};
pub use install::{InstallReport, cmd_install, cmd_uninstall, install_status};
// mcp_cmd retains shared helpers (mcp_sync_server) used by REST + service traits;
// the CLI shim (McpAction/cmd_mcp) is gone — `orca mcp <verb>` goes through OrcaOp.
pub use mcp_cmd::mcp_sync_server;
pub use oauth::{
    cmd_logout_atlassian, cmd_logout_github, cmd_oauth_atlassian, cmd_oauth_github,
    load_atlassian_access_token, load_github_token,
};
pub use pki_cmd::{PkiAction, cmd_pki};
// plugin_cmd retains install_plugin/remove_plugin helpers used by REST + service traits.
pub use plugin_cmd::{install_plugin, remove_plugin};
pub use profile_cmd::{ProfileAction, cmd_profile};
pub use projects::cmd_projects;
pub use spec::{SpecAction, cmd_spec};
pub use update::{cmd_update, startup_update_check};
