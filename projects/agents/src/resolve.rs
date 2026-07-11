//! Profile-aware agent prompt resolution.
//!
//! Search order:
//!   1. Active profile's `agents/` dir (per-user, mesh-syncable override)
//!   2. Embedded baseline (compiled into the binary)
//!
//! Agents are served to Claude Code via the orca-local MCP server
//! (list_agents / get_agent / run_agent) — there is no `~/.claude/agents`
//! filesystem fallback. Profile lookup failures (DB unavailable, no active
//! profile) degrade gracefully to the embedded baseline.

use contract::config::{Config, LOCAL_USER};
use std::path::PathBuf;

/// Compute the prioritized list of agent search dirs for the current user.
/// Returns the active profile's override dir if present; otherwise empty
/// (callers always append the embedded baseline as last resort).
pub fn agent_search_dirs(config: &Config) -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(1);
    if let Some(profile_dir) = active_profile_agents_dir(config) {
        dirs.push(profile_dir);
    }
    dirs
}

/// Load an agent prompt using the profile-aware search path.
pub fn load_agent_prompt(name: &str, config: &Config) -> Option<String> {
    let dirs = agent_search_dirs(config);
    let refs: Vec<&std::path::Path> = dirs.iter().map(|p| p.as_path()).collect();
    crate::embedded::load_agent_prompt_from_dirs(name, &refs)
}

fn active_profile_agents_dir(config: &Config) -> Option<PathBuf> {
    let conn = db::open(&config.db_path).ok()?;
    let mgr = namespace::NamespaceManager::from_config(config);
    let active = mgr.resolve_active(&conn, LOCAL_USER).ok().flatten()?;
    Some(active.agents_dir())
}
