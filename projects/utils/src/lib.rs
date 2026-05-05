//! Orca utilities — shared cross-cutting types and config plumbing.
//!
//! Modules:
//! - `consts` — string constants (paths, app names)
//! - `config` — `Config` loaded from env + DB; model/backend selection
//! - `state`  — daemon mode state file
//! - `types`  — `Message`, `ToolCall`, `ToolResult`, `ToolDef`, `truncate_preview`
//!
//! These siblings live in their own crates:
//! - `orca-auth`   — API-key formatting helpers
//! - `orca-db`     — encrypted SQLite (SQLCipher)
//! - `orca-log`    — `SessionLog` JSONL writer + log search
//! - `orca-ledger` — `TokenLedger` token-usage accounting
//! - `orca-fs`     — filesystem and search helpers (read/write/edit/glob/grep)

pub mod consts;
pub mod config;
pub mod state;
pub mod types;
