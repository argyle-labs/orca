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
use database as _;
use docker as _;
use dockge as _;
use files as _;
use homeassistant as _;
use jellyfin as _;
use plex as _;
// `mcp` crate is already linked via `server/src/mcp/mod.rs::use ::mcp::*`,
// so no explicit force-include needed.
use namespace as _;
use ntfy as _;
use orca_inventory as _;
use plugins as _;
use pod as _;
use proxmox as _;
use spec as _;
use system as _;
use unraid as _;

pub mod mcp;
pub mod serve;
pub mod spec_detail;
