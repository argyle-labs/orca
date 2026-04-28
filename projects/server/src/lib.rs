// Workspace crates — re-exported so existing `crate::` paths work unchanged
pub use brain_agents as agents;
pub use brain_docs as docs;
pub use brain_scanner as scanner;
pub use brain_utils::auth;
pub use brain_utils::config;
pub use brain_utils::ledger;
pub use brain_utils::log;
pub use brain_utils::types;

// Server-only modules
pub mod backend;
pub mod cmd;
pub mod context;
pub mod jobs;
pub mod mcp;
pub mod serve;
pub mod session;
pub mod tools;
pub mod tui;
