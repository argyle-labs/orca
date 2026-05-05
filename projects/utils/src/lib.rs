//! Orca shared constants — paths, app names. Anything heavier lives in its own crate.
//!
//! Sibling crates extracted out of utils:
//! - `orca-auth`   — API-key formatting helpers
//! - `orca-config` — runtime `Config` loaded from env + DB
//! - `orca-db`     — encrypted SQLite (SQLCipher)
//! - `orca-fs`     — filesystem and search helpers
//! - `orca-ledger` — `TokenLedger` token-usage accounting
//! - `orca-log`    — `SessionLog` JSONL writer + log search
//! - `orca-state`  — daemon mode state file
//! - `orca-types`  — cross-cutting `Message`, `ToolCall`, `ToolResult`, etc.

pub mod consts;
