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
#[cfg(feature = "server")]
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    routing::post,
};

/// Per-request header that routes a `/api/v1/<name>` call to a remote
/// peer over the mesh. Mirrors the CLI `--peer <h>` flag — same
/// universal opt-out (local_only tools reject), same `ToolCtx::peer_target`
/// pathway. The web UI sets this header on per-peer actions like
/// "update this system" so the same REST surface that does local work also
/// drives the fleet.
#[cfg(feature = "server")]
pub const PEER_HEADER: &str = "x-orca-peer";

/// Request extension marking an authenticated request with no resolvable
/// user (a legacy or orphaned token). Such a request must never borrow the
/// ctx's ambient host-operator identity, which is an admin.
#[cfg(feature = "server")]
#[derive(Clone, Debug)]
pub struct UnboundCaller {
    pub role: String,
}
use serde_json::{Value, json};
use std::collections::HashMap;
#[cfg(any(feature = "server", feature = "in-process"))]
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

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
            // Dedup by name rather than panicking: an artifact built with more
            // than one crate-type (e.g. `["cdylib", "rlib"]`) emits each
            // `inventory` entry once per crate-type, and a duplicate must not
            // abort `tool_manifest_json()` while a plugin builds its handshake
            // manifest. First registration wins.
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

pub(crate) fn find(name: &str) -> Option<&'static dyn ErasedTool> {
    let c = cache();
    c.by_name.get(name).map(|i| c.ordered[*i].as_ref())
}

// ── Dynamic (loaded-plugin) fallback hook ──────────────────────────────────────
//
// `dispatch` knows only the statically-linked `inventory` registry. Runtime
// subprocess plugins live in `plugin-loader`'s registry, which `dispatch` cannot
// depend on (plugin-loader → dispatch). To keep one tool namespace across REST
// / MCP / CLI without a dependency cycle, the host installs a *fallback* here
// at startup: a synchronous `(name, args) -> Option<Result<Value>>` that returns
// `Some` iff a loaded plugin owns the name. `dispatch` consults it on a miss.
// This inverts the dependency — `dispatch` holds a fn pointer the server wires —
// rather than re-exporting plugin-loader.

/// Returns `Some(result)` iff a dynamically-loaded plugin owns `name`. The
/// identity a plugin may act for is the ctx's verified caller
/// ([`ToolCtx::verified_caller`]), never the host operator.
type DynamicInvoker = dyn Fn(&str, &Value, &ToolCtx) -> Option<Result<Value>> + Send + Sync;

/// Returns the JSON tool defs (`{name, description, input_schema, output_schema}`)
/// of every dynamically-loaded plugin, for merging into list surfaces.
type DynamicDefs = dyn Fn() -> Vec<Value> + Send + Sync;

static DYNAMIC_INVOKER: OnceLock<Box<DynamicInvoker>> = OnceLock::new();
static DYNAMIC_DEFS: OnceLock<Box<DynamicDefs>> = OnceLock::new();

/// Install the loaded-plugin fallback. Called once by the host at startup after
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
///
/// This layer owns keeping plugin invokes off async workers: the invoker blocks
/// on plugin socket I/O, so it runs on a dedicated thread (a nested `block_on`
/// there would panic, #798). Threads are capped by [`invoke_permits`] and each
/// call waits at most [`plugin_permit_wait`] for a thread. The plugin loader
/// bounds the call itself: socket silence by [`plugin_idle_timeout`], total
/// time by [`plugin_max_invoke`].
#[cfg(feature = "in-process")]
async fn dynamic_dispatch(name: &str, args: &Value, ctx: &ToolCtx) -> Option<Result<Value>> {
    let invoker = DYNAMIC_INVOKER.get()?;
    let (owned, args, ctx) = (name.to_string(), args.clone(), ctx.clone());
    let result = run_off_worker(invoke_permits(), plugin_permit_wait(), move || {
        invoker(&owned, &args, &ctx)
    })
    .await;
    match result {
        Ok(found) => found,
        Err(e) => Some(Err(e.context(format!("plugin tool '{name}'")))),
    }
}

/// Without `in-process` no host installs an invoker, so nothing to bound.
#[cfg(not(feature = "in-process"))]
async fn dynamic_dispatch(name: &str, args: &Value, ctx: &ToolCtx) -> Option<Result<Value>> {
    DYNAMIC_INVOKER.get().and_then(|f| f(name, args, ctx))
}

pub(crate) fn secs_from_env(var: &str, default: u64) -> Duration {
    let secs = std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(default);
    Duration::from_secs(secs)
}

/// How long a plugin invoke waits for a free invoke thread, from
/// `ORCA_PLUGIN_PERMIT_WAIT_SECS` (default 60s).
pub fn plugin_permit_wait() -> Duration {
    static WAIT: OnceLock<Duration> = OnceLock::new();
    *WAIT.get_or_init(|| secs_from_env("ORCA_PLUGIN_PERMIT_WAIT_SECS", 60))
}

/// How long a plugin session may go without a frame mid-invoke, from
/// `ORCA_PLUGIN_IDLE_TIMEOUT_SECS` (default 300s). Plugins heartbeat while a
/// tool runs, so only a silent plugin hits it.
pub fn plugin_idle_timeout() -> Duration {
    static IDLE: OnceLock<Duration> = OnceLock::new();
    *IDLE.get_or_init(|| secs_from_env("ORCA_PLUGIN_IDLE_TIMEOUT_SECS", 300))
}

/// Wall-clock ceiling on one plugin invoke, from `ORCA_PLUGIN_MAX_INVOKE_SECS`
/// (default 3600s). Heartbeats don't extend it, so a plugin that heartbeats
/// while deadlocked is still cut off.
pub fn plugin_max_invoke() -> Duration {
    static MAX: OnceLock<Duration> = OnceLock::new();
    *MAX.get_or_init(|| secs_from_env("ORCA_PLUGIN_MAX_INVOKE_SECS", 3600))
}

/// Concurrent plugin-invoke threads, from `ORCA_PLUGIN_INVOKE_THREADS`
/// (default 32).
#[cfg(feature = "in-process")]
fn invoke_permits() -> Arc<tokio::sync::Semaphore> {
    static PERMITS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    PERMITS
        .get_or_init(|| {
            let n = std::env::var("ORCA_PLUGIN_INVOKE_THREADS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(32);
            Arc::new(tokio::sync::Semaphore::new(n))
        })
        .clone()
}

/// Run blocking `f` on its own thread, holding a permit from `permits` for the
/// thread's whole life so a hung call keeps counting against the cap. Only the
/// wait for a permit is bounded, by `permit_wait`.
#[cfg(feature = "in-process")]
async fn run_off_worker<T: Send + 'static>(
    permits: Arc<tokio::sync::Semaphore>,
    permit_wait: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    let span = tracing::Span::current();
    let permit = tokio::time::timeout(permit_wait, permits.acquire_owned())
        .await
        .map_err(|_| {
            anyhow::anyhow!("timed out after {permit_wait:?} waiting for a plugin invoke thread")
        })?
        .map_err(|e| anyhow::anyhow!("plugin invoke pool closed: {e}"))?;
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name("plugin-invoke".into())
        .spawn(move || {
            let _permit = permit;
            // A dropped receiver means the caller went away; nothing to report.
            tx.send(span.in_scope(f)).ok();
        })
        .map_err(|e| anyhow::anyhow!("spawn plugin invoke thread: {e}"))?;
    rx.await
        .map_err(|_| anyhow::anyhow!("plugin invoke thread panicked"))
}

/// JSON tool defs contributed by loaded plugins. Empty when no fallback
/// is installed.
pub fn dynamic_tool_defs() -> Vec<Value> {
    DYNAMIC_DEFS.get().map(|f| f()).unwrap_or_default()
}

/// True iff a loaded plugin owns `name` (via the installed fallback).
pub fn dynamic_owns(name: &str) -> bool {
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
/// Optional target-system argument on plugin and unit ops: the system's UUID.
/// Resolved and routed by [`dispatch`]; stripped before the op runs unless the
/// op's own schema declares it.
pub const SYSTEM_ID_KEY: &str = "systemId";
/// Reserved MCP `arguments` key carrying a correlation id. Mirrors the REST
/// `x-correlation-id` header.
pub const AMBIENT_CORRELATION_KEY: &str = "correlation_id";

/// A caller-supplied peer selector. Absent is `None`; a blank or non-string
/// value is refused, since reading it as "no peer" would run the call locally
/// when the caller asked for somewhere else.
pub fn peer_selector(
    raw: Option<&Value>,
) -> std::result::Result<Option<String>, contract::OrcaError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let Some(s) = raw.as_str() else {
        return Err(invalid_peer("a peer selector must be a string"));
    };
    match s.trim() {
        "" => Err(invalid_peer(
            "a blank peer selector does not mean this system; omit it to run here",
        )),
        s => Ok(Some(s.to_string())),
    }
}

