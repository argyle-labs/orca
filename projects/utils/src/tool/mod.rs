//! OrcaTool — define a capability once, expose it to MCP, HTTP, and CLI.
//!
//! Dependency rule: this crate imports only orca-config and orca-types (layer 0-1).
//! Tool *implementations* live in layer 3+ crates that import this one.

pub mod ctx;
pub mod erased;
pub mod registry;
pub mod types;

pub use ctx::ToolCtx;
pub use erased::ErasedTool;
pub use registry::ToolRegistry;
pub use types::{ToolCall, ToolDef, ToolResult};

use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

/// One capability: implement this trait and it is automatically available via MCP,
/// HTTP POST, and `orca exec <name>`. No other files to edit.
///
/// # Implementing
///
/// ```rust,ignore
/// #[derive(Deserialize, JsonSchema)]
/// pub struct Args { pub mode: String }
///
/// pub struct MyTool;
///
/// #[async_trait]
/// impl OrcaTool for MyTool {
///     const NAME: &'static str = "my_tool";
///     const DESCRIPTION: &'static str = "Does the thing.";
///     type Args = Args;
///     async fn run(args: Args, _ctx: &ToolCtx) -> Result<String> {
///         Ok(format!("mode={}", args.mode))
///     }
/// }
/// ```
#[async_trait]
pub trait OrcaTool: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;

    /// Args must be deserializable from JSON (for MCP + HTTP) and carry a JSON Schema
    /// (for tools/list and dynamic CLI flag generation).
    type Args: DeserializeOwned + JsonSchema + Send;

    async fn run(args: Self::Args, ctx: &ToolCtx) -> Result<String>;
}
