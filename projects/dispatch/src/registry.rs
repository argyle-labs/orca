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
    Extension, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};

/// Per-request header that routes a `/api/v1/<name>` call to a remote
/// peer over the pod mesh. Mirrors the CLI `--peer <h>` flag — same
/// universal opt-out (local_only tools reject), same `ToolCtx::peer_target`
/// pathway. The web UI sets this header on per-peer actions like
/// "update this system" so the same REST surface that does local work also
/// drives the fleet.
const PEER_HEADER: &str = "x-orca-peer";
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::erased::{ErasedTool, value_to_text};
use crate::inventory_slice::ToolRegistration;
use contract::ToolCtx;

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
            // Dedup by name rather than panicking. A `cdylib` built with
            // `crate-type = ["cdylib", "rlib"]` (the orca plugin artifact shape)
            // emits the `inventory` link-section entries twice — once from each
            // crate-type's object set — so a plugin's own `tool_manifest_json()`
            // would otherwise see every tool twice. This walk runs inside the
            // plugin's `extern "C"` `manifest()` accessor, where a panic cannot
            // unwind across FFI and would abort the host process — exactly the
            // UB-equivalent the abi contract forbids. First registration wins;
            // genuinely conflicting names are a build-time concern, not a
            // runtime abort.
            if by_name.contains_key(entry.name) {
                continue;
            }
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

// ── Dynamic (cdylib-plugin) fallback hook ──────────────────────────────────────
//
// `dispatch` knows only the statically-linked `inventory` registry. Runtime
// cdylib plugins live in `plugin-loader`'s registry, which `dispatch` cannot
// depend on (plugin-loader → dispatch). To keep one tool namespace across REST
// / MCP / CLI without a dependency cycle, the host installs a *fallback* here
// at startup: a synchronous `(name, args) -> Option<Result<Value>>` that returns
// `Some` iff a loaded plugin owns the name. `dispatch` consults it on a miss.
// This inverts the dependency — `dispatch` holds a fn pointer the server wires —
// rather than re-exporting plugin-loader.

/// Returns `Some(result)` iff a dynamically-loaded plugin owns `name`.
type DynamicInvoker = dyn Fn(&str, &Value) -> Option<Result<Value>> + Send + Sync;

/// Returns the JSON tool defs (`{name, description, input_schema, output_schema}`)
/// of every dynamically-loaded plugin, for merging into list surfaces.
type DynamicDefs = dyn Fn() -> Vec<Value> + Send + Sync;

static DYNAMIC_INVOKER: OnceLock<Box<DynamicInvoker>> = OnceLock::new();
static DYNAMIC_DEFS: OnceLock<Box<DynamicDefs>> = OnceLock::new();

/// Install the cdylib-plugin fallback. Called once by the host at startup after
/// the plugin install-dir scan. `invoke` routes a tool call into the loaded
/// plugin registry; `defs` reports loaded-plugin tool defs for list surfaces.
/// Idempotent: a second call is ignored (the `OnceLock` keeps the first).
pub fn set_dynamic_dispatch(invoke: Box<DynamicInvoker>, defs: Box<DynamicDefs>) {
    if DYNAMIC_INVOKER.set(invoke).is_err() {
        tracing::warn!("dynamic dispatch fallback already installed; ignoring second install");
    }
    if DYNAMIC_DEFS.set(defs).is_err() {
        tracing::warn!("dynamic tool defs already installed; ignoring second install");
    }
}

/// Try the installed dynamic fallback for `name`. `None` when no fallback is
/// installed or no loaded plugin owns the name.
fn dynamic_dispatch(name: &str, args: &Value) -> Option<Result<Value>> {
    DYNAMIC_INVOKER.get().and_then(|f| f(name, args))
}

/// JSON tool defs contributed by loaded cdylib plugins. Empty when no fallback
/// is installed.
pub fn dynamic_tool_defs() -> Vec<Value> {
    DYNAMIC_DEFS.get().map(|f| f()).unwrap_or_default()
}

/// True iff a loaded cdylib plugin owns `name` (via the installed fallback).
fn dynamic_owns(name: &str) -> bool {
    dynamic_tool_defs()
        .iter()
        .any(|d| d.get("name").and_then(|n| n.as_str()) == Some(name))
}

