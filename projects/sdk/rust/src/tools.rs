//! Tools surface — plugins declare callable tools, host invokes them.
// Tool dispatch is type-erased at the protocol boundary: args, result, and
// input_schema are all free-form JSON mandated by the MCP tool-call contract.
#![allow(clippy::disallowed_types)]
//!
//! Sibling of the `types.declare` / `context.publish` surface in
//! [`crate::transport`]. Plugins that opt into `surfaces.mcp = true` in
//! their manifest must declare at least one tool via `orca/tools.declare`;
//! the host then dispatches `orca/tools.call` requests back to the plugin
//! over the same TCP+mTLS connection.
//!
//! ## Wire shape
//!
//! Plugin → host (request):
//! ```json
//! {"jsonrpc":"2.0","id":1,"method":"orca/tools.declare","params":{
//!   "tools":[
//!     {"name":"stack.list","description":"...","input_schema":{...},"sensitivity":"general"}
//!   ]
//! }}
//! ```
//! Result:
//! ```json
//! {"accepted":["dockge.stack.list"]}
//! ```
//! Tool ids are namespaced by the host as `<plugin_id>.<name>`. Plugins
//! declare bare names; the host owns the namespace.
//!
//! Host → plugin (request):
//! ```json
//! {"jsonrpc":"2.0","id":42,"method":"orca/tools.call","params":{
//!   "name":"stack.list","arguments":{}
//! }}
//! ```
//! Result:
//! ```json
//! {"result": <opaque JSON>}
//! ```
//! On failure the response carries a JSON-RPC error object — see
//! [`tool_error_codes`].

use crate::transport::Sensitivity;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ── Method names ──────────────────────────────────────────────────────────────

/// Method name for the plugin → host tool declaration request.
pub const TOOLS_DECLARE_METHOD: &str = "orca/tools.declare";

/// Method name for the host → plugin tool invocation request.
pub const TOOLS_CALL_METHOD: &str = "orca/tools.call";

/// Method name for plugin → host cross-plugin invocation. The plugin
/// supplies a fully-qualified tool name (`<plugin_id>.<name>`) and the host
/// resolves the owning peer + dispatches via the in-process registry.
pub const TOOLS_INVOKE_METHOD: &str = "orca/tools.invoke";

/// Method name for plugin → host peer enumeration. Returns the currently
/// connected peers and their declared versions so a plugin can fail fast
/// when an optional dep is missing or at an incompatible version.
pub const PLUGINS_LIST_METHOD: &str = "orca/plugins.list";

// ── Wire types ────────────────────────────────────────────────────────────────

/// One tool the plugin is announcing. The fully-qualified id is computed
/// host-side as `<plugin_id>.<name>` and must be unique within the plugin.
///
/// `input_schema` is a JSON Schema document the host validates incoming
/// arguments against before dispatching the call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDeclaration {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default = "default_sensitivity")]
    pub sensitivity: Sensitivity,
}

fn default_sensitivity() -> Sensitivity {
    Sensitivity::General
}

/// Params for `orca/tools.declare`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsDeclareParams {
    pub tools: Vec<ToolDeclaration>,
}

/// Result for `orca/tools.declare`. Lists the namespaced ids the host
/// registered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsDeclareResult {
    pub accepted: Vec<String>,
}

/// Params for `orca/tools.call`. `name` is the bare tool name as the
/// plugin declared it (no `<plugin_id>.` prefix — the host strips it
/// before dispatch).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// Result for `orca/tools.call`. Opaque JSON — semantics are tool-specific.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    pub result: Value,
}

/// Params for `orca/tools.invoke`. `name` is the fully-qualified peer tool,
/// e.g. `"graphql.query"`. `arguments` is forwarded verbatim to the peer's
/// `orca/tools.call`. `timeout_secs` overrides the host's default per-call
/// budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvokeParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Result for `orca/tools.invoke`. The opaque JSON the peer returned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvokeResult {
    pub result: Value,
}

