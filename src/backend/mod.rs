use crate::types::{BackendResponse, Message, ToolDef};
use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub mod claude;
pub mod lmstudio;

pub use claude::ClaudeBackend;
pub use lmstudio::LMStudioBackend;

#[async_trait]
pub trait ModelBackend: Send + Sync {
    /// Send messages to the model, streaming tokens to stdout as they arrive.
    /// Returns the complete response once the stream ends.
    /// If cancel is triggered, streaming stops and partial response is returned.
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        system: &str,
        cancel: CancellationToken,
    ) -> Result<BackendResponse>;

    /// Human-readable name for display.
    fn name(&self) -> &str;

    /// Model identifier for API calls.
    fn model_id(&self) -> &str;
}