fn invalid_peer(why: &str) -> contract::OrcaError {
    contract::OrcaError::invalid(format!("{AMBIENT_PEER_KEY}: {why}")).with_code("args.invalid")
}

/// Split ambient inputs out of a JSON args object for a JSON-RPC (MCP) tool
/// call, returning `(cleaned_args, peer, correlation_id)`. The reserved keys
/// are removed so they never reach the tool's typed `Args`. Non-object args
/// (rare) pass through untouched with no ambient values.
pub fn take_ambient(
    mut args: Value,
) -> std::result::Result<(Value, Option<String>, Option<String>), contract::OrcaError> {
    let peer = peer_selector(
        args.as_object_mut()
            .and_then(|obj| obj.remove(AMBIENT_PEER_KEY))
            .as_ref(),
    )?;
    let correlation_id = take_correlation_id(&mut args);
    Ok((args, peer, correlation_id))
}

/// Removes the ambient `correlation_id` key from an args object, returning its
/// trimmed value when it is a non-blank string.
fn take_correlation_id(args: &mut Value) -> Option<String> {
    args.as_object_mut()?
        .remove(AMBIENT_CORRELATION_KEY)
        .and_then(|v| v.as_str().map(str::trim).map(str::to_string))
        .filter(|s| !s.is_empty())
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
                "description": "Optional. Run this tool on a remote system over the mesh — universal peer-dispatch. Accepts the peer's machine_id (peer_id), hostname/display name, or LAN address (any of the three). Prefer the stable machine_id — the display name is mutable and can duplicate, in which case a name/addr selector is rejected as ambiguous and you must use the machine_id. Omit to run on the local host."
            })
        });
    }
    schema
}

/// Advertise the optional [`SYSTEM_ID_KEY`] target on a plugin/unit op schema.
/// Never overrides an op's own field of that name.
pub fn advertise_system(mut schema: Value) -> Value {
    let Some(obj) = schema.as_object_mut() else {
        return schema;
    };
    let props = obj.entry("properties").or_insert_with(|| json!({}));
    if let Some(props) = props.as_object_mut() {
        props.entry(SYSTEM_ID_KEY).or_insert_with(|| {
            json!({
                "type": "string",
                "description": "Optional. UUID of the system to run this on. Omit to run on this system."
            })
        });
    }
    schema
}

/// True when an op's own input schema has a `systemId` field, so the value is
/// the op's argument rather than a routing target.
pub(crate) fn declares_system(name: &str) -> bool {
    let has = |schema: Option<&Value>| {
        schema
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.get(SYSTEM_ID_KEY))
            .is_some()
    };
    if let Some(tool) = find(name) {
        return has(Some(&tool.input_schema()));
    }
    dynamic_tool_defs()
        .iter()
        .find(|d| d.get("name").and_then(|n| n.as_str()) == Some(name))
        .is_some_and(|d| has(d.get("input_schema")))
        || crate::unit_surface::unit_ops()
            .iter()
            .find(|op| op.name == name)
            .is_some_and(|op| has(Some(&op.input_schema)))
}

/// Pull a `systemId` target out of `args` and resolve it against the mesh
/// roster. `Ok(None)` means run here (absent, or names this system);
/// `Ok(Some(id))` is the remote system's machine id to forward to.
async fn take_system_target(name: &str, args: &mut Value, ctx: &ToolCtx) -> Result<Option<String>> {
    if args.get(SYSTEM_ID_KEY).is_none() || declares_system(name) {
        return Ok(None);
    }
    if find(name).is_some() {
        anyhow::bail!("{name} does not take {SYSTEM_ID_KEY}; it runs only on this system");
    }
    if ctx.peer().is_some() {
        anyhow::bail!("conflicting routing target: use {SYSTEM_ID_KEY} alone");
    }
    let raw = args
        .as_object_mut()
        .and_then(|o| o.remove(SYSTEM_ID_KEY))
        .unwrap_or(Value::Null);
    let Some(sel) = raw
        .as_str()
        .map(str::trim)
        .filter(|s| uuid::Uuid::parse_str(s).is_ok())
    else {
        anyhow::bail!("{SYSTEM_ID_KEY}: must be a system UUID; omit it to run here, got {raw}");
    };
    let svc = ctx
        .service::<std::sync::Arc<dyn contract::RemoteExec>>()
        .map_err(|_| anyhow::anyhow!("unknown system `{sel}`: no mesh transport to resolve it"))?;
    let peers = svc.peers().await?;
    match peers.iter().find(|p| p.id.eq_ignore_ascii_case(sel)) {
        None => anyhow::bail!("unknown system `{sel}`"),
        Some(p) if p.is_local => Ok(None),
        Some(p) => Ok(Some(p.id.clone())),
    }
}

/// True when [`dispatch`] can run or route `name` on this host: a static tool,
/// a loaded plugin, unit/diagnostics/UPS op, or any call carrying a
/// [`SYSTEM_ID_KEY`] target (the op may be loaded only on that system).
pub fn dispatchable(name: &str, args: &Value) -> bool {
    find(name).is_some()
        || dynamic_owns(name)
        || crate::unit_surface::unit_owns(name)
        || crate::diagnostics_surface::diagnostics_owns(name)
        || crate::ups_surface::ups_owns(name)
        || args.get(SYSTEM_ID_KEY).is_some()
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
    // Merge dynamically-loaded plugin tools so `tools/list` surfaces
    // them alongside the static registry. `dynamic_tool_defs` carries the
    // plugin manifest shape (`input_schema`); remap to MCP's `inputSchema`.
    for d in dynamic_tool_defs() {
        defs.push(json!({
            "name": d.get("name").cloned().unwrap_or(Value::Null),
            "description": d.get("description").cloned().unwrap_or(Value::Null),
            "inputSchema": advertise_system(d.get("input_schema").cloned().unwrap_or(json!({ "type": "object" }))),
        }));
    }
    // Merge the live, plugin-driven unit surface (unit.<kind>.<verb|action>),
    // built from the current provider catalog — already in MCP `inputSchema` shape.
    defs.extend(crate::unit_surface::unit_mcp_defs());
    // The two fixed diagnostics ops (diagnostics.diagnose / .repair).
    defs.extend(crate::diagnostics_surface::diagnostics_mcp_defs());
    // The three fixed UPS ops (ups.state / .config / .configure).
    defs.extend(crate::ups_surface::ups_mcp_defs());
    defs
}

/// Build a plugin's tool manifest as a JSON string: an array of objects
/// `{ name, description, input_schema, output_schema, role, execute_gated,
/// data_mutation }`. The daemon installs the last three into [`crate::tool_roles`]
/// at plugin load so plugin tools are gated like core ones. This is the exact
/// shape `plugin_toolkit::abi::ToolDef` deserializes, so a plugin builds its
/// handshake manifest from `tool_manifest_json()` directly — reusing its own
/// internally-linked inventory registry rather than reimplementing schema
/// emission. Returned as a `String` (not a typed Vec)
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
                "role": t.required_role(),
                "execute_gated": t.execute_gated(),
                "data_mutation": t.data_mutation(),
                "local_only": !t.remote_ok(),
                "reaches": t.reaches(),
            })
        })
        .collect();
    // The registry is always serializable JSON; never fails in practice.
    serde_json::to_string(&defs).unwrap_or_else(|_| "[]".to_string())
}

/// Dispatch a `tools/call` by name, returning a structured JSON value.
/// Returns `Err` for unknown tool names.
pub async fn dispatch(name: &str, args: Value, ctx: &ToolCtx) -> Result<Value> {
    #[cfg(feature = "in-process")]
    if let Some(verb) = core_verb(name, &args, ctx) {
        return contract::invocation_scope::core_verb(verb, dispatch_verb(name, args, ctx)).await;
    }
    dispatch_verb(name, args, ctx).await
}

