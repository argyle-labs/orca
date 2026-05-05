//! LLM backend types — message history, backend response, stop conditions.

pub use tool::{ToolCall, ToolDef, ToolResult};

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
    pub fn user(content: impl Into<String>) -> Self {
        Message::User { content: content.into() }
    }
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

/// Why the model stopped generating.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum StopReason {
    #[default]
    EndTurn,
    ToolUse,
    MaxTokens,
}
