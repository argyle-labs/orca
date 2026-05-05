pub mod backend;
pub mod discovery;
pub mod resolve;
pub mod tools;
pub mod types;

pub use types::{BackendResponse, Message, StopReason};
pub use backend::{
    ModelBackend, OutputSink,
    build_backend, buffer_sink, sink_write, sink_writeln, stdout_sink,
    ClaudeBackend, LMStudioBackend,
};
pub use discovery::{
    TaskKind, ModelCapabilities, DiscoveredModel,
    classify_model, discover_all, select_for_task, to_config_model,
};
pub use resolve::{resolve_model, estimate_context_window};