#[cfg(feature = "in-process")]
fn core_verb(
    name: &str,
    args: &Value,
    ctx: &ToolCtx,
) -> Option<contract::invocation_scope::CoreVerb> {
    let caller = ctx.verified_caller()?.clone();
    let execute_gated = find(name).is_some_and(|t| t.execute_gated())
        || crate::tool_roles::plugin_execute_gated(name);
    let dry_run = ctx.dry_run() || crate::erased::dry_run_requested(args).unwrap_or(true);
    Some(contract::invocation_scope::CoreVerb::new(
        caller,
        crate::tool_roles::required_role(name),
        execute_gated,
        dry_run,
        ctx.local_transport(),
        ctx.second_factor(),
    ))
}

async fn dispatch_verb(name: &str, mut args: Value, ctx: &ToolCtx) -> Result<Value> {
    if let Some(system) = take_system_target(name, &mut args, ctx).await? {
        // Same rule as an ambient-peer forward: nothing proves the target
        // honours `dryRun`, so only an execute-gated tool may dry-run remotely.
        if crate::erased::dry_run_requested(&args)?
            && !crate::tool_roles::plugin_execute_gated(name)
        {
            return Err(contract::OrcaError::invalid(format!(
                "{name}: a dry run can't be verified on system {system}; run it without dryRun"
            ))
            .with_code("args.invalid")
            .into());
        }
        return forward_to_peer(&system, name, args, ctx).await;
    }
    if let Some(tool) = find(name) {
        return tool.run_json(args, ctx).await;
    }
    authorize_fallback(name, ctx)?;
    let dry_run = crate::erased::dry_run_requested(&args)?;
    if crate::tool_roles::plugin_execute_gated(name)
        && let Some(obj) = args.as_object_mut()
    {
        // An older plugin's gate plans unless told to execute, so a normal
        // call says so explicitly; a dry run carries only `dryRun`.
        obj.remove(contract::plan::EXECUTE_FIELD);
        if !dry_run {
            obj.remove(contract::plan::DRY_RUN_FIELD);
            obj.insert(contract::plan::EXECUTE_FIELD.into(), Value::Bool(true));
        }
    }
    // The surfaces below bypass the `run_json` gate, so a `dryRun` they
    // cannot honour would apply. A plugin tool declared execute-gated plans in
    // its own gate. A tool this host does not have is refused even with a
    // peer named: no peer capability signal proves the peer honours `dryRun`,
    // and one that predates it would drop the flag and apply.
    if dry_run
        && !crate::tool_roles::plugin_execute_gated(name)
        && !crate::unit_surface::unit_dry_run_ok(name)
    {
        let known_here = dynamic_owns(name)
            || crate::unit_surface::unit_owns(name)
            || crate::diagnostics_surface::diagnostics_owns(name)
            || crate::ups_surface::ups_owns(name);
        if known_here {
            return Err(
                contract::OrcaError::invalid(format!("{name} does not support dryRun"))
                    .with_code("args.invalid")
                    .into(),
            );
        }
        let msg = match ctx.peer() {
            Some(peer) => format!(
                "{name} is not on this system, so a dry run can't be verified on {peer}; \
                 run it on that system directly"
            ),
            None => {
                format!("{name} is not available on this system; cannot verify dryRun support")
            }
        };
        return Err(contract::OrcaError::invalid(msg)
            .with_code("args.invalid")
            .into());
    }
    // On a static miss, try the dynamic loaded-plugin fallback, then the live
    // unit surface, before giving up — so loaded plugin tools AND the
    // universal `unit.<kind>.<verb>` surface share this one entrypoint.
    match dynamic_dispatch(name, &args, ctx).await {
        Some(result) => result,
        None => {
            match crate::unit_surface::unit_dispatch(name, &args, ctx.verified_caller()).await {
                Some(result) => result,
                None => match crate::diagnostics_surface::diagnostics_dispatch(name, &args).await {
                    Some(result) => result,
                    None => match crate::ups_surface::ups_dispatch(name, &args).await {
                        Some(result) => result,
                        // Unknown on THIS host — but if the caller named an
                        // explicit peer, the tool almost certainly lives there
                        // (a plugin loaded only on that peer, e.g. proxmox on a
                        // PVE host). Forward the raw call over the mesh instead
                        // of 404ing, so a host's surface can reach any tool
                        // anywhere in the mesh, not just its locally-loaded set.
                        // (mesh-tool-surface-must-expose-peer-tools)
                        None => forward_unknown_to_peer(name, args, ctx).await,
                    },
                },
            }
        }
    }
}

/// Role check for the non-static surfaces, mirroring the one `run_json`
/// applies to static tools, so a surface that skips its own gate still cannot
/// reach a plugin/unit/diagnostics/UPS body. A name with no local role is
/// left to the peer forward, whose recipient authorizes it.
fn authorize_fallback(name: &str, ctx: &ToolCtx) -> Result<()> {
    if let Some(role) = ctx.unbound_role()
        && let Some(required) = crate::tool_roles::known_role(name)
        && !crate::tool_roles::satisfies(role, required)
    {
        return Err(contract::OrcaError::forbidden(format!(
            "{name} requires role '{required}'; a token with no resolvable user has '{role}'"
        ))
        .into());
    }
    let caller = ctx.verified_caller().cloned().or_else(|| ctx.caller());
    let (Some(caller), Some(required)) = (caller, crate::tool_roles::known_role(name)) else {
        return Ok(());
    };
    if crate::tool_roles::authorize(
        &caller.role,
        caller.can_mutate,
        required,
        crate::tool_roles::is_data_mutation(name),
    ) {
        return Ok(());
    }
    Err(contract::OrcaError::forbidden(format!(
        "{name} requires role '{required}'; caller '{}' has '{}'",
        caller.username, caller.role
    ))
    .into())
}

/// Router-level peer pass-through for a tool this host does not have. Only
/// fires when `ctx.peer()` names an explicit peer; otherwise the call is a
/// genuine unknown-tool error. Strips the ambient `peer` key from the forwarded
/// args so the recipient runs the tool LOCALLY instead of bouncing it onward.
async fn forward_unknown_to_peer(name: &str, args: Value, ctx: &ToolCtx) -> Result<Value> {
    let Some(peer) = ctx.peer().map(str::to_string) else {
        anyhow::bail!("unknown tool: {name}");
    };
    forward_to_peer(&peer, name, args, ctx).await
}

