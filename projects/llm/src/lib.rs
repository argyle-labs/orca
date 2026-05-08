pub mod backend;
pub mod discovery;
pub mod resolve;
pub mod tools;
pub mod types;

pub use backend::{
    ClaudeBackend, LMStudioBackend, ModelBackend, OllamaBackend, OutputSink, buffer_sink,
    build_backend, sink_write, sink_writeln, stdout_sink,
};
pub use discovery::{
    DiscoveredModel, ModelCapabilities, TaskKind, classify_model, discover_all, select_for_task,
    to_config_model,
};
pub use resolve::{estimate_context_window, resolve_model};
pub use types::{BackendResponse, Message, StopReason};
