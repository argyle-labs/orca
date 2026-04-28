//! Brain server binary — the interactive AI agent orchestrator.
//!
//! Modules:
//! - `context`  — project context resolution (memory, system prompt, working dir)
//! - `mcp`      — MCP stdio server exposing brain tools to Claude Code via JSON-RPC 2.0
//! - `serve`    — Axum HTTP server: REST API, OpenAPI spec, static frontend, middleware
//! - `session`  — interactive REPL + TUI, chat loop, tool execution, job management
//! - `tui`      — split-pane terminal UI (crossterm/ratatui), keybindings, layout

pub mod context;
pub mod mcp;
pub mod serve;
pub mod session;
pub mod tui;
