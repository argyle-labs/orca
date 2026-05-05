//! Orca core — tool execution.
//!
//! Modules:
//! - `tools`   — `ToolRegistry`: tool definitions, dispatch, bash execution with permissions
//!
//! LLM backend types and implementations have moved to `orca-llm`.

pub mod tools;

// Re-export llm types for backward compatibility
pub use llm::{BackendResponse, Message, StopReason};
pub use llm::backend;
pub use llm::types;
