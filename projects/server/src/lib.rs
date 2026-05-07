#![recursion_limit = "256"]
//! Orca server binary — the interactive AI agent orchestrator.
//!
//! Modules:
//! - `context`  — project context resolution (memory, system prompt, working dir)
//! - `mcp`      — MCP stdio server exposing orca tools to Claude Code via JSON-RPC 2.0
//! - `serve`    — Axum HTTP server: REST API, OpenAPI spec, static frontend, middleware
//! - `session`  — interactive REPL + TUI, chat loop, tool execution, job management
//! - `tui`      — split-pane terminal UI (crossterm/ratatui), keybindings, layout

pub use ::conversation::agent_backend;
pub use ::conversation::context;
pub use ::conversation::conversation;
pub use ::conversation::tui;

pub mod log_cmd;
pub mod markdown;
pub mod mcp;
pub mod serve;
