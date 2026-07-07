//! LLM backend types — message history, backend response, stop conditions.

use contract::{ToolCall, ToolResult};

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
        Message::User {
            content: content.into(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_user_constructs_correctly() {
        let m = Message::user("hello");
        assert!(matches!(m, Message::User { content } if content == "hello"));
    }

    #[test]
    fn message_user_accepts_string_and_str() {
        let from_str = Message::user("str input");
        let from_string = Message::user("str input".to_string());
        assert!(matches!(from_str, Message::User { .. }));
        assert!(matches!(from_string, Message::User { .. }));
    }

    #[test]
    fn stop_reason_default_is_end_turn() {
        assert_eq!(StopReason::default(), StopReason::EndTurn);
    }

    #[test]
    fn backend_response_default_is_empty() {
        let r = BackendResponse::default();
        assert!(r.text.is_empty());
        assert!(r.tool_calls.is_empty());
        assert_eq!(r.input_tokens, 0);
        assert_eq!(r.output_tokens, 0);
        assert_eq!(r.stop_reason, StopReason::EndTurn);
    }
}
