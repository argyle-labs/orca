use crate::types::{BackendResponse, Message, ToolDef};
use anyhow::Result;
use async_trait::async_trait;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub mod claude;
pub mod lmstudio;

pub use claude::ClaudeBackend;
pub use lmstudio::LMStudioBackend;

/// A thread-safe write target for streaming output.
/// Foreground sessions pass stdout; background jobs pass a `Vec<u8>` buffer.
pub type OutputSink = Arc<Mutex<Box<dyn Write + Send>>>;

/// Create an OutputSink that writes to stdout.
pub fn stdout_sink() -> OutputSink {
    Arc::new(Mutex::new(Box::new(std::io::stdout())))
}

/// Create an OutputSink that writes to an in-memory buffer.
/// Returns (sink, buffer) — read the buffer after the job completes.
pub fn buffer_sink() -> (OutputSink, Arc<Mutex<Vec<u8>>>) {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = BufferWriter(buf.clone());
    (Arc::new(Mutex::new(Box::new(writer))), buf)
}

/// Write adapter that forwards into a shared `Vec<u8>`.
struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl Write for BufferWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut buf) = self.0.lock() {
            buf.extend_from_slice(data);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Helper: write formatted output to a sink (replaces print!/println! for redirectable output).
pub fn sink_write(sink: &OutputSink, data: &str) {
    if let Ok(mut w) = sink.lock() {
        let _ = w.write_all(data.as_bytes());
        let _ = w.flush();
    }
}

/// Helper: write formatted output to a sink with trailing newline.
pub fn sink_writeln(sink: &OutputSink, data: &str) {
    if let Ok(mut w) = sink.lock() {
        let _ = w.write_all(data.as_bytes());
        let _ = w.write_all(b"\n");
        let _ = w.flush();
    }
}

#[async_trait]
pub trait ModelBackend: Send + Sync {
    /// Send messages to the model, streaming tokens to the provided output sink.
    /// Returns the complete response once the stream ends.
    /// If cancel is triggered, streaming stops and partial response is returned.
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        system: &str,
        cancel: CancellationToken,
        output: &OutputSink,
    ) -> Result<BackendResponse>;

    /// Human-readable name for display.
    fn name(&self) -> &str;

    /// Model identifier for API calls.
    fn model_id(&self) -> &str;
}
