//! Model backend abstraction for the orca binary.
//!
//! `ModelBackend` is the core trait all LLM clients implement.
//! `build_backend()` is the factory — everything else in the codebase interacts
//! only with `Box<dyn ModelBackend>`, never with concrete backend types.
//!
//! `OutputSink` is the streaming output target: stdout for interactive sessions,
//! a memory buffer (`buffer_sink`) for background jobs.

use crate::types::{BackendResponse, Message};
use anyhow::{Context, Result};
use contract::ToolDef;
use contract::config::{Config, Model};
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Boxed, `Send` future — the hand-desugared return type for async trait
/// methods (the `async_trait` macro is banned workspace-wide). Mirrors the
/// `service` crate's `BoxFuture`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub mod claude;
pub mod lmstudio;
pub mod ollama;
pub mod serialize;

pub use claude::ClaudeBackend;
pub use lmstudio::LMStudioBackend;
pub use ollama::OllamaBackend;

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
        _ = w.write_all(data.as_bytes());
        _ = w.flush();
    }
}

/// Helper: write formatted output to a sink with trailing newline.
pub fn sink_writeln(sink: &OutputSink, data: &str) {
    if let Ok(mut w) = sink.lock() {
        _ = w.write_all(data.as_bytes());
        _ = w.write_all(b"\n");
        _ = w.flush();
    }
}

pub trait ModelBackend: Send + Sync {
    /// Send messages to the model, streaming tokens to the provided output sink.
    /// Returns the complete response once the stream ends.
    /// If cancel is triggered, streaming stops and partial response is returned.
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDef],
        system: &'a str,
        cancel: CancellationToken,
        output: &'a OutputSink,
    ) -> BoxFuture<'a, Result<BackendResponse>>;

    /// Human-readable name for display.
    fn name(&self) -> &str;

    /// Model identifier for API calls.
    fn model_id(&self) -> &str;

    /// Whether this backend supports tool/function calling.
    /// Local models that don't reliably handle tool schemas should return false.
    fn supports_tools(&self) -> bool {
        true
    }

    /// Whether this is a local model backend (LM Studio, Ollama).
    /// Cloud backends (Claude) return false, which enables the full Wolf persona prompt.
    /// Local backends get a stripped-down prompt — no Otter narration, no agent routing.
    fn is_local(&self) -> bool {
        true
    }
}

/// Construct the correct backend from config and model selection.
pub fn build_backend(config: &Config, model: &Model) -> Result<Box<dyn ModelBackend>> {
    match model {
        Model::Claude(id) => {
            let key = config
                .anthropic_api_key
                .clone()
                .context("no API key — run `orca login` to store one")?;
            Ok(Box::new(ClaudeBackend::new(key, id)))
        }
        Model::LMStudio { id, url } => {
            let base = if url.is_empty() {
                &config.lmstudio_url
            } else {
                url
            };
            Ok(Box::new(LMStudioBackend::new(base, id)))
        }
        Model::Ollama { id, url } => {
            let base = if url.is_empty() {
                &config.ollama_url
            } else {
                url
            };
            Ok(Box::new(OllamaBackend::new(base, id)))
        }
    }
}