async fn forward_to_peer(peer: &str, name: &str, mut args: Value, ctx: &ToolCtx) -> Result<Value> {
    let svc = ctx
        .service::<std::sync::Arc<dyn contract::RemoteExec>>()
        .map_err(|_| {
            anyhow::anyhow!("unknown tool: {name} (no mesh transport to reach peer {peer})")
        })?;
    let dry_run = crate::erased::dry_run_requested(&args)?;
    if let Some(obj) = args.as_object_mut() {
        obj.remove(AMBIENT_PEER_KEY);
        // Only a tool this host knows as execute-gated gets the explicit
        // opt-in; a gated plugin loaded solely on the peer plans unless told.
        // A dry run must never carry `execute`, or an older gate applies it.
        if crate::tool_roles::plugin_execute_gated(name) {
            obj.remove(contract::plan::EXECUTE_FIELD);
            if !dry_run {
                obj.remove(contract::plan::DRY_RUN_FIELD);
                obj.insert(contract::plan::EXECUTE_FIELD.into(), Value::Bool(true));
            }
        }
    }
    svc.exec(
        peer,
        name,
        args,
        ctx.caller(),
        ctx.correlation_id().map(str::to_string),
    )
    .await
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
///
/// Gated behind the `server` feature: only the orca daemon binary mounts the
/// REST surface, so a plugin (which links `dispatch` for `register_op!` /
/// `OrcaTool` only) never pulls axum/tower. Plugin tools still reach REST — via
/// the daemon, which loads them over the UDS capability channel.
#[cfg(feature = "server")]
pub fn axum_router(ctx: Arc<ToolCtx>) -> Router {
    // Single wildcard route — the path segment is the tool name.
    Router::new()
        .route("/{name}", post(http_dispatch))
        .with_state(ToolHttpState { ctx })
}

#[cfg(feature = "server")]
#[derive(Clone)]
struct ToolHttpState {
    ctx: Arc<ToolCtx>,
}

#[cfg(feature = "server")]
async fn http_dispatch(
    State(state): State<ToolHttpState>,
    Path(name): Path<String>,
    caller: Option<Extension<contract::CallerIdentity>>,
    unbound: Option<Extension<UnboundCaller>>,
    headers: HeaderMap,
    query: std::result::Result<Query<Vec<(String, String)>>, QueryRejection>,
    Json(mut args): Json<Value>,
) -> std::result::Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Query(query) = query.map_err(|e| {
        orca_error_response(
            contract::OrcaError::invalid(format!("invalid query string: {e}"))
                .with_code("args.invalid"),
        )
    })?;
    apply_dry_run_query(&query, &mut args).map_err(orca_error_response)?;
    // Per-request identity + peer-routing overlay. Both come off the shared
    // ctx via clone-and-mutate so the base ctx stays immutable across
    // concurrent requests. Caller swap: auth middleware → real user identity
    // for caller-token minting on any mesh/exec the tool fires. Peer swap:
    // `X-Orca-Peer: <hostname>` header → universal peer-dispatch trigger,
    // same pathway as the CLI `--peer` flag.
    //
    // Peer is resolved BEFORE the unknown-tool guard: a tool this host never
    // loaded (a peer-only plugin) is not a 404 when an explicit peer is named
    // — `dispatch` forwards it over the mesh. Only 404 when it is unknown AND
    // no peer was given.
    // REST parity with MCP's ambient `peer` arg: a top-level `peer` field in
    // the JSON body (AMBIENT_PEER_KEY) is always stripped so it is never
    // forwarded as a tool argument. Both selectors are validated, and two that
    // name different systems are refused rather than one silently winning.
    let body_peer = args
        .as_object_mut()
        .and_then(|obj| obj.remove(AMBIENT_PEER_KEY));
    let body_peer = peer_selector(body_peer.as_ref()).map_err(orca_error_response)?;
    let header_peer = headers
        .get(PEER_HEADER)
        .map(|v| {
            v.to_str()
                .map(|s| Value::String(s.to_string()))
                .map_err(|_| {
                    orca_error_response(invalid_peer("the X-Orca-Peer header is not valid UTF-8"))
                })
        })
        .transpose()?;
    let header_peer = peer_selector(header_peer.as_ref()).map_err(orca_error_response)?;
    let peer = match (header_peer, body_peer) {
        (Some(h), Some(b)) if !h.eq_ignore_ascii_case(&b) => {
            return Err(orca_error_response(invalid_peer(&format!(
                "the X-Orca-Peer header '{h}' and the body peer '{b}' name different systems"
            ))));
        }
        (h, b) => h.or(b),
    };
    // Always stripped from the body, as on MCP; the header wins when both are set.
    let body_correlation_id = take_correlation_id(&mut args);
    let correlation_id = headers
        .get("x-correlation-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or(body_correlation_id);
    // Unknown-tool guard, deferred until after peer resolution: a peer-only
    // tool forwards over the mesh; a genuinely unknown local call 404s.
    if peer.is_none() && !dispatchable(&name, &args) {
        let oe = contract::OrcaError::not_found(format!("unknown tool: {name}"))
            .with_code("tool.unknown");
        return Err(orca_error_response(oe));
    }
    let mut ctx = (*state.ctx).clone();
    ctx.set_local_transport(false);
    ctx.set_second_factor(false);
    if let Some(Extension(u)) = unbound {
        ctx.set_caller(None);
        ctx.set_unbound_role(Some(u.role));
    }
    if let Some(Extension(c)) = caller {
        ctx.set_verified_caller(Some(c.clone()));
        ctx.set_caller(Some(c));
    }
    if let Some(p) = peer {
        ctx.set_peer(Some(p));
    }
    if let Some(cid) = correlation_id {
        ctx.set_correlation_id(Some(cid));
    }
    let ctx_ref: &ToolCtx = &ctx;
    dispatch(&name, args, ctx_ref).await.map(Json).map_err(|e| {
        if let Some(oe) = find_orca_error(&e) {
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

/// Fold the `?dryRun=` query opt-in into the args so the gate, the refusal
/// and peer forwarding all see one value. Only `true`/`false` parse, and a
/// body `dryRun` that disagrees is refused rather than one silently winning.
#[cfg(feature = "server")]
fn apply_dry_run_query(
    query: &[(String, String)],
    args: &mut Value,
) -> std::result::Result<(), contract::OrcaError> {
    let field = contract::plan::DRY_RUN_FIELD;
    let invalid = |msg: String| contract::OrcaError::invalid(msg).with_code("args.invalid");
    let mut values = query.iter().filter(|(k, _)| k == field).map(|(_, v)| v);
    let Some(raw) = values.next() else {
        return Ok(());
    };
    if values.next().is_some() {
        return Err(invalid(format!("?{field} given more than once")));
    }
    let dry_run = match raw.as_str() {
        "true" => true,
        "false" => false,
        _ => {
            return Err(invalid(format!(
                "?{field} must be true or false, got '{raw}'"
            )));
        }
    };
    let Some(obj) = args.as_object_mut() else {
        return Err(invalid(format!(
            "?{field} needs a JSON object request body"
        )));
    };
    match obj.get(field) {
        Some(body) if *body != Value::Bool(dry_run) => Err(invalid(format!(
            "?{field}={dry_run} and the body {field} {body} disagree"
        ))),
        _ => {
            obj.insert(field.to_string(), Value::Bool(dry_run));
            Ok(())
        }
    }
}

/// The classified error behind `e`, if any. `downcast_ref` sees through anyhow
/// context layers, including one that is itself the `OrcaError`; the chain walk
/// finds one held as a std `source()`.
#[cfg(any(feature = "server", test))]
fn find_orca_error(e: &anyhow::Error) -> Option<&contract::OrcaError> {
    e.downcast_ref::<contract::OrcaError>().or_else(|| {
        e.chain()
            .find_map(|c| c.downcast_ref::<contract::OrcaError>())
    })
}

#[cfg(feature = "server")]
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
/// Used to populate the static allowlist for `mesh/exec` dispatch.
pub fn remote_ok_names() -> Vec<&'static str> {
    cache()
        .ordered
        .iter()
        .filter(|t| t.remote_ok())
        .map(|t| t.name())
        .collect()
}

/// The `local_only` tools — those that opted OUT of cross-host dispatch
/// (`#[orca_tool(local_only = true)]` / `remote_ok = false`). This is the
/// DENYLIST: everything is REMOTE_OK by default, and only these names are
/// refused at the mesh-exec gate. Mirror of [`remote_ok_names`], inverted.
pub fn local_only_names() -> Vec<&'static str> {
    cache()
        .ordered
        .iter()
        .filter(|t| !t.remote_ok())
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

/// Names of every registered tool that is a data mutation (write against an
/// external managed system). Used to install the process-global mutation
/// lookup the REST middleware consults so a `can_mutate` opt-in identity may
/// invoke these despite their `admin` required-role.
pub fn data_mutation_names() -> Vec<&'static str> {
    cache()
        .ordered
        .iter()
        .filter(|t| t.data_mutation())
        .map(|t| t.name())
        .collect()
}

/// Names of every registered tool that is execute-gated — i.e. executes by
/// default and plans under `dryRun`. Lets a surface (and the conformance test)
/// enumerate them without walking the inventory.
pub fn execute_gated_names() -> Vec<&'static str> {
    cache()
        .ordered
        .iter()
        .filter(|t| t.execute_gated())
        .map(|t| t.name())
        .collect()
}

/// Names of every registered ungated tool that honours `dryRun` in its own
/// body.
pub fn dry_run_aware_names() -> Vec<&'static str> {
    cache()
        .ordered
        .iter()
        .filter(|t| t.dry_run_aware())
        .map(|t| t.name())
        .collect()
}

/// Names of every registered tool that clears its apply opt-ins before a dry
/// run is forwarded to a peer.
pub fn dry_run_neutralized_names() -> Vec<&'static str> {
    cache()
        .ordered
        .iter()
        .filter(|t| t.dry_run_neutralizes())
        .map(|t| t.name())
        .collect()
}

