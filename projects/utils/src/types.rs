//! Shared data types used across all brain crates.
//!
//! These are the canonical wire types — every backend and tool converts to/from these.
//! Keeping them in `brain-utils` (the leaf crate) prevents circular dependencies.

use serde::{Deserialize, Serialize};

/// Canonical internal message representation.
/// Each backend converts to/from its own wire format.
#[derive(Debug, Clone)]
pub enum Message {
    User {
        content: String,
    },
    Assistant {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    ToolResults(Vec<ToolResult>),
}

impl Message {
    /// Construct a user-role message.
    pub fn user(content: impl Into<String>) -> Self {
        Message::User {
            content: content.into(),
        }
    }
}

/// A model-requested tool invocation — name, id, and JSON input from the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Opaque identifier the model uses to correlate results back to this call.
    pub id: String,
    pub name: String,
    /// Raw JSON arguments as returned by the model.
    pub input: serde_json::Value,
}

/// The result of executing a tool, returned to the model in the next turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Must match the `id` from the originating `ToolCall`.
    pub tool_use_id: String,
    pub content: String,
    /// Set `true` when the tool failed — the model will see the error and can recover.
    pub is_error: bool,
}

/// What a backend returns after a full (streamed) response.
#[derive(Debug, Clone, Default)]
pub struct BackendResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub stop_reason: StopReason,
}

/// Why the model stopped generating — determines whether to execute tools or end the turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum StopReason {
    /// Normal completion — no tool calls, session can continue or end.
    #[default]
    EndTurn,
    /// Model emitted tool calls — execute them and send results back.
    ToolUse,
    /// Context window exhausted — may need to summarize or truncate history.
    MaxTokens,
}

/// Definition of a tool exposed to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input.
    pub input_schema: serde_json::Value,
}

/// Truncate a string to at most `max_chars` characters, appending "…" if truncated.
/// Safe for multi-byte UTF-8 — never slices mid-character.
pub fn truncate_preview(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}
