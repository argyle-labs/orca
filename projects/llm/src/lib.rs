pub mod backend;
pub mod resolve;
pub mod types;

pub use types::{BackendResponse, Message, StopReason};
pub use backend::{
    ModelBackend, OutputSink,
    build_backend, buffer_sink, sink_write, sink_writeln, stdout_sink,
    ClaudeBackend, LMStudioBackend,
};