/// Whether a statically-linked (inventory) tool with this name exists. Used by
/// the runtime plugin loader to reject a plugin tool that would shadow a
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
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let err = dispatch("ghost.tool", serde_json::json!({}), &make_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown tool"));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_with_peer_forwards_over_mesh() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        // A tool this host never loaded (a peer-only plugin) must forward to
        // the named peer instead of 404ing — router-level peer pass-through.
        use contract::{CallerIdentity, RemoteExec};
        struct SpyExec;
        #[async_trait]
        impl RemoteExec for SpyExec {
            async fn exec(
                &self,
                peer: &str,
                tool: &str,
                args: Value,
                _caller: Option<CallerIdentity>,
                _cid: Option<String>,
            ) -> Result<Value> {
                // Echo back what would go on the wire so the test can assert
                // the peer key was stripped before forwarding.
                Ok(serde_json::json!({ "peer": peer, "tool": tool, "args": args }))
            }
        }
        let mut ctx = make_ctx();
        let spy: Arc<dyn RemoteExec> = Arc::new(SpyExec);
        ctx.register_service(spy);
        ctx.set_peer(Some("thor".into()));

        let out = dispatch(
            "proxmox.cluster_list",
            serde_json::json!({"peer": "thor"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(out["peer"], "thor");
        assert_eq!(out["tool"], "proxmox.cluster_list");
        // Ambient peer key stripped so the recipient runs it locally (no bounce).
        assert!(
            out["args"].get("peer").is_none(),
            "peer key must be stripped"
        );
    }

    const LOCAL_ID: &str = "0190a1b2-0000-7000-8000-000000000001";
    const REMOTE_ID: &str = "0190a1b2-0000-7000-8000-000000000002";

    struct RosterExec;
    #[async_trait]
    impl contract::RemoteExec for RosterExec {
        async fn peers(&self) -> Result<Vec<contract::PeerRef>> {
            Ok(vec![
                contract::PeerRef {
                    id: LOCAL_ID.into(),
                    name: "here".into(),
                    is_local: true,
                },
                contract::PeerRef {
                    id: REMOTE_ID.into(),
                    name: "there".into(),
                    is_local: false,
                },
            ])
        }
        async fn exec(
            &self,
            peer: &str,
            tool: &str,
            args: Value,
            _caller: Option<contract::CallerIdentity>,
            _cid: Option<String>,
        ) -> Result<Value> {
            Ok(serde_json::json!({ "peer": peer, "tool": tool, "args": args }))
        }
    }

    fn roster_ctx() -> ToolCtx {
        let mut ctx = make_ctx();
        let exec: Arc<dyn contract::RemoteExec> = Arc::new(RosterExec);
        ctx.register_service(exec);
        ctx
    }

    #[tokio::test]
    async fn dispatch_system_id_remote_forwards_and_strips_it() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let out = dispatch(
            "docker.ps",
            serde_json::json!({ "systemId": REMOTE_ID, "all": true }),
            &roster_ctx(),
        )
        .await
        .unwrap();
        assert_eq!(out["peer"], REMOTE_ID);
        assert_eq!(out["tool"], "docker.ps");
        assert_eq!(out["args"], serde_json::json!({ "all": true }));
    }

    #[tokio::test]
    async fn dispatch_system_id_local_runs_here() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        // Local target → not forwarded; no local owner, so it is unknown here.
        let err = dispatch(
            "docker.ps",
            serde_json::json!({ "systemId": LOCAL_ID }),
            &roster_ctx(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown tool"), "got: {err}");
    }

    #[tokio::test]
    async fn dispatch_system_id_unknown_errors() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let id = "0190a1b2-0000-7000-8000-0000000000ff";
        let err = dispatch(
            "docker.ps",
            serde_json::json!({ "systemId": id }),
            &roster_ctx(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), format!("unknown system `{id}`"));
    }

    #[tokio::test]
    async fn dispatch_system_id_rejects_non_uuid() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        for bad in [
            serde_json::json!("there"),
            serde_json::json!(""),
            serde_json::json!(7),
        ] {
            let err = dispatch(
                "docker.ps",
                serde_json::json!({ "systemId": bad }),
                &roster_ctx(),
            )
            .await
            .unwrap_err();
            assert!(
                err.to_string().contains("must be a system UUID"),
                "got: {err}"
            );
        }
    }

    #[test]
    fn advertise_system_adds_system_id_without_overriding() {
        let out = advertise_system(serde_json::json!({ "type": "object" }));
        assert_eq!(out["properties"]["systemId"]["type"], "string");
        let own = serde_json::json!({ "properties": { "systemId": { "type": "integer" } } });
        assert_eq!(
            advertise_system(own)["properties"]["systemId"]["type"],
            "integer"
        );
    }

    #[tokio::test]
    async fn dry_run_is_refused_on_paths_that_bypass_the_gate() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let _serial = SEEN_TESTS.lock().await;
        install_fake_plugin();
        use contract::{CallerIdentity, RemoteExec};
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingExec(AtomicUsize);
        #[async_trait]
        impl RemoteExec for CountingExec {
            async fn exec(
                &self,
                _peer: &str,
                _tool: &str,
                _args: Value,
                _caller: Option<CallerIdentity>,
                _cid: Option<String>,
            ) -> Result<Value> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Null)
            }
        }
        let mut ctx = make_ctx();
        let spy = Arc::new(CountingExec(AtomicUsize::new(0)));
        let svc: Arc<dyn RemoteExec> = spy.clone();
        ctx.register_service(svc);
        ctx.set_peer(Some("thor".into()));

        // A plugin tool loaded here is refused even with a peer named: this
        // host knows it does not honour `dryRun`.
        let name = FAKE_PLUGIN_TOOL;
        let err = dispatch(name, serde_json::json!({"id": "u-1", "dryRun": true}), &ctx)
            .await
            .unwrap_err();
        let oe = err
            .downcast_ref::<contract::OrcaError>()
            .expect("OrcaError");
        assert_eq!(oe.code.as_deref(), Some("args.invalid"), "{name}: {err}");
        assert_eq!(oe.kind, contract::ErrorKind::Invalid, "{name}: {err}");
        assert!(
            err.to_string()
                .contains(&format!("{name} does not support dryRun")),
            "{name}: {err}"
        );
        assert_eq!(
            spy.0.load(Ordering::SeqCst),
            0,
            "a refused dry run reached a peer"
        );
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_without_peer_still_errors() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        // No peer + no transport → genuine unknown-tool error, not a forward.
        let err = dispatch("proxmox.cluster_list", serde_json::json!({}), &make_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown tool"));
    }

    #[tokio::test]
    async fn cli_dispatch_unknown_tool_errors() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
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
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
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
        let _registry = crate::UNIT_REGISTRY_LOCK.blocking_lock();
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
        }))
        .unwrap();
        assert_eq!(peer.as_deref(), Some("host-a"));
        assert_eq!(cid.as_deref(), Some("abc-123"));
        // Reserved keys removed so they never reach the tool's typed Args.
        assert_eq!(clean, json!({ "self_secure": true }));
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn rest_body_correlation_id_never_reaches_the_tool() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        use axum::body::Body;
        use axum::http::Request as AxumReq;
        use tower::ServiceExt;

        let _serial = SEEN_TESTS.lock().await;
        install_fake_plugin();
        SEEN_ARGS.lock().unwrap().clear();
        let req = AxumReq::builder()
            .method("POST")
            .uri(format!("/{FAKE_PLUGIN_TOOL}"))
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "correlation_id": "abc", "x": 1 }).to_string(),
            ))
            .unwrap();
        let resp = axum_router(Arc::new(make_ctx()))
            .oneshot(req)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*SEEN_ARGS.lock().unwrap(), vec![json!({ "x": 1 })]);
    }

    #[cfg(feature = "server")]
    #[test]
    fn rest_error_body_carries_remediation_intact() {
        let oe = contract::OrcaError::blocked(
            "x.off",
            "service off",
            contract::ManualSteps::new("no API")
                .step("Enable", "in the UI")
                .verify("x status"),
        );
        let (status, Json(body)) = orca_error_response(oe);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "x.off");
        assert_eq!(body["remediation"]["type"], "manual");
        assert_eq!(body["remediation"]["steps"][0]["verify"], "x status");
    }

    #[test]
    fn find_orca_error_sees_wrapped_and_context_held_errors() {
        let wrapped = anyhow::Error::from(contract::OrcaError::invalid("a")).context("ctx");
        assert_eq!(find_orca_error(&wrapped).unwrap().message, "a");

        #[derive(Debug)]
        struct Marker;
        impl std::fmt::Display for Marker {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("marker")
            }
        }
        impl std::error::Error for Marker {}
        let held = anyhow::Error::new(Marker).context(contract::OrcaError::invalid("b"));
        assert_eq!(find_orca_error(&held).unwrap().message, "b");

        // anyhow's downcast only sees context layers and the root error, so an
        // OrcaError reachable only as another error's source() needs the chain walk.
        #[derive(Debug)]
        struct Sourced(contract::OrcaError);
        impl std::fmt::Display for Sourced {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("sourced")
            }
        }
        impl std::error::Error for Sourced {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let sourced = anyhow::Error::new(Sourced(contract::OrcaError::invalid("c")));
        assert!(sourced.downcast_ref::<contract::OrcaError>().is_none());
        assert_eq!(find_orca_error(&sourced).unwrap().message, "c");

        assert!(find_orca_error(&anyhow::anyhow!("plain")).is_none());
    }

    #[test]
    fn take_ambient_missing_yields_none() {
        let (clean, peer, cid) = take_ambient(json!({ "x": 1 })).unwrap();
        assert!(peer.is_none());
        assert!(cid.is_none());
        assert_eq!(clean, json!({ "x": 1 }));
        // Non-object args pass through untouched.
        let (clean2, peer2, cid2) = take_ambient(json!("scalar")).unwrap();
        assert_eq!(clean2, json!("scalar"));
        assert!(peer2.is_none() && cid2.is_none());
    }

    #[test]
    fn a_blank_peer_selector_is_refused_not_run_locally() {
        for blank in [json!(""), json!("   ")] {
            let err = take_ambient(json!({ "peer": blank, "x": 1 })).unwrap_err();
            assert_eq!(err.kind, contract::ErrorKind::Invalid, "{blank}");
            assert!(err.message.starts_with("peer: a blank"), "{err}");
        }
    }

    #[test]
    fn a_non_string_peer_selector_is_refused_as_such() {
        let err = take_ambient(json!({ "peer": 7 })).unwrap_err();
        assert_eq!(err.kind, contract::ErrorKind::Invalid);
        assert!(err.message.contains("must be a string"), "{err}");
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
        let _registry = crate::UNIT_REGISTRY_LOCK.blocking_lock();
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
    fn execute_gated_names_are_a_subset_of_names() {
        let all: std::collections::HashSet<&'static str> = names().into_iter().collect();
        for n in execute_gated_names() {
            assert!(all.contains(n), "execute_gated name {n} not in names()");
        }
    }

    /// An execute-gated verb applies changes, so it must also be gated on WHO
    /// may call it — for the default execute and for its dry run alike.
    #[test]
    fn every_execute_gated_tool_is_admin_only() {
        for name in execute_gated_names() {
            assert_eq!(
                required_role(name),
                Some("admin"),
                "{name} is execute_gated but not role=admin — mutation without authorization"
            );
        }
    }

    /// The gate injects `dryRun` and the deprecated `execute` into a gated
    /// tool's advertised schema and strips both before typed deserialization.
    /// A verb that ALSO declares either field would have it silently
    /// swallowed, so catch that here rather than at runtime. The ad-hoc gates
    /// (`system.update`, `plugin.update` with `execute`;
    /// `storage.share.repair-permissions` with `apply`; `storage.mount.create`
    /// with `force`) are deliberately NOT execute_gated.
    #[test]
    fn no_execute_gated_tool_declares_its_own_gate_field() {
        for name in execute_gated_names() {
            let Some(tool) = find(name) else { continue };
            let schema = tool.input_schema();
            // Both properties are present because the gate injected them; a
            // verb-owned one would carry its own description instead.
            for (field, marker) in [("dryRun", "ExecutionPlan"), ("execute", "Deprecated no-op")] {
                let desc = schema["properties"][field]["description"]
                    .as_str()
                    .unwrap_or_default();
                assert!(
                    desc.contains(marker),
                    "{name} appears to declare its own `{field}` field; it would be \
                     swallowed by the gate. Migrate it or drop execute_gated."
                );
            }
        }
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
    async fn fallback_dispatch_refuses_caller_below_plugin_tool_role() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        use crate::tool_roles::{PluginToolPolicy, install_plugin_tools, remove_plugin_tools};
        let name = "registry_test_plugin.no_role";
        install_plugin_tools([(
            name.to_string(),
            PluginToolPolicy::from_manifest(name, None, None, None),
        )]);
        let caller = |role: &str| contract::CallerIdentity {
            user_id: "u".into(),
            username: "u".into(),
            role: role.into(),
            can_mutate: false,
        };
        let member = make_ctx().with_auth(caller("member"));
        let err = dispatch(name, serde_json::json!({}), &member)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("requires role 'admin'"), "{err}");
        let admin = make_ctx().with_auth(caller("admin"));
        let err = dispatch(name, serde_json::json!({}), &admin)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown tool"), "{err}");
        remove_plugin_tools([name]);
    }

    #[tokio::test]
    async fn dispatch_text_unknown_tool_propagates_error() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
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

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn http_dispatch_returns_404_for_unknown_tool() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
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

    const FAKE_PLUGIN_TOOL: &str = "rest_fallback_test_plugin.adopt";

    /// Callers the fake plugin saw, by username (`None` = no caller).
    static SEEN: std::sync::Mutex<Vec<Option<String>>> = std::sync::Mutex::new(Vec::new());
    static SEEN_LOCAL: std::sync::Mutex<Vec<bool>> = std::sync::Mutex::new(Vec::new());

    /// Args the fake plugin saw.
    static SEEN_ARGS: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());

    /// Serializes the tests that clear and read `SEEN` / `SEEN_ARGS`.
    static SEEN_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn install_fake_plugin() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            set_dynamic_dispatch(
                Box::new(|name, args: &Value, ctx: &ToolCtx| {
                    let caller = ctx.verified_caller();
                    if name == NESTED_RUNTIME_TOOL {
                        // What a plugin's `http.request` capability does.
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .build()
                            .unwrap();
                        return Some(Ok(rt.block_on(async { json!({ "nested": true }) })));
                    }
                    if name == GATED_PLUGIN_TOOL {
                        GATED_SEEN.lock().unwrap().push(args.clone());
                        return Some(Ok(json!({})));
                    }
                    if name == OWN_SYSTEM_TOOL {
                        return Some(Ok(args.clone()));
                    }
                    (name == FAKE_PLUGIN_TOOL).then(|| {
                        SEEN_LOCAL.lock().unwrap().push(ctx.local_transport());
                        SEEN.lock()
                            .unwrap()
                            .push(caller.map(|c| c.username.clone()));
                        SEEN_ARGS.lock().unwrap().push(args.clone());
                        Ok(json!({}))
                    })
                }),
                Box::new(|| {
                    vec![
                        json!({ "name": FAKE_PLUGIN_TOOL }),
                        json!({ "name": GATED_PLUGIN_TOOL }),
                        json!({
                            "name": OWN_SYSTEM_TOOL,
                            "input_schema": { "properties": { "systemId": { "type": "string" } } }
                        }),
                    ]
                }),
            );
            crate::tool_roles::install_plugin_tools([(
                GATED_PLUGIN_TOOL.to_string(),
                crate::tool_roles::PluginToolPolicy::from_manifest(
                    GATED_PLUGIN_TOOL,
                    None,
                    Some(true),
                    None,
                ),
            )]);
        });
    }

    const GATED_PLUGIN_TOOL: &str = "rest_fallback_test_plugin.gated";

    /// Args the execute-gated fake plugin tool received.
    static GATED_SEEN: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());

    #[cfg(feature = "server")]
    async fn post_json(uri: &str, body: Value) -> (StatusCode, Value) {
        use axum::body::{Body, to_bytes};
        use axum::http::Request as AxumReq;
        use tower::ServiceExt;

        let router = axum_router(Arc::new(make_ctx()));
        let req = AxumReq::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn http_requests_never_carry_local_transport() {
        use axum::body::Body;
        use axum::http::Request as AxumReq;
        use tower::ServiceExt;

        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let _serial = SEEN_TESTS.lock().await;
        install_fake_plugin();
        SEEN_LOCAL.lock().unwrap().clear();
        let mut local = make_ctx();
        local.set_local_transport(true);
        local.set_second_factor(true);
        for forwarded in [None, Some("127.0.0.1"), Some("10.0.0.5")] {
            let mut req = AxumReq::builder()
                .method("POST")
                .uri(format!("/{FAKE_PLUGIN_TOOL}"))
                .header("content-type", "application/json");
            if let Some(ip) = forwarded {
                req = req.header("x-forwarded-for", ip).header("x-real-ip", ip);
            }
            let resp = axum_router(Arc::new(local.clone()))
                .oneshot(req.body(Body::from("{}")).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        assert_eq!(*SEEN_LOCAL.lock().unwrap(), vec![false, false, false]);
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn dry_run_query_on_a_plugin_tool_is_refused_unless_it_is_gated() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let _serial = SEEN_TESTS.lock().await;
        install_fake_plugin();
        let (status, body) =
            post_json(&format!("/{FAKE_PLUGIN_TOOL}?dryRun=true"), json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], json!("args.invalid"), "{body}");

        GATED_SEEN.lock().unwrap().clear();
        let (status, body) =
            post_json(&format!("/{GATED_PLUGIN_TOOL}?dryRun=true"), json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let seen = GATED_SEEN.lock().unwrap().clone();
        assert_eq!(seen, vec![json!({ "dryRun": true })]);
    }

    #[tokio::test]
    async fn a_gated_plugin_is_told_to_execute_unless_it_is_a_dry_run() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let _serial = SEEN_TESTS.lock().await;
        install_fake_plugin();
        GATED_SEEN.lock().unwrap().clear();
        let ctx = make_ctx();
        for args in [
            json!({ "id": "a" }),
            json!({ "id": "a", "dryRun": false }),
            json!({ "id": "a", "dryRun": true, "execute": true }),
        ] {
            dispatch(GATED_PLUGIN_TOOL, args, &ctx).await.unwrap();
        }
        let seen = GATED_SEEN.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                json!({ "id": "a", "execute": true }),
                json!({ "id": "a", "execute": true }),
                json!({ "id": "a", "dryRun": true }),
            ]
        );
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn dry_run_query_must_be_a_boolean() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        for q in ["1", "yes", "True", ""] {
            let (status, body) = post_json(&format!("/ghost.tool?dryRun={q}"), json!({})).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{q}: {body}");
            assert_eq!(body["code"], json!("args.invalid"), "{q}: {body}");
        }
    }

    #[cfg(feature = "server")]
    #[test]
    fn dry_run_query_and_body_must_agree() {
        let q = |v: &str| vec![("dryRun".to_string(), v.to_string())];
        let mut args = json!({ "dryRun": true });
        let err = apply_dry_run_query(&q("false"), &mut args).unwrap_err();
        assert_eq!(err.code.as_deref(), Some("args.invalid"));
        let mut args = json!({ "dryRun": "true" });
        assert!(apply_dry_run_query(&q("true"), &mut args).is_err());
        let mut args = json!({ "dryRun": true });
        apply_dry_run_query(&q("true"), &mut args).unwrap();
        let mut args = json!({});
        apply_dry_run_query(&q("false"), &mut args).unwrap();
        assert_eq!(args, json!({ "dryRun": false }));
        let mut args = json!({});
        apply_dry_run_query(&[], &mut args).unwrap();
        assert_eq!(args, json!({}));
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn a_repeated_dry_run_query_is_refused() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        for q in ["dryRun=true&dryRun=false", "dryRun=true&dryRun=true"] {
            let (status, body) = post_json(&format!("/ghost.tool?{q}"), json!({})).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{q}: {body}");
            assert_eq!(body["code"], json!("args.invalid"), "{q}: {body}");
        }
    }

    #[tokio::test]
    async fn a_dry_run_of_a_tool_absent_here_names_why_it_cannot_be_checked() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let err = dispatch("ghost.tool", json!({ "dryRun": true }), &make_ctx())
            .await
            .unwrap_err();
        let oe = find_orca_error(&err).unwrap_or_else(|| panic!("{err}"));
        assert_eq!(oe.code.as_deref(), Some("args.invalid"), "{oe}");
        assert_eq!(
            oe.message,
            "ghost.tool is not available on this system; cannot verify dryRun support"
        );
    }

    type RemoteCall = (String, String, Value);

    struct RecordingRemote(std::sync::Mutex<Vec<RemoteCall>>);

    #[async_trait]
    impl contract::RemoteExec for RecordingRemote {
        async fn exec(
            &self,
            peer: &str,
            tool: &str,
            args: Value,
            _caller: Option<contract::CallerIdentity>,
            _correlation_id: Option<String>,
        ) -> Result<Value> {
            self.0
                .lock()
                .unwrap()
                .push((peer.into(), tool.into(), args));
            Ok(json!({ "forwarded": true }))
        }
    }

    #[tokio::test]
    async fn a_dry_run_of_a_tool_absent_here_is_refused_not_sent_to_the_peer() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let remote = Arc::new(RecordingRemote(std::sync::Mutex::new(Vec::new())));
        let mut ctx = make_ctx();
        ctx.register_service::<Arc<dyn contract::RemoteExec>>(remote.clone());
        ctx.set_peer(Some("peer-a".into()));
        let err = dispatch("ghost.tool", json!({ "dryRun": true }), &ctx)
            .await
            .unwrap_err();
        let oe = find_orca_error(&err).unwrap_or_else(|| panic!("{err}"));
        assert_eq!(oe.code.as_deref(), Some("args.invalid"), "{oe}");
        assert_eq!(
            oe.message,
            "ghost.tool is not on this system, so a dry run can't be verified on peer-a; \
             run it on that system directly"
        );
        assert!(remote.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_normal_call_forwarded_to_a_peer_carries_execute_only_when_gated() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let _serial = SEEN_TESTS.lock().await;
        install_fake_plugin();
        let remote = Arc::new(RecordingRemote(std::sync::Mutex::new(Vec::new())));
        let mut ctx = make_ctx();
        ctx.register_service::<Arc<dyn contract::RemoteExec>>(remote.clone());
        ctx.set_peer(Some("peer-a".into()));
        dispatch("ghost.tool", json!({ "id": "a" }), &ctx)
            .await
            .unwrap();
        forward_unknown_to_peer(GATED_PLUGIN_TOOL, json!({ "id": "a" }), &ctx)
            .await
            .unwrap();
        let calls = remote.0.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                ("peer-a".into(), "ghost.tool".into(), json!({ "id": "a" })),
                (
                    "peer-a".into(),
                    GATED_PLUGIN_TOOL.into(),
                    json!({ "id": "a", "execute": true })
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_non_boolean_dry_run_is_refused_off_the_static_registry() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        for v in [json!("true"), json!(1), Value::Null] {
            let err = dispatch("ghost.tool", json!({ "dryRun": v }), &make_ctx())
                .await
                .unwrap_err();
            let oe = find_orca_error(&err).unwrap_or_else(|| panic!("{err}"));
            assert_eq!(oe.code.as_deref(), Some("args.invalid"), "{oe}");
        }
    }

    const OWN_SYSTEM_TOOL: &str = "rest_fallback_test_plugin.own_system";

    #[tokio::test]
    async fn dispatch_system_id_declared_by_op_passes_through() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        install_fake_plugin();
        let args = json!({ "systemId": "not-a-uuid" });
        let out = dispatch(OWN_SYSTEM_TOOL, args.clone(), &roster_ctx())
            .await
            .unwrap();
        assert_eq!(out, args);
    }

    #[tokio::test]
    async fn dispatch_system_id_dry_run_of_gated_tool_forwards_without_execute() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let _serial = SEEN_TESTS.lock().await;
        install_fake_plugin();
        let out = dispatch(
            GATED_PLUGIN_TOOL,
            json!({ "systemId": REMOTE_ID, "id": "a", "dryRun": true, "execute": true }),
            &roster_ctx(),
        )
        .await
        .unwrap();
        assert_eq!(out["peer"], REMOTE_ID);
        assert_eq!(out["args"], json!({ "id": "a", "dryRun": true }));
        let out = dispatch(
            GATED_PLUGIN_TOOL,
            json!({ "systemId": REMOTE_ID, "id": "a", "dryRun": false }),
            &roster_ctx(),
        )
        .await
        .unwrap();
        assert_eq!(out["args"], json!({ "id": "a", "execute": true }));
    }

    #[tokio::test]
    async fn dispatch_system_id_dry_run_of_ungated_tool_is_refused() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let err = dispatch(
            "docker.ps",
            json!({ "systemId": REMOTE_ID, "dryRun": true }),
            &roster_ctx(),
        )
        .await
        .unwrap_err();
        let oe = find_orca_error(&err).unwrap_or_else(|| panic!("{err}"));
        assert_eq!(oe.code.as_deref(), Some("args.invalid"), "{oe}");
    }

    #[tokio::test]
    async fn dispatch_system_id_with_peer_errors() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let mut ctx = roster_ctx();
        ctx.set_peer(Some("there".into()));
        let err = dispatch("docker.ps", json!({ "systemId": REMOTE_ID }), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("conflicting routing target"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn dispatch_system_id_without_transport_errors() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let err = dispatch("docker.ps", json!({ "systemId": REMOTE_ID }), &make_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no mesh transport"), "got: {err}");
    }

    #[test]
    fn dispatchable_waives_unknown_name_when_system_id_present() {
        let _registry = crate::UNIT_REGISTRY_LOCK.blocking_lock();
        assert!(!dispatchable("ghost.tool", &json!({})));
        assert!(dispatchable(
            "ghost.tool",
            &json!({ "systemId": REMOTE_ID })
        ));
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn http_dispatch_forwards_unknown_tool_with_system_id() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        use axum::body::{Body, to_bytes};
        use axum::http::Request as AxumReq;
        use tower::ServiceExt;

        let router = axum_router(Arc::new(roster_ctx()));
        let req = AxumReq::builder()
            .method("POST")
            .uri("/ghost.tool")
            .header("content-type", "application/json")
            .body(Body::from(json!({ "systemId": REMOTE_ID }).to_string()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["peer"], REMOTE_ID);
        assert_eq!(out["tool"], "ghost.tool");
    }

    const NESTED_RUNTIME_TOOL: &str = "rest_fallback_test_plugin.nested_block_on";

    /// A plugin invoke that blocks on its own runtime must not run on an async
    /// worker, where a nested `block_on` panics.
    #[cfg(feature = "in-process")]
    #[tokio::test(flavor = "multi_thread")]
    async fn plugin_invoke_that_blocks_on_a_runtime_runs_off_the_async_worker() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        install_fake_plugin();
        let out = dispatch(NESTED_RUNTIME_TOOL, json!({}), &make_ctx())
            .await
            .unwrap();
        assert_eq!(out, json!({ "nested": true }));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test(flavor = "current_thread")]
    async fn a_long_plugin_invoke_outlives_the_permit_wait() {
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let out = run_off_worker(permits, Duration::from_millis(20), || {
            std::thread::sleep(Duration::from_millis(200));
            7
        })
        .await
        .unwrap();
        assert_eq!(out, 7);
    }

    #[cfg(feature = "in-process")]
    #[tokio::test(flavor = "current_thread")]
    async fn plugin_invoke_times_out_waiting_for_a_free_permit() {
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let _held = permits.clone().acquire_owned().await.unwrap();
        let err = run_off_worker(permits, Duration::from_millis(50), || ())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test(flavor = "current_thread")]
    async fn plugin_invoke_returns_the_thread_result_and_frees_the_permit() {
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let out = run_off_worker(permits.clone(), Duration::from_secs(5), || 7)
            .await
            .unwrap();
        assert_eq!(out, 7);
        // The permit drops with the thread, just after it sends.
        let _permit = tokio::time::timeout(Duration::from_secs(5), permits.acquire())
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn core_verb_scope_needs_a_verified_caller() {
        let mut ctx = make_ctx();
        assert!(core_verb("anything", &json!({}), &ctx).is_none());
        ctx.set_verified_caller(Some(identity("ann")));
        let verb = core_verb("anything", &json!({"dryRun": true}), &ctx).unwrap();
        assert_eq!(verb.caller.username, "ann");
        assert!(verb.dry_run && !verb.execute_gated);
        assert_eq!(verb.role, crate::tool_roles::required_role("anything"));
    }

    fn identity(name: &str) -> contract::CallerIdentity {
        contract::CallerIdentity {
            user_id: format!("u_{name}"),
            username: name.into(),
            role: "admin".into(),
            can_mutate: true,
        }
    }

    /// The daemon's shared ctx carries the host operator. A plugin must see
    /// only the identity a surface verified for the request, so the host
    /// operator never rides an `Invoke`.
    #[tokio::test]
    async fn plugin_invoke_carries_the_verified_caller_never_the_host_operator() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        let _serial = SEEN_TESTS.lock().await;
        install_fake_plugin();
        SEEN.lock().unwrap().clear();
        let host = make_ctx().with_auth(identity("host_admin"));
        dispatch(FAKE_PLUGIN_TOOL, json!({}), &host).await.unwrap();

        let mut per_req = host.clone();
        per_req.set_verified_caller(Some(identity("alice")));
        dispatch(FAKE_PLUGIN_TOOL, json!({}), &per_req)
            .await
            .unwrap();

        assert_eq!(*SEEN.lock().unwrap(), vec![None, Some("alice".into())]);
    }

    #[test]
    fn unbound_caller_is_refused_role_gated_fallback_names() {
        let mut ctx = make_ctx().with_auth(identity("host_admin"));
        assert!(authorize_fallback(crate::diagnostics_surface::REPAIR_TOOL, &ctx).is_ok());
        ctx.set_caller(None);
        ctx.set_unbound_role(Some("admin".into()));
        assert!(authorize_fallback(crate::diagnostics_surface::REPAIR_TOOL, &ctx).is_ok());
        ctx.set_unbound_role(Some("member".into()));
        assert!(authorize_fallback(crate::diagnostics_surface::REPAIR_TOOL, &ctx).is_err());
        assert!(authorize_fallback(crate::ups_surface::STATE_TOOL, &ctx).is_ok());
        assert!(authorize_fallback("__no_such_tool__", &ctx).is_ok());
        ctx.set_unbound_role(Some("super-admin".into()));
        assert!(authorize_fallback(crate::diagnostics_surface::REPAIR_TOOL, &ctx).is_ok());
        ctx.set_unbound_role(Some("bogus".into()));
        assert!(authorize_fallback(crate::diagnostics_surface::REPAIR_TOOL, &ctx).is_err());
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn rest_fallback_never_forwards_the_daemon_identity() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        use axum::body::Body;
        use axum::http::Request as AxumReq;
        use tower::ServiceExt;

        let _serial = SEEN_TESTS.lock().await;
        install_fake_plugin();
        let router = axum_router(Arc::new(make_ctx().with_auth(identity("host_admin"))));
        let post = || {
            AxumReq::builder()
                .method("POST")
                .uri(format!("/{FAKE_PLUGIN_TOOL}"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap()
        };

        SEEN.lock().unwrap().clear();
        // No per-request identity: the shared-ctx fallback branch.
        let resp = router.clone().oneshot(post()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mut with_user = post();
        with_user.extensions_mut().insert(identity("alice"));
        let resp = router.oneshot(with_user).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let seen = SEEN.lock().unwrap().clone();
        assert!(seen.contains(&None), "{seen:?}");
        assert!(seen.contains(&Some("alice".into())), "{seen:?}");
        assert!(!seen.contains(&Some("host_admin".into())), "{seen:?}");
    }

    /// A legacy or orphaned token resolves no user. Forwarding its call to a
    /// peer must carry no caller, so the peer refuses any role-gated tool,
    /// rather than borrowing the admin host operator from the shared ctx.
    #[cfg(feature = "server")]
    #[tokio::test]
    async fn rest_unbound_caller_forwards_to_a_peer_without_the_host_operator() {
        let _registry = crate::UNIT_REGISTRY_LOCK.lock().await;
        use axum::body::Body;
        use axum::http::Request as AxumReq;
        use contract::{CallerIdentity, RemoteExec};
        use tower::ServiceExt;

        struct SpyExec;
        #[async_trait]
        impl RemoteExec for SpyExec {
            async fn exec(
                &self,
                _peer: &str,
                _tool: &str,
                _args: Value,
                caller: Option<CallerIdentity>,
                _cid: Option<String>,
            ) -> Result<Value> {
                Ok(json!({ "caller": caller.map(|c| c.username) }))
            }
        }
        let mut ctx = make_ctx().with_auth(identity("host_admin"));
        let spy: Arc<dyn RemoteExec> = Arc::new(SpyExec);
        ctx.register_service(spy);
        let router = axum_router(Arc::new(ctx));
        let post = |unbound: bool| {
            let mut req = AxumReq::builder()
                .method("POST")
                .uri("/proxmox.cluster_list")
                .header("content-type", "application/json")
                .header(PEER_HEADER, "thor")
                .body(Body::from("{}"))
                .unwrap();
            if unbound {
                req.extensions_mut().insert(UnboundCaller {
                    role: "admin".into(),
                });
            }
            req
        };
        let caller_of = |resp: axum::response::Response| async move {
            let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
                .await
                .unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()["caller"].clone()
        };

        let resp = router.clone().oneshot(post(true)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(caller_of(resp).await, Value::Null);
        // Control: the loopback/in-process path keeps the host operator.
        let resp = router.oneshot(post(false)).await.unwrap();
        assert_eq!(caller_of(resp).await, json!("host_admin"));
    }
}
