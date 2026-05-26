//! Free-function dispatchers over the `#[orca_tool]` inventory slice.
//!
//! One inventory walk drives:
//!   - MCP:  `mcp_definitions()` → tools/list, `dispatch()` → tools/call
//!   - HTTP: `axum_router(ctx)` → one POST route per tool (caller mounts)
//!   - CLI:  `clap_command()` + `cli_dispatch()` → `orca exec <name> [flags]`
//!
//! Each entry's `make_erased` closure is invoked once and the resulting
//! `Box<dyn ErasedTool>` cached in a process-global `OnceLock`. Subsequent
//! lookups hit a `HashMap<&'static str, usize>` rather than a linear scan.
//!
//! `serde_json::Value` is the tool dispatch protocol — args and outputs
//! cross the type-erased ErasedTool boundary as Value. This is deliberate:
//! dispatch is a multiplexer over many typed tools and cannot know
//! concrete types at compile time. Callers downcast via serde immediately
//! after dispatch.
#![allow(clippy::disallowed_types)]

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::erased::{ErasedTool, value_to_text};
use crate::inventory_slice::ToolRegistration;
use orca_contract::ToolCtx;

// ── Cache ────────────────────────────────────────────────────────────────────

struct ToolCache {
    /// Stable order from inventory walk; used for `mcp_definitions` /
    /// `names` / `role_table` so output is deterministic per build.
    ordered: Vec<Box<dyn ErasedTool>>,
    /// Name → index into `ordered`. `dispatch` and friends look up here.
    by_name: HashMap<&'static str, usize>,
}

static CACHE: OnceLock<ToolCache> = OnceLock::new();

fn cache() -> &'static ToolCache {
    CACHE.get_or_init(|| {
        let mut ordered: Vec<Box<dyn ErasedTool>> = Vec::new();
        let mut by_name: HashMap<&'static str, usize> = HashMap::new();
        for entry in inventory::iter::<ToolRegistration> {
            assert!(
                !by_name.contains_key(entry.name),
                "duplicate tool name: {}",
                entry.name
            );
            let tool = (entry.make_erased)();
            by_name.insert(entry.name, ordered.len());
            ordered.push(tool);
        }
        ToolCache { ordered, by_name }
    })
}

fn find(name: &str) -> Option<&'static dyn ErasedTool> {
    let c = cache();
    c.by_name.get(name).map(|i| c.ordered[*i].as_ref())
}

// ── MCP ──────────────────────────────────────────────────────────────────────

/// Build the JSON array for `tools/list`.
pub fn mcp_definitions() -> Vec<Value> {
    cache()
        .ordered
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
pub async fn dispatch(name: &str, args: Value, ctx: &ToolCtx) -> Result<Value> {
    match find(name) {
        Some(tool) => tool.run_json(args, ctx).await,
        None => anyhow::bail!("unknown tool: {name}"),
    }
}

/// Dispatch and render the result as plain text. MCP + CLI use this; REST
/// + WASM use `dispatch` directly so they get the structured JSON.
pub async fn dispatch_text(name: &str, args: Value, ctx: &ToolCtx) -> Result<String> {
    let value = dispatch(name, args, ctx).await?;
    Ok(value_to_text(&value))
}

// ── HTTP / REST ──────────────────────────────────────────────────────────────

/// Build an axum router that exposes every registered tool as
/// `POST /<name>` with a JSON body matching `input_schema()` and a JSON
/// response matching `output_schema()`. The caller decides where to mount
/// it (typically `.nest("/api/tools", axum_router(ctx))`).
pub fn axum_router(ctx: Arc<ToolCtx>) -> Router {
    // Single wildcard route — the path segment is the tool name.
    Router::new()
        .route("/{name}", post(http_dispatch))
        .with_state(ToolHttpState { ctx })
}

#[derive(Clone)]
struct ToolHttpState {
    ctx: Arc<ToolCtx>,
}

async fn http_dispatch(
    State(state): State<ToolHttpState>,
    Path(name): Path<String>,
    Json(args): Json<Value>,
) -> std::result::Result<Json<Value>, (StatusCode, Json<Value>)> {
    if find(&name).is_none() {
        let oe = orca_contract::OrcaError::not_found(format!("unknown tool: {name}"))
            .with_code("tool.unknown");
        return Err(orca_error_response(oe));
    }
    dispatch(&name, args, &state.ctx)
        .await
        .map(Json)
        .map_err(|e| {
            if let Some(oe) = e.downcast_ref::<orca_contract::OrcaError>() {
                let kind = oe.kind;
                let body = serde_json::to_value(oe).unwrap_or_else(
                    |_| json!({ "kind": "internal", "message": "serialize failure" }),
                );
                let status = StatusCode::from_u16(kind.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return (status, Json(body));
            }
            let oe = orca_contract::OrcaError::internal(e.to_string());
            orca_error_response(oe)
        })
}

fn orca_error_response(oe: orca_contract::OrcaError) -> (StatusCode, Json<Value>) {
    let status =
        StatusCode::from_u16(oe.kind.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = serde_json::to_value(&oe)
        .unwrap_or_else(|_| json!({ "kind": "internal", "message": oe.message }));
    (status, Json(body))
}

// ── Introspection ────────────────────────────────────────────────────────────

/// Returns all registered tool names — used to build CLI help text.
pub fn names() -> Vec<&'static str> {
    cache().ordered.iter().map(|t| t.name()).collect()
}

/// Names of every registered tool whose `OrcaToolDef::REMOTE_OK` is true.
/// Used to populate the static allowlist for `pod/exec` dispatch.
pub fn remote_ok_names() -> Vec<&'static str> {
    cache()
        .ordered
        .iter()
        .filter(|t| t.remote_ok())
        .map(|t| t.name())
        .collect()
}

/// `(name, required_role)` pairs for every registered tool. Used to install
/// the process-global role lookup the REST middleware consults to gate
/// `/api/tools/*` invocations.
pub fn role_table() -> Vec<(&'static str, &'static str)> {
    cache()
        .ordered
        .iter()
        .map(|t| (t.name(), t.required_role()))
        .collect()
}

/// Required role for a single tool, or `None` if no such tool is registered.
pub fn required_role(name: &str) -> Option<&'static str> {
    find(name).map(|t| t.required_role())
}

