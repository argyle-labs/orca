//! Brain utilities — shared types, config, auth, logging, and filesystem tools.
//!
//! Modules:
//! - `auth`   — secure API key storage via OS keychain (keyring crate)
//! - `config` — `Config` loaded from `~/brain/config/brain.toml`; model/backend selection
//! - `ledger` — `TokenLedger` for tracking input/output token usage across a session
//! - `log`    — `SessionLog` JSONL writer; `search_logs`, `list_sessions`, `recall_session`
//! - `tools`  — filesystem and search helpers used by `ToolRegistry` (read/write/edit/glob/grep)
//! - `types`  — `Message`, `ToolCall`, `ToolResult`, `ToolDef`, `truncate_preview`

pub mod auth;
pub mod config;
pub mod ledger;
pub mod log;
pub mod state;
pub mod tools;
pub mod types;