/// One entry in `orca/plugins.list` — the host's view of a connected peer.
/// `version` mirrors what the peer announced via `orca/hello.plugin_version`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: String,
    pub version: String,
}

/// Result for `orca/plugins.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginsListResult {
    pub peers: Vec<PeerInfo>,
}

// ── Error codes ───────────────────────────────────────────────────────────────

/// JSON-RPC error codes specific to the tools surface. These extend the
/// standard `-32600..-32099` range. Plugin handlers signal failures via
/// these codes; the host translates `Result::Err` into the right code
/// based on the variant.
pub mod tool_error_codes {
    /// The named tool is not registered for this plugin.
    pub const UNKNOWN_TOOL: i64 = -32001;
    /// Arguments did not match the declared input_schema.
    pub const SCHEMA_VIOLATION: i64 = -32002;
    /// Handler ran but returned an application error (e.g. upstream API
    /// rejected the request, target resource not found, etc.).
    pub const HANDLER_ERROR: i64 = -32003;
}

// ── Handler trait ─────────────────────────────────────────────────────────────

/// Boxed future returned by a tool handler. Aliased so handler signatures
/// stay readable.
pub type ToolFuture = Pin<Box<dyn Future<Output = Result<Value, ToolHandlerError>> + Send>>;

/// Application-level error a handler can return. The transport translates
/// this into a JSON-RPC error response with code [`tool_error_codes::HANDLER_ERROR`].
#[derive(Debug)]
pub struct ToolHandlerError {
    pub message: String,
    pub data: Option<Value>,
}

impl std::fmt::Display for ToolHandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tool handler error: {}", self.message)
    }
}

impl std::error::Error for ToolHandlerError {}

impl ToolHandlerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            data: None,
        }
    }
    pub fn with_data(message: impl Into<String>, data: Value) -> Self {
        Self {
            message: message.into(),
            data: Some(data),
        }
    }
}

impl From<anyhow::Error> for ToolHandlerError {
    fn from(e: anyhow::Error) -> Self {
        Self::new(format!("{e:#}"))
    }
}

/// Trait every tool implementation satisfies. The blanket impl below means
/// callers can pass an async closure directly to `register_tool` — they
/// only implement this trait by hand for stateful handlers.
pub trait ToolHandler: Send + Sync + 'static {
    fn call(&self, args: Value) -> ToolFuture;
}

impl<F, Fut> ToolHandler for F
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, ToolHandlerError>> + Send + 'static,
{
    fn call(&self, args: Value) -> ToolFuture {
        Box::pin((self)(args))
    }
}

/// Convenience wrapper bundling a declaration with its handler. Stored
/// inside the transport's tool registry; not part of the wire format.
#[derive(Clone)]
pub struct RegisteredTool {
    pub declaration: ToolDeclaration,
    pub handler: Arc<dyn ToolHandler>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn declaration_roundtrips() {
        let d = ToolDeclaration {
            name: "stack.list".into(),
            description: "List Dockge stacks".into(),
            input_schema: json!({"type":"object","properties":{}}),
            sensitivity: Sensitivity::General,
        };
        let s = serde_json::to_string(&d).unwrap();
        let back: ToolDeclaration = serde_json::from_str(&s).unwrap();
        assert_eq!(back.name, "stack.list");
        assert_eq!(back.sensitivity, Sensitivity::General);
    }

    #[test]
    fn call_params_default_arguments_to_null() {
        let p: ToolCallParams = serde_json::from_str(r#"{"name":"foo"}"#).unwrap();
        assert_eq!(p.name, "foo");
        assert_eq!(p.arguments, Value::Null);
    }

    #[tokio::test]
    async fn closure_blanket_impl_dispatches() {
        let handler = |args: Value| async move { Ok(json!({"echoed": args})) };
        let h: Arc<dyn ToolHandler> = Arc::new(handler);
        let out = h.call(json!({"x":1})).await.unwrap();
        assert_eq!(out["echoed"]["x"], json!(1));
    }
}