// ── Ambient inputs (surface-parity) ────────────────────────────────────────────
//
// Peer-dispatch and correlation-id are *ambient* tool inputs: they ride
// alongside a tool's own args on every surface, delivered by whatever channel
// that transport offers — CLI global `--peer` flag, REST `X-Orca-Peer` /
// `x-correlation-id` headers, and (on JSON-RPC, which has no header/flag
// channel for a tool call) reserved keys inside the MCP `arguments` object.
// They are stripped before the tool's typed `Args` deserialization and folded
// onto `ToolCtx`, so the universal macro-emitted peer-dispatch stanza fires for
// EVERY `remote_ok` tool with zero per-tool code. This keeps MCP at full parity
// with CLI/REST automatically — a new `#[orca_tool]` is peer-dispatchable on all
// three surfaces the moment it exists.

/// Reserved MCP `arguments` key carrying a remote peer hostname. Mirrors the
/// REST `X-Orca-Peer` header and the CLI `--peer` flag.
pub const AMBIENT_PEER_KEY: &str = "peer";
/// Reserved MCP `arguments` key carrying a correlation id. Mirrors the REST
/// `x-correlation-id` header.
pub const AMBIENT_CORRELATION_KEY: &str = "correlation_id";

/// Split ambient inputs out of a JSON args object for a JSON-RPC (MCP) tool
/// call, returning `(cleaned_args, peer, correlation_id)`. The reserved keys
/// are removed so they never reach the tool's typed `Args`. Non-object args
/// (rare) pass through untouched with no ambient values.
pub fn take_ambient(mut args: Value) -> (Value, Option<String>, Option<String>) {
    let mut peer = None;
    let mut correlation_id = None;
    if let Some(obj) = args.as_object_mut() {
        peer = obj
            .remove(AMBIENT_PEER_KEY)
            .and_then(|v| v.as_str().map(str::to_string))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        correlation_id = obj
            .remove(AMBIENT_CORRELATION_KEY)
            .and_then(|v| v.as_str().map(str::to_string))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    (args, peer, correlation_id)
}

/// Advertise the ambient `peer` property on an MCP `inputSchema` for a
/// peer-dispatchable (`remote_ok`) tool, so clients discover that any tool can
/// be aimed at a mesh peer. Local-only tools are left untouched — they reject a
/// peer target by design. Idempotent: never overrides a tool's own field.
fn advertise_ambient(mut schema: Value, remote_ok: bool) -> Value {
    if !remote_ok {
        return schema;
    }
    let Some(obj) = schema.as_object_mut() else {
        return schema;
    };
    let props = obj.entry("properties").or_insert_with(|| json!({}));
    if let Some(props) = props.as_object_mut() {
        props.entry(AMBIENT_PEER_KEY).or_insert_with(|| {
            json!({
                "type": "string",
                "description": "Optional. Run this tool on a remote pod peer (by hostname) over the mesh — universal peer-dispatch. Omit to run on the local host."
            })
        });
    }
    schema
}

// ── MCP ──────────────────────────────────────────────────────────────────────

/// Build the JSON array for `tools/list`.
pub fn mcp_definitions() -> Vec<Value> {
    let mut defs: Vec<Value> = cache()
        .ordered
        .iter()
        .map(|t| {
            json!({
                "name": t.name(),
                "description": t.description(),
                "inputSchema": advertise_ambient(t.input_schema(), t.remote_ok()),
            })
        })
        .collect();
    // Merge dynamically-loaded cdylib plugin tools so `tools/list` surfaces
    // them alongside the static registry. `dynamic_tool_defs` carries the
    // plugin manifest shape (`input_schema`); remap to MCP's `inputSchema`.
    for d in dynamic_tool_defs() {
        defs.push(json!({
            "name": d.get("name").cloned().unwrap_or(Value::Null),
            "description": d.get("description").cloned().unwrap_or(Value::Null),
            "inputSchema": d.get("input_schema").cloned().unwrap_or(json!({ "type": "object" })),
        }));
    }
    // Merge the live, plugin-driven unit surface (unit.<kind>.<verb|action>),
    // built from the current provider catalog — already in MCP `inputSchema` shape.
    defs.extend(crate::unit_surface::unit_mcp_defs());
    // The two fixed diagnostics ops (diagnostics.diagnose / .repair).
    defs.extend(crate::diagnostics_surface::diagnostics_mcp_defs());
    defs
}

/// Build the cdylib-plugin manifest as a JSON string: an array of objects
/// `{ name, description, input_schema, output_schema }`. This is the exact
/// shape `plugin_toolkit::abi::ToolDef` deserializes, so a cdylib plugin's
/// ABI `manifest()` entrypoint can return `tool_manifest_json()` directly —
/// reusing its own internally-linked inventory registry rather than
/// reimplementing schema emission. Returned as a `String` (not a typed Vec)
/// so dispatch carries no dependency on the toolkit's abi types.
pub fn tool_manifest_json() -> String {
    let defs: Vec<Value> = cache()
        .ordered
        .iter()
        .map(|t| {
            json!({
                "name": t.name(),
                "description": t.description(),
                "input_schema": t.input_schema(),
                "output_schema": t.output_schema(),
            })
        })
        .collect();
    // The registry is always serializable JSON; never fails in practice.
    serde_json::to_string(&defs).unwrap_or_else(|_| "[]".to_string())
}

/// Dispatch a `tools/call` by name, returning a structured JSON value.
/// Returns `Err` for unknown tool names.
pub async fn dispatch(name: &str, args: Value, ctx: &ToolCtx) -> Result<Value> {
    match find(name) {
        Some(tool) => tool.run_json(args, ctx).await,
        // On a static miss, try the dynamic cdylib-plugin fallback, then the
        // live unit surface, before giving up — so loaded plugin tools AND the
        // universal `unit.<kind>.<verb>` surface share this one entrypoint.
        None => match dynamic_dispatch(name, &args) {
            Some(result) => result,
            None => match crate::unit_surface::unit_dispatch(name, &args).await {
                Some(result) => result,
                None => match crate::diagnostics_surface::diagnostics_dispatch(name, &args).await {
                    Some(result) => result,
                    None => anyhow::bail!("unknown tool: {name}"),
                },
            },
        },
    }
}

/// Dispatch and render the result as plain text. MCP + CLI use this; REST
/// uses `dispatch` directly so it gets the structured JSON.
pub async fn dispatch_text(name: &str, args: Value, ctx: &ToolCtx) -> Result<String> {
    let value = dispatch(name, args, ctx).await?;
    Ok(value_to_text(&value))
}

// ── HTTP / REST ──────────────────────────────────────────────────────────────

/// Build an axum router that exposes every registered tool as
/// `POST /<name>` with a JSON body matching `input_schema()` and a JSON
/// response matching `output_schema()`. The caller decides where to mount
/// it (typically `.nest("/api/v1", axum_router(ctx))`).
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
    caller: Option<Extension<contract::CallerIdentity>>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> std::result::Result<Json<Value>, (StatusCode, Json<Value>)> {
    if find(&name).is_none()
        && !dynamic_owns(&name)
        && !crate::unit_surface::unit_owns(&name)
        && !crate::diagnostics_surface::diagnostics_owns(&name)
    {
        let oe = contract::OrcaError::not_found(format!("unknown tool: {name}"))
            .with_code("tool.unknown");
        return Err(orca_error_response(oe));
    }
    // Per-request identity + peer-routing overlay. Both come off the shared
    // ctx via clone-and-mutate so the base ctx stays immutable across
    // concurrent requests. Caller swap: auth middleware → real user identity
    // for caller-token minting on any pod/exec the tool fires. Peer swap:
    // `X-Orca-Peer: <hostname>` header → universal peer-dispatch trigger,
    // same pathway as the CLI `--peer` flag.
    let peer = headers
        .get(PEER_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let correlation_id = headers
        .get("x-correlation-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let ctx_owned = if caller.is_some() || peer.is_some() || correlation_id.is_some() {
        let mut ctx = (*state.ctx).clone();
        if let Some(Extension(c)) = caller {
            ctx.set_caller(Some(c));
        }
        if let Some(p) = peer {
            ctx.set_peer(Some(p));
        }
        if let Some(cid) = correlation_id {
            ctx.set_correlation_id(Some(cid));
        }
        Some(ctx)
    } else {
        None
    };
    let ctx_ref: &ToolCtx = ctx_owned.as_ref().unwrap_or(&state.ctx);
    dispatch(&name, args, ctx_ref).await.map(Json).map_err(|e| {
        if let Some(oe) = e.downcast_ref::<contract::OrcaError>() {
            let kind = oe.kind;
            let body = serde_json::to_value(oe)
                .unwrap_or_else(|_| json!({ "kind": "internal", "message": "serialize failure" }));
            let status = StatusCode::from_u16(kind.http_status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            return (status, Json(body));
        }
        let oe = contract::OrcaError::internal(e.to_string());
        orca_error_response(oe)
    })
}

fn orca_error_response(oe: contract::OrcaError) -> (StatusCode, Json<Value>) {
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
/// `/api/v1/*` invocations.
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

/// Whether a statically-linked (inventory) tool with this name exists. Used by
/// the runtime cdylib plugin loader to reject a plugin tool that would shadow a
/// built-in one.
pub fn tool_exists(name: &str) -> bool {
    find(name).is_some()
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
    use contract::{OrcaTool, OrcaToolDef, ToolCtx};
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
        use contract::config::{Config, Model};
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

    #[derive(Deserialize, Serialize, JsonSchema)]
    struct AddArgs {
        a: i64,
        b: i64,
    }

    struct AddTool;

    impl OrcaToolDef for AddTool {
        const NAME: &'static str = "add.local";
        const DESCRIPTION: &'static str = "Adds two numbers.";
        const REMOTE_OK: bool = true;
        type Args = AddArgs;
        type Output = String;
    }

    #[async_trait]
    impl OrcaTool for AddTool {
        async fn run(args: AddArgs, _ctx: &ToolCtx) -> Result<String> {
            Ok((args.a + args.b).to_string())
        }
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
    async fn erased_wrapper_add_serializes_typed_output() {
        let w = ToolWrapper::<AddTool>(PhantomData);
        let e: &dyn ErasedTool = &w;
        let v = e
            .run_json(serde_json::json!({"a": 7, "b": 3}), &make_ctx())
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!("10"));
    }

    #[tokio::test]
    async fn erased_wrapper_invalid_args_returns_named_error() {
        let w = ToolWrapper::<EchoTool>(PhantomData);
        let e: &dyn ErasedTool = &w;
        let err = e
            .run_json(serde_json::json!({}), &make_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid args for echo.local"));
    }

    #[test]
    fn erased_wrapper_forwards_remote_ok_and_required_role() {
        let w = ToolWrapper::<EchoTool>(PhantomData);
        let e: &dyn ErasedTool = &w;
        assert!(!e.remote_ok());
        assert_eq!(e.required_role(), "admin");
        let w2 = ToolWrapper::<AddTool>(PhantomData);
        let e2: &dyn ErasedTool = &w2;
        assert!(e2.remote_ok());
        assert_eq!(e2.required_role(), "any");
    }

    #[test]
    fn erased_wrapper_exposes_input_and_output_schemas() {
        let w = ToolWrapper::<AddTool>(PhantomData);
        let e: &dyn ErasedTool = &w;
        let inp = e.input_schema();
        let out = e.output_schema();
        for v in [&inp, &out] {
            let obj = v.as_object().expect("schema is an object");
            assert!(!obj.contains_key("$schema"));
            assert!(!obj.contains_key("title"));
        }
        assert!(inp.to_string().contains('a') && inp.to_string().contains('b'));

        // Exercise the schema methods on EchoTool too, so the
        // `ToolWrapper<EchoTool>` / `schema_for::<EchoArgs>` monomorphizations
        // are executed, not just compiled.
        let echo = ToolWrapper::<EchoTool>(PhantomData);
        let echo_erased: &dyn ErasedTool = &echo;
        assert!(echo_erased.input_schema().is_object());
        assert!(echo_erased.output_schema().is_object());
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
    async fn cli_dispatch_pair_args_parse_into_typed_struct() {
        // Build an args map exactly the way CliArgs::Pairs would and prove
        // numeric coercion picks i64 over string.
        let pairs = CliArgs::Pairs(vec!["a=5".into(), "b=3".into()]);
        let CliArgs::Pairs(p) = &pairs else {
            unreachable!()
        };
        let mut map = serde_json::Map::new();
        for pair in p {
            let (k, v) = pair.split_once('=').unwrap();
            let val: Value = serde_json::from_str(v).unwrap_or(Value::String(v.to_string()));
            map.insert(k.to_string(), val);
        }
        assert_eq!(map["a"], serde_json::json!(5));
        assert_eq!(map["b"], serde_json::json!(3));
    }

    #[test]
    fn cli_args_pairs_constructor_round_trips() {
        // CliArgs is opaque from outside the crate; this proves both
        // variants enumerate cleanly inside.
        match CliArgs::Json("{}".into()) {
            CliArgs::Json(s) => assert_eq!(s, "{}"),
            _ => panic!(),
        }
        match CliArgs::Pairs(vec!["a=1".into()]) {
            CliArgs::Pairs(v) => assert_eq!(v[0], "a=1"),
            _ => panic!(),
        }
    }

    #[test]
    fn clap_command_builds_top_level_orca_root() {
        // Empty inventory → no subcommands, but the root command still has
        // the right name + about line.
        let cmd = clap_command();
        assert_eq!(cmd.get_name(), "orca");
    }

    #[test]
    fn value_to_text_passes_strings_through() {
        assert_eq!(value_to_text(&Value::String("hi".into())), "hi");
    }

    #[test]
    fn take_ambient_strips_peer_and_correlation_keys() {
        let (clean, peer, cid) = take_ambient(json!({
            "peer": "host-a",
            "correlation_id": "abc-123",
            "self_secure": true
        }));
        assert_eq!(peer.as_deref(), Some("host-a"));
        assert_eq!(cid.as_deref(), Some("abc-123"));
        // Reserved keys removed so they never reach the tool's typed Args.
        assert_eq!(clean, json!({ "self_secure": true }));
    }

    #[test]
    fn take_ambient_blank_and_missing_yield_none() {
        let (clean, peer, cid) = take_ambient(json!({ "peer": "  ", "x": 1 }));
        assert!(peer.is_none(), "blank peer must be None");
        assert!(cid.is_none());
        assert_eq!(clean, json!({ "x": 1 }));
        // Non-object args pass through untouched.
        let (clean2, peer2, cid2) = take_ambient(json!("scalar"));
        assert_eq!(clean2, json!("scalar"));
        assert!(peer2.is_none() && cid2.is_none());
    }

    #[test]
    fn advertise_ambient_injects_peer_only_for_remote_ok() {
        let base =
            json!({ "type": "object", "properties": { "self_secure": { "type": "boolean" } } });
        let with = advertise_ambient(base.clone(), true);
        assert!(
            with["properties"]["peer"].is_object(),
            "remote_ok tool must advertise `peer`"
        );
        // local_only: schema untouched.
        let without = advertise_ambient(base.clone(), false);
        assert!(without["properties"].get("peer").is_none());
    }

    #[test]
    fn advertise_ambient_never_overrides_a_tools_own_field() {
        let base = json!({ "type": "object", "properties": { "peer": { "type": "integer" } } });
        let out = advertise_ambient(base, true);
        // A tool that already declares `peer` keeps its own shape.
        assert_eq!(out["properties"]["peer"]["type"], "integer");
    }

    #[test]
    fn advertise_ambient_creates_properties_when_absent() {
        let out = advertise_ambient(json!({ "type": "object" }), true);
        assert!(out["properties"]["peer"].is_object());
    }

    #[test]
    fn mcp_definitions_returns_a_vec_even_when_empty() {
        // Empty inventory in this test binary: should still be a Vec, not
        // panic.
        let defs = mcp_definitions();
        assert!(defs.is_empty() || defs.iter().all(|d| d.is_object()));
    }

    #[test]
    fn role_table_is_consistent_with_required_role_lookup() {
        for (name, role) in role_table() {
            assert_eq!(required_role(name), Some(role));
        }
    }

    #[test]
    fn names_and_role_table_have_the_same_cardinality() {
        let names_len = names().len();
        let roles_len = role_table().len();
        assert_eq!(names_len, roles_len);
    }

    #[test]
    fn remote_ok_names_are_a_subset_of_names() {
        let all: std::collections::HashSet<&'static str> = names().into_iter().collect();
        for n in remote_ok_names() {
            assert!(all.contains(n), "remote_ok name {n} not in names()");
        }
    }

    #[test]
    fn required_role_returns_none_for_unknown_tool() {
        assert!(required_role("does.not.exist").is_none());
    }

    #[tokio::test]
    async fn dispatch_text_unknown_tool_propagates_error() {
        let err = dispatch_text("ghost.tool", serde_json::json!({}), &make_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown tool"));
    }

    #[test]
    fn value_to_text_pretty_prints_objects() {
        let pretty = value_to_text(&serde_json::json!({"a": 1}));
        assert!(pretty.contains('\n'));
        assert!(pretty.contains("\"a\""));
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
