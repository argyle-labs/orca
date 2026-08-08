#![recursion_limit = "256"]
//! Orca server binary — the interactive AI agent orchestrator.
//!
//! Remaining modules pending dissolution: `mcp` (stdio server) and `serve`
//! (Axum HTTP). The rest of the original server crate has been distributed
//! into domain crates.

// Plugin link force-include. Every plugin crate listed in Cargo.toml must
// be referenced somewhere in source or Rust's linker drops it as unused —
// which means its `#[orca_tool]` registrations never reach the inventory
// table that `dispatch::cli::build_root` walks. Without these `use _`
// statements, no plugin tools appear in CLI / MCP / REST surfaces, even
// though they compile fine on their own.
//
// Long-term, the orca-plugin-toolkit registration macro should emit a
// linker anchor automatically, so plugin authors never have to remember
// this. Until then, every new plugin gets a line here.
use agents as _;
use auth as _;
use files as _;
// dockge, homeassistant, and the *arr stack (sonarr/radarr/prowlarr/lidarr)
// live in external repos (~/code/{dockge,homeassistant,arr}). They run as
// subprocess plugins spawned by plugin-loader, like jellyfin/plex.
// `mcp` crate is already linked via `server/src/mcp/mod.rs::use ::mcp::*`,
// so no explicit force-include needed.
use namespace as _;
// ntfy lives in ~/code/ntfy (argyle-labs/ntfy) — runs as a subprocess plugin
// spawned by plugin-loader, like jellyfin/plex/nfs; no static force-link needed.
use orca_inventory as _;
use plugins as _;
use pod as _;
use spec as _;
use system as _;

pub mod daemon_client;
pub mod mcp;
pub mod serve;
pub mod spec_detail;
