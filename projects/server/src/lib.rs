#![recursion_limit = "256"]
//! Orca server binary — the interactive AI agent orchestrator.
//!
//! Modules:
//! - `context`  — project context resolution (memory, system prompt, working dir)
//! - `mcp`      — MCP stdio server exposing orca tools to Claude Code via JSON-RPC 2.0
//! - `serve`    — Axum HTTP server: REST API, OpenAPI spec, static frontend, middleware
//! - `session`  — interactive REPL + TUI, chat loop, tool execution, job management
//! - `tui`      — split-pane terminal UI (crossterm/ratatui), keybindings, layout

// Absorbed crates — each previously had its own workspace member.
pub mod agents;
pub mod commands;
pub mod conversation;
pub mod docs;
pub mod jobs;
pub mod llm;
pub mod profile;
pub mod scanner;

// Re-exports for compatibility with code that previously imported these
// at the orca-conversation crate root.
pub use crate::conversation::agent_backend;
pub use crate::conversation::context;
pub use crate::conversation::tui;

pub mod log_cmd;
pub mod markdown;
pub mod mcp;
pub mod plugin_host;
pub mod pod;
pub mod serve;
