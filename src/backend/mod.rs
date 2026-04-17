use crate::types::{BackendResponse, Message, ToolDef};
use anyhow::Result;
use async_trait::async_trait;

pub mod claude;
pub mod lmstudio;

pub use claude::ClaudeBackend;
pub use lmstudio::LMStudioBackend;

#[async_trait]
pub trait ModelBackend: Send + Sync {
    /// Send messages to the model, streaming tokens to stdout as they arrive.
    /// Returns the complete response once the stream ends.
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        system: &str,
    ) -> Result<BackendResponse>;

    /// Human-readable name for display.
    fn name(&self) -> &str;

    /// Model identifier for API calls.
    fn model_id(&self) -> &str;
}

/// Parse SSE stream lines, yielding "data: ..." payloads.
/// Filters out "[DONE]" and empty data lines.
pub fn sse_data_lines(chunk: &str) -> impl Iterator<Item = &str> {
    chunk
        .lines()
        .filter(|l| l.starts_with("data: "))
        .map(|l| &l[6..])
        .filter(|l| *l != "[DONE]" && !l.is_empty())
}
