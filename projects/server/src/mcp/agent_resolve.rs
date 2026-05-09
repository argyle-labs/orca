//! Profile-aware agent prompt resolution.
//!
//! Search order:
//!   1. Active profile's `agents/` dir (per-user, mesh-syncable)
//!   2. `Config::agents_dir()` (dev override — currently `~/.claude/agents`)
//!   3. Embedded baseline (compiled into the binary)
//!
//! Profile lookup failures (DB unavailable, no active profile) degrade
//! gracefully to the existing two-tier path — never block agent loading.

use config::{Config, LOCAL_USER};
use std::path::PathBuf;

/// Compute the prioritized list of agent search dirs for the current user.
/// Always includes `Config::agents_dir()` as the dev-override fallback.
pub fn agent_search_dirs(config: &Config) -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(2);
    if let Some(profile_dir) = active_profile_agents_dir(config) {
        dirs.push(profile_dir);
    }
    dirs.push(config.agents_dir());
    dirs
}

/// Load an agent prompt using the profile-aware search path.
pub fn load_agent_prompt(name: &str, config: &Config) -> Option<String> {
    let dirs = agent_search_dirs(config);
    let refs: Vec<&std::path::Path> = dirs.iter().map(|p| p.as_path()).collect();
    orca_agents::load_agent_prompt_from_dirs(name, &refs)
}

fn active_profile_agents_dir(config: &Config) -> Option<PathBuf> {
    let conn = db::open(&config.db_path).ok()?;
    let mgr = profile::ProfileManager::from_config(config);
    let active = mgr.resolve_active(&conn, LOCAL_USER).ok().flatten()?;
    Some(active.agents_dir())
}
