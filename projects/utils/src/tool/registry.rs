//! ToolRegistry — single source of truth for all registered OrcaTool impls.
//!
//! One registry instance drives:
//!   - MCP:  mcp_definitions() → tools/list,  dispatch() → tools/call
//!   - HTTP: axum_router() → one POST route per tool  (returns Router, caller mounts it)
//!   - CLI:  clap_command() + cli_dispatch() → `orca exec <name> [flags]`

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde_json::{Value, json};
use std::marker::PhantomData;
use std::sync::Arc;

use super::erased::{ErasedTool, ToolWrapper, value_to_text};
use super::{OrcaTool, ToolCtx};

pub struct ToolRegistry {
    tools: Vec<Box<dyn ErasedTool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Register a tool. Panics at startup (not runtime) if NAME collides with an existing tool.
    pub fn register<T: OrcaTool>(&mut self) -> &mut Self {
        let name = T::NAME;
        assert!(
            !self.tools.iter().any(|t| t.name() == name),
            "duplicate tool name: {name}"
        );
        self.tools.push(Box::new(ToolWrapper::<T>(PhantomData)));
        self
    }

    // ── MCP ──────────────────────────────────────────────────────────────────

    /// Build the JSON array for `tools/list`.
    pub fn mcp_definitions(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name(),
                    "description": t.description(),
                    "inputSchema": t.input_schema(),
                })
            })
            .collect()
    }

    /// Dispatch a `tools/call` by name, returning a structured JSON value.
    /// Returns `Err` for unknown tool names.
    pub async fn dispatch(&self, name: &str, args: Value, ctx: &ToolCtx) -> Result<Value> {
        match self.tools.iter().find(|t| t.name() == name) {
            Some(tool) => tool.run_json(args, ctx).await,
            None => anyhow::bail!("unknown tool: {name}"),
        }
    }

    /// Dispatch and render the result as plain text. MCP + CLI use this; REST
    /// + WASM use `dispatch` directly so they get the structured JSON.
    pub async fn dispatch_text(&self, name: &str, args: Value, ctx: &ToolCtx) -> Result<String> {
        let value = self.dispatch(name, args, ctx).await?;
        Ok(value_to_text(&value))
    }

    // ── HTTP / REST ──────────────────────────────────────────────────────────

    /// Build an axum router that exposes every registered tool as
    /// `POST /<name>` with a JSON body matching `input_schema()` and a JSON
    /// response matching `output_schema()`. The caller decides where to mount
    /// it (typically `.nest("/api/tools", reg.axum_router(ctx))`).
    pub fn axum_router(self: Arc<Self>, ctx: Arc<ToolCtx>) -> Router {
        // Single wildcard route — the path segment is the tool name. Keeping
        // it one route (vs N) means utoipa registration can be done later by
        // walking `self.tools`, without rewiring axum.
        Router::new()
            .route("/{name}", post(http_dispatch))
            .with_state(ToolHttpState {
                registry: self,
                ctx,
            })
    }
}

#[derive(Clone)]
struct ToolHttpState {
    registry: Arc<ToolRegistry>,
    ctx: Arc<ToolCtx>,
}

async fn http_dispatch(
    State(state): State<ToolHttpState>,
    Path(name): Path<String>,
    Json(args): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Unknown tool → 404; bad args / tool failure → 500 with the error body.
    if !state.registry.names().iter().any(|n| *n == name) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown tool: {name}") })),
        ));
    }
    state
        .registry
        .dispatch(&name, args, &state.ctx)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })
}

impl ToolRegistry {
    // ── CLI ───────────────────────────────────────────────────────────────────

    /// Returns all registered tool names — used to build CLI help text.
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// Execute a tool by name, accepting args as a JSON string or `key=value` pairs.
    ///
    /// Used by `orca exec <name> [--json '{...}' | key=value ...]`.
    pub async fn cli_dispatch(
        &self,
        name: &str,
        raw_args: CliArgs,
        ctx: &ToolCtx,
    ) -> Result<String> {
        let args_json = match raw_args {
            CliArgs::Json(s) => {
                serde_json::from_str(&s).map_err(|e| anyhow::anyhow!("invalid JSON args: {e}"))?
            }
            CliArgs::Pairs(pairs) => {
                let mut map = serde_json::Map::new();
                for pair in pairs {
                    let (k, v) = pair
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!("expected key=value, got: {pair}"))?;
                    // Try to parse the value as JSON first (handles booleans, numbers, etc.),
                    // fall back to plain string.
                    let val: Value =
                        serde_json::from_str(v).unwrap_or(Value::String(v.to_string()));
                    map.insert(k.to_string(), val);
                }
                Value::Object(map)
            }
        };
        self.dispatch_text(name, args_json, ctx).await
    }
}