// ── CLI ──────────────────────────────────────────────────────────────────────

/// Mirror of `ToolRegistry::clap_command` — returns the clap command tree
/// built from `register_op!` ops. `cli::build_root` is the real entry
/// point used by `orca`'s main binary; this helper exists so embedders
/// outside the binary can construct the same tree without depending on
/// the `cli` module directly.
pub fn clap_command() -> clap::Command {
    crate::cli::build_root(clap::Command::new("orca"))
}

/// How the CLI passes arguments to a tool.
pub enum CliArgs {
    /// `--json '{"mode":"hybrid"}'`
    Json(String),
    /// `mode=hybrid enabled=true`
    Pairs(Vec<String>),
}

/// Execute a tool by name, accepting args as a JSON string or `key=value`
/// pairs. Used by `orca exec <name> [--json '{...}' | key=value ...]`.
pub async fn cli_dispatch(name: &str, raw_args: CliArgs, ctx: &ToolCtx) -> Result<String> {
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
                let val: Value = serde_json::from_str(v).unwrap_or(Value::String(v.to_string()));
                map.insert(k.to_string(), val);
            }
            Value::Object(map)
        }
    };
    dispatch_text(name, args_json, ctx).await
}

#[cfg(test)]
mod tests {
    //! Unit tests for the free-fn dispatchers.
    //!
    //! The process-global cache is shared across tests and built from the
    //! `inventory` slice of this binary. This crate ships no `#[orca_tool]`
    //! definitions, so the cache is empty in unit tests — the inventory
    //! walk + populated-dispatch path is exercised in `fleet::inventory_tests`
    //! where every domain crate is linked in.
    use super::*;
    use crate::erased::ToolWrapper;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_contract::{OrcaTool, OrcaToolDef, ToolCtx};
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use std::marker::PhantomData;
    use std::sync::Arc;

    #[derive(Deserialize, Serialize, JsonSchema)]
    struct EchoArgs {
        message: String,
    }

    struct EchoTool;

    impl OrcaToolDef for EchoTool {
        const NAME: &'static str = "echo.local";
        const DESCRIPTION: &'static str = "Echoes a message.";
        const REQUIRED_ROLE: &'static str = "admin";
        type Args = EchoArgs;
        type Output = String;
    }

    #[async_trait]
    impl OrcaTool for EchoTool {
        async fn run(args: EchoArgs, _ctx: &ToolCtx) -> Result<String> {
            Ok(args.message)
        }
    }

    fn make_ctx() -> ToolCtx {
        use orca_utils::config::{Config, Model};
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
            ports: Default::default(),
        }))
    }

    #[tokio::test]
    async fn erased_wrapper_round_trips_via_run_json() {
        let w = ToolWrapper::<EchoTool>(PhantomData);
        let e: &dyn ErasedTool = &w;
        let v = e
            .run_json(serde_json::json!({"message": "hi"}), &make_ctx())
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!("hi"));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_error() {
        let err = dispatch("ghost.tool", serde_json::json!({}), &make_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown tool"));
    }

    #[tokio::test]
    async fn cli_dispatch_unknown_tool_errors() {
        let err = cli_dispatch("ghost.tool", CliArgs::Json("{}".into()), &make_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown tool"), "got: {err}");
    }

    #[tokio::test]
    async fn cli_dispatch_invalid_json_errors() {
        let err = cli_dispatch("ghost.tool", CliArgs::Json("{bad".into()), &make_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid JSON"), "got: {err}");
    }

    #[tokio::test]
    async fn cli_dispatch_pair_missing_equals_errors() {
        let err = cli_dispatch(
            "ghost.tool",
            CliArgs::Pairs(vec!["no-equals".into()]),
            &make_ctx(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("expected key=value"), "got: {err}");
    }

    #[test]
    fn introspection_helpers_dont_panic_on_empty_inventory() {
        let _ = names();
        let _ = remote_ok_names();
        let _ = role_table();
        assert!(required_role("ghost.tool").is_none());
        let _ = mcp_definitions();
    }

    #[tokio::test]
    async fn http_dispatch_returns_404_for_unknown_tool() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request as AxumReq;
        use tower::ServiceExt;

        let router = axum_router(Arc::new(make_ctx()));
        let req = AxumReq::builder()
            .method("POST")
            .uri("/ghost.tool")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("unknown tool"));
    }
}
