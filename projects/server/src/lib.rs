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
pub mod commands;
pub mod llm;

pub mod diagnostic;
pub mod mcp;
pub mod plugin_host;
pub mod serve;
pub mod services;
pub mod spec_detail;