/// How the CLI passes arguments to a tool.
pub enum CliArgs {
    /// `--json '{"mode":"hybrid"}'`
    Json(String),
    /// `mode=hybrid enabled=true`
    Pairs(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{OrcaTool, ToolCtx};
    use anyhow::Result;
    use async_trait::async_trait;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use std::sync::Arc;

    // ── Test tool implementations ─────────────────────────────────────────────

    #[derive(Deserialize, JsonSchema)]
    struct EchoArgs {
        message: String,
    }

    struct EchoTool;

    #[async_trait]
    impl OrcaTool for EchoTool {
        const NAME: &'static str = "echo";
        const DESCRIPTION: &'static str = "Echoes a message.";
        type Args = EchoArgs;
        type Output = String;
        async fn run(args: EchoArgs, _ctx: &ToolCtx) -> Result<String> {
            Ok(args.message)
        }
    }

    #[derive(Deserialize, JsonSchema)]
    struct AddArgs {
        a: i64,
        b: i64,
    }

    struct AddTool;

    #[async_trait]
    impl OrcaTool for AddTool {
        const NAME: &'static str = "add";
        const DESCRIPTION: &'static str = "Adds two numbers.";
        type Args = AddArgs;
        type Output = String;
        async fn run(args: AddArgs, _ctx: &ToolCtx) -> Result<String> {
            Ok((args.a + args.b).to_string())
        }
    }

    fn make_ctx() -> ToolCtx {
        use crate::config::{Config, Model};
        use std::path::PathBuf;
        ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/test.db"),
        }))
    }

    // ── Registration ──────────────────────────────────────────────────────────

    #[test]
    fn registry_registers_tool_and_names_shows_it() {
        let mut reg = ToolRegistry::new();
        reg.register::<EchoTool>();
        assert!(reg.names().contains(&"echo"));
    }

    #[test]
    fn registry_names_empty_when_no_tools_registered() {
        let reg = ToolRegistry::new();
        assert!(reg.names().is_empty());
    }

    #[test]
    #[should_panic(expected = "duplicate tool name")]
    fn registry_panics_on_duplicate_name() {
        let mut reg = ToolRegistry::new();
        reg.register::<EchoTool>();
        reg.register::<EchoTool>(); // duplicate
    }

    #[test]
    fn registry_multiple_tools_all_appear_in_names() {
        let mut reg = ToolRegistry::new();
        reg.register::<EchoTool>().register::<AddTool>();
        let names = reg.names();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"add"));
        assert_eq!(names.len(), 2);
    }

    // ── MCP definitions ───────────────────────────────────────────────────────

    #[test]
    fn mcp_definitions_includes_name_description_schema() {
        let mut reg = ToolRegistry::new();
        reg.register::<EchoTool>();
        let defs = reg.mcp_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0]["name"], "echo");
        assert_eq!(defs[0]["description"], "Echoes a message.");
        assert!(
            defs[0]["inputSchema"].is_object(),
            "inputSchema must be an object"
        );
    }

    #[test]
    fn mcp_definitions_schema_has_no_dollar_schema_key() {
        let mut reg = ToolRegistry::new();
        reg.register::<EchoTool>();
        let defs = reg.mcp_definitions();
        assert!(
            defs[0]["inputSchema"]["$schema"].is_null(),
            "$schema should be stripped"
        );
    }

    #[test]
    fn mcp_definitions_empty_when_no_tools() {
        let reg = ToolRegistry::new();
        assert!(reg.mcp_definitions().is_empty());
    }

    // ── dispatch ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_known_tool_returns_result() {
        let mut reg = ToolRegistry::new();
        reg.register::<EchoTool>();
        let ctx = make_ctx();
        let result = reg
            .dispatch("echo", serde_json::json!({"message": "hello"}), &ctx)
            .await
            .unwrap();
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_error() {
        let reg = ToolRegistry::new();
        let ctx = make_ctx();
        let err = reg
            .dispatch("ghost", serde_json::json!({}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown tool"), "got: {err}");
    }

    #[tokio::test]
    async fn dispatch_invalid_args_returns_error() {
        let mut reg = ToolRegistry::new();
        reg.register::<EchoTool>();
        let ctx = make_ctx();
        // Missing required "message" field
        let err = reg
            .dispatch("echo", serde_json::json!({}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid args"), "got: {err}");
    }

    #[tokio::test]
    async fn dispatch_add_tool_computes_correctly() {
        let mut reg = ToolRegistry::new();
        reg.register::<AddTool>();
        let ctx = make_ctx();
        let result = reg
            .dispatch("add", serde_json::json!({"a": 7, "b": 3}), &ctx)
            .await
            .unwrap();
        assert_eq!(result, "10");
    }

    // ── cli_dispatch ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn cli_dispatch_json_args_works() {
        let mut reg = ToolRegistry::new();
        reg.register::<EchoTool>();
        let ctx = make_ctx();
        let result = reg
            .cli_dispatch(
                "echo",
                CliArgs::Json(r#"{"message":"via json"}"#.into()),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result, "via json");
    }

    #[tokio::test]
    async fn cli_dispatch_pair_args_works() {
        let mut reg = ToolRegistry::new();
        reg.register::<EchoTool>();
        let ctx = make_ctx();
        let result = reg
            .cli_dispatch(
                "echo",
                CliArgs::Pairs(vec!["message=hello pairs".into()]),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result, "hello pairs");
    }

    #[tokio::test]
    async fn cli_dispatch_pair_numeric_coercion() {
        let mut reg = ToolRegistry::new();
        reg.register::<AddTool>();
        let ctx = make_ctx();
        let result = reg
            .cli_dispatch(
                "add",
                CliArgs::Pairs(vec!["a=5".into(), "b=3".into()]),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result, "8");
    }

    #[tokio::test]
    async fn cli_dispatch_pair_missing_equals_errors() {
        let mut reg = ToolRegistry::new();
        reg.register::<EchoTool>();
        let ctx = make_ctx();
        let err = reg
            .cli_dispatch("echo", CliArgs::Pairs(vec!["no-equals-here".into()]), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("expected key=value"), "got: {err}");
    }

    #[tokio::test]
    async fn cli_dispatch_invalid_json_errors() {
        let mut reg = ToolRegistry::new();
        reg.register::<EchoTool>();
        let ctx = make_ctx();
        let err = reg
            .cli_dispatch("echo", CliArgs::Json("{bad json".into()), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid JSON"), "got: {err}");
    }
}
