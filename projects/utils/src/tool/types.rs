//! Tool protocol types — the contract between LLM backends and tool execution.

use serde::{Deserialize, Serialize};

/// A model-requested tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Opaque identifier the model uses to correlate results back to this call.
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// The result of executing a tool, returned to the model in the next turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Must match the `id` from the originating `ToolCall`.
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

/// Definition of a tool exposed to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}
