//! Shared test helpers for in-crate unit tests.
//! `native`+`test` only.

use orca_tool::ToolCtx;
use orca_utils::config::{Config, Model};
use std::path::PathBuf;
use std::sync::Arc;

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
        db_path: PathBuf::from("/tmp/orca-tools-platform-test.db"),
        ports: Default::default(),
    }))
}
