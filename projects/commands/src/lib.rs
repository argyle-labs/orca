//! Brain commands — CLI subcommand handlers.
//!
//! Each module owns one or more `cmd_*` functions called directly from `main.rs`.
//! Commands that require `Session` or `ProjectContext` live in `main.rs` instead
//! (those need the server crate which commands cannot import).
//!
//! Modules:
//! - `agents`   — list available agents, install embedded agents into ~/.claude/agents/
//! - `auth`     — login/logout/auth (keychain read/write via brain_utils::auth)
//! - `codegen`  — `brain gen`: fetch OpenAPI spec, run openapi-typescript codegen
//! - `daemon`   — daemon lifecycle: status/stop/park/reclaim/install/uninstall
//! - `doctor`   — validate agent files, symlinks, config, and tool availability
//! - `log_cmd`  — session log subcommands: list, search, recall, tail
//! - `mcp_cmd`  — MCP registry subcommands: add/remove/list external MCP servers
//! - `projects` — list projects from brain vault memory directory
//! - `spec`     — manage external OpenAPI spec registry (add, remove, list, refresh)
//! - `update`   — self-update from GitHub releases; startup update check
//! - `oauth`    — GitHub device flow + Atlassian PKCE OAuth; token keychain storage

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
pub mod codegen;
pub mod daemon;
pub mod doctor;
pub mod install;
pub mod log_cmd;
pub mod mcp_cmd;
pub mod projects;
pub mod spec;
pub mod oauth;
pub mod update;

pub use spec::{SpecAction, cmd_spec};
pub use log_cmd::{LogAction, cmd_log};
pub use auth::{cmd_login, cmd_logout, cmd_auth};
pub use agents::{cmd_agents, cmd_install_agents};
pub use daemon::{DaemonAction, cmd_daemon};
pub use doctor::cmd_doctor;
pub use install::{cmd_install, cmd_uninstall, install_status, InstallReport};
pub use projects::cmd_projects;
pub use codegen::cmd_gen;
pub use mcp_cmd::{McpAction, cmd_mcp};
pub use update::{cmd_update, startup_update_check};
pub use oauth::{cmd_oauth_github, cmd_oauth_atlassian, cmd_logout_github, cmd_logout_atlassian, load_github_token, load_atlassian_access_token};
