//! Orca core — model backends and tool execution.
//!
//! Modules:
//! - `backend` — `ModelBackend` trait + Claude (Anthropic) and LM Studio (OpenAI-compat) impls,
//!               streaming SSE parser, output sink abstraction, `build_backend` factory
//! - `tools`   — `ToolRegistry`: tool definitions, dispatch, bash execution with permissions

pub mod backend;
pub mod tools;
