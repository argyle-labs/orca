use std::sync::Arc;

/// Shared context passed to every tool invocation.
///
/// Deliberately minimal: tools that need a DB connection open one via `db::open_default()`
/// directly (matching existing patterns). Expand this struct as shared state grows.
pub struct ToolCtx {
    pub config: Arc<config::Config>,
}

impl ToolCtx {
    pub fn new(config: Arc<config::Config>) -> Self {
        Self { config }
    }
}
