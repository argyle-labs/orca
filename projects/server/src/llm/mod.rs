pub mod backend;
pub mod discovery;
pub mod resolve;
pub mod tools;
pub mod types;

/// Install ring as the rustls process-default crypto provider.
///
/// Reqwest is built with `rustls-no-provider` (to avoid pulling in aws-lc-sys);
/// without a process-default provider, the first reqwest client construction
/// panics with "No provider set". Each backend constructor calls this so both
/// production and test callers work without any explicit setup.
///
/// Idempotent: subsequent calls are no-ops (the first install wins).
pub fn ensure_crypto_provider() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

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
