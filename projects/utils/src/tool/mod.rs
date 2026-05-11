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
use serde::Serialize;
use serde::de::DeserializeOwned;

/// One capability: implement this trait and it is automatically available via
/// MCP, REST (`/api/tools/<name>`), CLI (`orca exec <name>`), and the WASM
/// client (`orcaClient.<name>(args)`). No other files to edit.
///
/// `Args` and `Output` are both `JsonSchema` so each surface can emit the
/// right typed wrapper: utoipa for REST/OpenAPI, MCP `tools/list`, CLI flags,
/// and wasm-bindgen `.d.ts` types for the frontend.
///
/// # Implementing
///
/// ```rust,ignore
/// #[derive(Deserialize, JsonSchema)]
/// pub struct Args { pub mode: String }
///
/// #[derive(Serialize, JsonSchema)]
/// pub struct Output { pub mode: String, pub applied: bool }
///
/// pub struct MyTool;
///
/// #[async_trait]
/// impl OrcaTool for MyTool {
///     const NAME: &'static str = "my_tool";
///     const DESCRIPTION: &'static str = "Does the thing.";
///     type Args = Args;
///     type Output = Output;
///     async fn run(args: Args, _ctx: &ToolCtx) -> Result<Output> {
///         Ok(Output { mode: args.mode, applied: true })
///     }
/// }
/// ```
#[async_trait]
pub trait OrcaTool: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;

    /// Args must be deserializable from JSON (for MCP + REST + WASM) and carry
    /// a JSON Schema (for tools/list, OpenAPI, CLI flag generation, and TS
    /// type emission).
    type Args: DeserializeOwned + JsonSchema + Send;

    /// Output must be serializable to JSON (for REST + WASM) and carry a JSON
    /// Schema (for OpenAPI response bodies + TS type emission). Use `String`
    /// when the tool genuinely returns human-readable text.
    type Output: Serialize + JsonSchema + Send + 'static;

    async fn run(args: Self::Args, ctx: &ToolCtx) -> Result<Self::Output>;
}
