//! Shared test helpers for in-crate unit tests.
//!
//! `native`+`test` only — keeps domain test modules from each rebuilding the
//! same boilerplate ToolCtx.

use orca_utils::config::{Config, Model};
use orca_utils::tool::ToolCtx;
use std::path::PathBuf;
use std::sync::Arc;

/// Empty `ToolCtx` with a stubbed `Config`. Domain tests register their own
/// services on top via `ctx.register_service(...)`.
pub fn empty_ctx() -> ToolCtx {
    ToolCtx::new(Arc::new(Config {
        anthropic_api_key: None,
        lmstudio_url: String::new(),
        ollama_url: String::new(),
        default_model: Model::LMStudio {
            id: String::new(),
            url: String::new(),
        },
        app_dir: PathBuf::from("/tmp"),
        memory_root: PathBuf::from("/tmp"),
        db_path: PathBuf::from("/tmp/orca-tools-def-test.db"),
    }))
}
