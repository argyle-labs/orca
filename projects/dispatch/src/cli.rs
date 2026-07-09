//! Unified CLI surface for `OrcaOp`-flavoured tools.
//!
//! Each migrated op contributes one `CliOp` to a linker-time inventory via
//! the [`register_op!`] macro. The orca binary walks the inventory once at
//! startup to build a clap `Command` tree (`orca <domain> <verb> [args]`)
//! and dispatches matched args back through the tool's own `OrcaTool::run`.
//!
//! Why this exists: before this module, every tool needed a hand-written
//! `commands/<domain>_cmd.rs` shim duplicating arg parsing + dispatch that
//! already lives on the tool. With `register_op!`, that file goes away —
//! Args/Output flow end-to-end across MCP/REST/WASM/CLI from one source.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use clap::{ArgMatches, Command};

use contract::ToolCtx;

/// Transport for the CLI/embedder → local-daemon HTTP round-trip. The concrete
/// client (reqwest + session-cookie / loopback-token auth) lives in the `server`
/// crate and is installed once at startup via [`set_daemon_client`].
///
/// Keeping the HTTP stack OUT of `dispatch` is what lets a **plugin** stay thin:
/// a plugin links `dispatch` only for the `register_op!` / `OrcaTool` surface,
/// and the macro-emitted CLI `run` closure references only this trait — never a
/// linked reqwest/rustls. A plugin never installs a client (its tools are
/// invoked by the daemon over the UDS capability channel, not through this
/// CLI→daemon path), so the transport is dead code there and adds no deps.
pub trait DaemonClient: Send + Sync {
    /// POST typed args (already serialized to JSON) to the local daemon's
    /// `/api/v1/<name>` and return the JSON output — the same route + auth the
    /// web UI and MCP use, so CLI = REST = MCP.
    #[allow(clippy::disallowed_types)]
    fn post_tool<'a>(
        &'a self,
        name: &'a str,
        args: serde_json::Value,
        correlation_id: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send + 'a>>;

    /// GET a JSON document from the local daemon (e.g. `/api/catalog`).
    #[allow(clippy::disallowed_types)]
    fn get_json<'a>(
        &'a self,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send + 'a>>;
}

static DAEMON_CLIENT: OnceLock<Box<dyn DaemonClient>> = OnceLock::new();

/// Install the local-daemon HTTP client. Called once by the orca binary at
/// startup — the CLI and the daemon are the only builds that own an HTTP stack.
/// Idempotent: a second call is ignored (the `OnceLock` keeps the first).
pub fn set_daemon_client(client: Box<dyn DaemonClient>) {
    if DAEMON_CLIENT.set(client).is_err() {
        tracing::warn!("daemon client already installed; ignoring second install");
    }
}

/// The installed daemon client, or `None` in a build that never installs one
/// (e.g. a plugin subprocess, which reaches the daemon over the UDS instead).
fn daemon_client() -> Option<&'static dyn DaemonClient> {
    DAEMON_CLIENT.get().map(|b| b.as_ref())
}

/// Top-level `--peer <hostname>` flag exposed on every CLI invocation. When
/// present, the dispatcher populates `ToolCtx::peer_target` before invoking
/// the matched tool. Tools marked `local_only` will reject the peer routing
/// and surface a clear error.
const PEER_FLAG: &str = "__orca_peer";

/// Erased CLI dispatch closure: parses matches into the op's Args struct,
/// invokes `OrcaTool::run`, formats Output to stdout.
pub type CliRunFn =
    fn(&ArgMatches, Arc<ToolCtx>) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// Erased clap-subcommand builder for a single op.
pub type CliBuildFn = fn() -> Command;

/// One entry per `register_op!`. The orca binary collects these via the
/// `inventory` crate (linker-time) and groups by `domain` to assemble the
/// `orca <domain>` subcommand tree.
pub struct CliOp {
    pub domain: &'static str,
    pub verb: &'static str,
    pub summary: &'static str,
    pub build: CliBuildFn,
    pub run: CliRunFn,
}

inventory::collect!(CliOp);

/// Iterate over every registered CLI op. Stable order is **not** guaranteed
/// (`inventory` is linker-order); callers that need stable order should sort
/// by `(domain, verb)`.
pub fn ops() -> impl Iterator<Item = &'static CliOp> {
    inventory::iter::<CliOp>()
}

/// Build the top-level `orca` clap command from every registered op.
/// Domains become subcommands; verbs become sub-subcommands. A dotted domain
/// (`"pod.peer"`) nests further: `orca pod peer list` rather than the literal
/// `orca pod.peer list`. The dotted form remains the canonical tool NAME on
/// REST/MCP/WASM (`pod.peer.list`); only the CLI surface splits on the dots.
pub fn build_root(mut root: Command) -> Command {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Node {
        children: BTreeMap<&'static str, Node>,
        ops: Vec<&'static CliOp>,
    }

    let mut tree = Node::default();
    for op in ops() {
        let mut cur = &mut tree;
        for seg in op.domain.split('.') {
            cur = cur.children.entry(seg).or_default();
        }
        cur.ops.push(op);
    }

    fn materialize(name: &'static str, mut node: Node) -> Command {
        let mut cmd = Command::new(name)
            .about(format!("Manage {name}"))
            .subcommand_required(true)
            .arg_required_else_help(true);
        node.ops.sort_by_key(|o| o.verb);
        for op in node.ops {
            cmd = cmd.subcommand((op.build)());
        }
        for (child_name, child) in node.children {
            cmd = cmd.subcommand(materialize(child_name, child));
        }
        cmd
    }

    for (name, node) in tree.children {
        root = root.subcommand(materialize(name, node));
    }
    root.arg(
        clap::Arg::new(PEER_FLAG)
            .long("peer")
            .value_name("HOSTNAME")
            .global(true)
            .help(
                "Run this command on a remote peer over the pod mesh. Any tool \
                 that isn't marked local-only can be peer-dispatched; the peer \
                 enforces the same role checks as a local call.",
            ),
    )
}

/// Pull `--peer` out of the matched args (top-level or any subcommand level,
/// since it's a clap global). `None` means run locally.
fn extract_peer_flag(matches: &ArgMatches) -> Option<String> {
    let mut cur = matches;
    loop {
        if let Some(v) = cur.get_one::<String>(PEER_FLAG) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        match cur.subcommand() {
            Some((_, sub)) => cur = sub,
            None => return None,
        }
    }
}

use contract::RemoteExec;

/// Run an OrcaTool on a paired peer with end-to-end typed Args/Output. The
/// JSON serialization happens internally — the call site, the trait API, and
/// the rendered output all stay typed. JsonAny / Value never appear.
///
/// Peer dispatch goes through whatever `RemoteExec` the host registered on
/// the `ToolCtx`. The remote allowlist (`REMOTE_OK = true` on the tool) is
/// enforced on the peer side; calls to non-remote-ok tools surface here as
/// an error.
pub async fn exec_remote<T: contract::OrcaToolDef>(
    peer: &str,
    args: T::Args,
    ctx: &ToolCtx,
) -> Result<T::Output> {
    // Wire-only serialization: Value lives entirely behind the
    // RemoteExec trait boundary, never on a public type.
    #[allow(clippy::disallowed_types)]
    let args_value =
        serde_json::to_value(&args).map_err(|e| anyhow::anyhow!("serialize args: {e}"))?;
    let svc = ctx.service::<Arc<dyn RemoteExec>>()?;
    // Local CLI/REST entrypoints reach this path after passing their own auth
    // gate (CLI has local DB/key access; REST has session middleware). The
    // ambient operator identity on ToolCtx is set at build_tool_ctx; the
    // transport mints a signed caller token from it so the recipient can verify
    // origin and derive the role from its replicated users table.
    let result = svc
        .exec(
            peer,
            T::NAME,
            args_value,
            ctx.caller(),
            ctx.correlation_id().map(str::to_string),
        )
        .await?;
    #[allow(clippy::disallowed_types)]
    let out: T::Output = serde_json::from_value(result)
        .map_err(|e| anyhow::anyhow!("decode {} output from peer {peer}: {e}", T::NAME))?;
    Ok(out)
}

/// HTTP base URL of the local daemon's REST surface. Each orca instance sets
/// its own HTTP port independently; the CLI must dial whatever port THIS
/// instance bound. Precedence (highest to lowest):
///   1. `ORCA_DAEMON_URL` — full URL override (non-default host/port).
///   2. `ORCA_HTTP_PORT` — process-scoped port override (matches the daemon).
///   3. `$ORCA_HOME/http.port` — the port the running daemon published at bind
///      time. This is how a DB-configured per-instance port reaches the CLI,
///      which can't depend on `db`/`files` (dependency cycle) to resolve it.
///   4. `APP_REST_HTTP_PORT` (12000) — compile-time default.
pub fn local_daemon_url() -> String {
    if let Ok(url) = std::env::var("ORCA_DAEMON_URL") {
        let trimmed = url.trim_end_matches('/').to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    format!("http://127.0.0.1:{}", local_http_port())
}

/// Resolve the local daemon's HTTP port: `ORCA_HTTP_PORT` env > the port the
/// daemon published to `$ORCA_HOME/http.port` at bind time > the compile-time
/// const. Mirrors the daemon's own `db::ports::http_port()` precedence for the
/// two inputs the CLI can see without a `db` dependency.
fn local_http_port() -> u16 {
    if let Ok(raw) = std::env::var("ORCA_HTTP_PORT")
        && let Ok(p) = raw.trim().parse::<u16>()
    {
        return p;
    }
    if let Some(dir) = contract::config::orca_home()
        && let Ok(raw) = std::fs::read_to_string(dir.join("http.port"))
        && let Ok(p) = raw.trim().parse::<u16>()
    {
        return p;
    }
    contract::config::APP_REST_HTTP_PORT
}

/// Quick TCP probe of the local daemon. Returns true if a connection succeeds
/// within ~200ms — fast enough to keep CLI startup snappy on hosts where the
/// daemon isn't running (`orca install`, `orca --version`, etc.).
pub fn local_daemon_reachable() -> bool {
    use std::net::ToSocketAddrs;
    use std::time::Duration;
    let url = local_daemon_url();
    let host_port = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(&url);
    let addr = match host_port
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
    {
        Some(a) => a,
        None => return false,
    };
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

/// Proxy a tool call through the local daemon's `POST /api/v1/<name>`. Same
/// HTTP route the UI hits, same session-cookie auth, same handler — so
/// CLI = REST = MCP = daemon. Tools that need to bring up the daemon (e.g.
/// `system install`, `daemon`) set `LOCAL_ONLY = true` and bypass this path.
pub async fn exec_local_daemon<T: contract::OrcaToolDef>(
    args: T::Args,
    ctx: &ToolCtx,
) -> Result<T::Output> {
    #[allow(clippy::disallowed_types)]
    let body = serde_json::to_value(&args).map_err(|e| anyhow::anyhow!("serialize args: {e}"))?;
    let client = daemon_client().ok_or_else(|| {
        anyhow::anyhow!(
            "no daemon client installed for local dispatch of {}",
            T::NAME
        )
    })?;
    let cid = ctx.correlation_id().map(str::to_string);
    let out_value = client.post_tool(T::NAME, body, cid).await?;
    #[allow(clippy::disallowed_types)]
    let out: T::Output = serde_json::from_value(out_value)
        .map_err(|e| anyhow::anyhow!("decode {} output: {e}", T::NAME))?;
    Ok(out)
}

/// Try to dispatch one parsed clap match through the inventory.
/// Returns `Some(result)` if the (domain, verb) pair was found and ran;
/// `None` if no match — caller should fall through to legacy dispatch.
pub async fn try_dispatch(matches: &ArgMatches, ctx: Arc<ToolCtx>) -> Option<Result<()>> {
    let (domain, verb, op_matches) = walk_to_verb(matches)?;
    let op = ops().find(|o| o.domain == domain.as_str() && o.verb == verb)?;
    // Lift the per-invocation peer target onto a fresh ctx clone so the
    // shared base ctx stays immutable (REST hot-path pattern).
    let ctx = if let Some(peer) = extract_peer_flag(matches) {
        let mut owned = (*ctx).clone();
        owned.set_peer(Some(peer));
        Arc::new(owned)
    } else {
        ctx
    };
    Some((op.run)(op_matches, ctx).await)
}

/// Walk nested subcommands to the verb leaf. A node whose `.subcommand()` is
/// `Some` is treated as a domain segment (`pod` → `peer`); the first node
/// without a further subcommand is the verb. Returns `(domain, verb,
/// verb_matches)`, where `domain` is the dotted concat of the traversed
/// segments. `None` when no subcommand was selected.
fn walk_to_verb(matches: &ArgMatches) -> Option<(String, &str, &ArgMatches)> {
    let mut domain_parts: Vec<&str> = Vec::new();
    let mut cur = matches.subcommand()?;
    let (verb, op_matches) = loop {
        let (name, sub) = cur;
        if sub.subcommand().is_some() {
            domain_parts.push(name);
            cur = sub.subcommand()?;
        } else {
            break (name, sub);
        }
    };
    Some((domain_parts.join("."), verb, op_matches))
}

// ── Live unit surface (runtime service discovery) ───────────────────────────────
//
// The unit surface is not in the static `CliOp` inventory — its ops come from
// loaded plugins, known only at runtime. `unit` is internal: each kind is a
// top-level command (`orca vm …`, `orca lxc …`). The CLI process usually has no
// plugins loaded (the daemon does), so it fetches the live catalog from the
// daemon to build those commands + `--help`, and round-trips invocations through
// the same `POST /api/v1/<name>` path as REST/MCP. Falls back to the local
// catalog when the daemon isn't up (e.g. an embedded/in-process build).

/// Fetch the live managed-unit ops. Prefers the running daemon's
/// `GET /api/catalog` (so `--help` reflects what's actually loaded); falls back
/// to the in-process catalog when the daemon isn't reachable.
pub async fn fetch_unit_ops() -> Vec<crate::unit_surface::UnitOp> {
    if !local_daemon_reachable() {
        return crate::unit_surface::unit_ops();
    }
    let Some(client) = daemon_client() else {
        return crate::unit_surface::unit_ops();
    };
    match client.get_json("/api/catalog").await {
        #[allow(clippy::disallowed_types)]
        Ok(v) => serde_json::from_value::<Vec<crate::unit_surface::UnitOp>>(v)
            .unwrap_or_else(|_| crate::unit_surface::unit_ops()),
        Err(_) => crate::unit_surface::unit_ops(),
    }
}

/// Dispatch an `orca <kind> <op> [--json '{…}' | key=value …]` invocation, where
/// `<kind>` is a top-level command owned by the live unit surface (`vm`, `lxc`,
/// `container`, …). Returns `None` if the top subcommand isn't one of `kinds`
/// (caller falls through to the static op tree).
pub async fn dispatch_unit(
    matches: &ArgMatches,
    ctx: Arc<ToolCtx>,
    kinds: &[String],
) -> Option<Result<()>> {
    let (kind, kind_sub) = matches.subcommand()?;
    if !kinds.iter().any(|k| k == kind) {
        return None;
    }
    let Some((op, op_sub)) = kind_sub.subcommand() else {
        return Some(Err(anyhow::anyhow!(
            "usage: orca {kind} <op> [--json '{{…}}' | key=value …]"
        )));
    };
    let name = format!("{kind}.{op}");
    let args = match build_unit_args(op_sub) {
        Ok(a) => a,
        Err(e) => return Some(Err(e)),
    };
    Some(run_unit(&name, args, &ctx).await)
}

/// Dispatch an `orca diagnostics <diagnose|repair> [flags]` invocation. Static
/// top-level command (unlike the dynamic unit kinds). Returns `None` if the top
/// subcommand isn't `diagnostics` (caller falls through to the static op tree).
#[allow(clippy::disallowed_types)]
pub async fn dispatch_diagnostics(matches: &ArgMatches, ctx: Arc<ToolCtx>) -> Option<Result<()>> {
    use serde_json::{Map, Value};
    let (top, sub) = matches.subcommand()?;
    if top != "diagnostics" {
        return None;
    }
    let Some((op, op_sub)) = sub.subcommand() else {
        return Some(Err(anyhow::anyhow!(
            "usage: orca diagnostics <diagnose|repair> [flags]"
        )));
    };
    let mut map = Map::new();
    let (name, ok) = match op {
        "diagnose" => {
            if let Some(p) = op_sub.get_one::<String>("provider") {
                map.insert("provider".into(), Value::String(p.clone()));
            }
            ("diagnostics.diagnose", true)
        }
        "repair" => {
            for k in ["provider", "repair_id"] {
                // clap arg id is repair_id; flag is --repair-id
                if let Some(v) = op_sub.get_one::<String>(k) {
                    map.insert(k.into(), Value::String(v.clone()));
                }
            }
            ("diagnostics.repair", true)
        }
        other => ("", {
            let _ = other;
            false
        }),
    };
    if !ok {
        return Some(Err(anyhow::anyhow!("unknown diagnostics op: {op}")));
    }
    Some(run_diag(name, Value::Object(map), &ctx).await)
}

// Diagnostics op payload/response across the daemon boundary — same opaque seam
// as unit ops (typed at the contract layer; forwarded as JSON here).
#[allow(clippy::disallowed_types)]
async fn run_diag(name: &str, args: serde_json::Value, ctx: &ToolCtx) -> Result<()> {
    let out = if local_daemon_reachable() {
        post_daemon_raw(name, &args, ctx).await?
    } else {
        match crate::diagnostics_surface::diagnostics_dispatch(name, &args).await {
            Some(r) => r?,
            None => anyhow::bail!("unknown diagnostics op: {name}"),
        }
    };
    println!("{}", crate::value_to_text(&out));
    Ok(())
}

/// Parse a unit leaf's args: `--json '{…}'` wins; otherwise `key=value` pairs
/// (each value parsed as JSON, falling back to a string).
///
/// Unit-op args cross the CLI→daemon boundary as opaque JSON: each op's payload
/// is typed per-plugin (declared in its schema), but at this generic dispatch
/// layer the CLI only forwards the free-form object to the daemon's REST
/// surface — the designated opaque seam, like plugin-loader's file-level allow.
#[allow(clippy::disallowed_types)]
fn build_unit_args(m: &ArgMatches) -> Result<serde_json::Value> {
    use serde_json::{Map, Value};
    if let Some(js) = m.get_one::<String>("json") {
        return serde_json::from_str(js).map_err(|e| anyhow::anyhow!("invalid --json: {e}"));
    }
    let mut map = Map::new();
    if let Some(pairs) = m.get_many::<String>("pairs") {
        for pair in pairs {
            let (k, v) = pair
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("expected key=value, got: {pair}"))?;
            let val: Value =
                serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_string()));
            map.insert(k.to_string(), val);
        }
    }
    Ok(Value::Object(map))
}

// Opaque unit-op payload forwarded to the daemon — see [`build_unit_args`].
#[allow(clippy::disallowed_types)]
async fn run_unit(name: &str, args: serde_json::Value, ctx: &ToolCtx) -> Result<()> {
    let out = if local_daemon_reachable() {
        post_daemon_raw(name, &args, ctx).await?
    } else {
        match crate::unit_surface::unit_dispatch(name, &args).await {
            Some(r) => r?,
            None => anyhow::bail!("unknown unit op: {name}"),
        }
    };
    println!("{}", crate::value_to_text(&out));
    Ok(())
}

/// POST a raw `(name, body)` through the daemon's `/api/v1/<name>` — the same
/// route REST/MCP use. Mirrors [`exec_local_daemon`] but for a dynamic name
/// (unit ops have no static `OrcaToolDef`).
///
/// Opaque unit-op payload/response across the daemon boundary — see
/// [`build_unit_args`].
#[allow(clippy::disallowed_types)]
async fn post_daemon_raw(
    name: &str,
    body: &serde_json::Value,
    ctx: &ToolCtx,
) -> Result<serde_json::Value> {
    let client =
        daemon_client().ok_or_else(|| anyhow::anyhow!("no daemon client installed for {name}"))?;
    let cid = ctx.correlation_id().map(str::to_string);
    client.post_tool(name, body.clone(), cid).await
}

/// Register one op with the unified CLI surface.
///
/// ```ignore
/// register_op! {
///     tool: EngineList,
///     domain: "engine",
///     verb: "list",
///     summary: "List registered LLM backends",
///     render: |out| {
///         for p in out.0 { println!("{} {}", p.name, p.url); }
///     }
/// }
/// ```
///
/// Requires `Tool::Args: clap::Args` so the macro can auto-derive the
/// subcommand's flags from the same struct that MCP/REST already serialize.
#[macro_export]
macro_rules! register_op {
    // Default form: pretty-print the output as JSON. Use this during the
    // mechanical sweep and override per-tool when human-friendly output
    // matters (e.g. `engine list` colored table).
    (
        tool: $tool:path,
        domain: $domain:expr,
        verb: $verb:expr,
        summary: $summary:expr $(,)?
    ) => {
        $crate::register_op! {
            crate_path: ::plugin_toolkit,
            tool: $tool,
            domain: $domain,
            verb: $verb,
            summary: $summary,
        }
    };
    (
        crate_path: $cp:path,
        tool: $tool:path,
        domain: $domain:expr,
        verb: $verb:expr,
        summary: $summary:expr $(,)?
    ) => {
        $crate::register_op! {
            crate_path: $cp,
            tool: $tool,
            domain: $domain,
            verb: $verb,
            summary: $summary,
            render: |out| {
                let s = __cp::serde_json::to_string_pretty(&out)
                    .unwrap_or_else(|e| format!("<unserializable output: {e}>"));
                println!("{s}");
            }
        }
    };
    (
        tool: $tool:path,
        domain: $domain:expr,
        verb: $verb:expr,
        summary: $summary:expr,
        render: |$out:ident| $render:block $(,)?
    ) => {
        $crate::register_op! {
            crate_path: ::plugin_toolkit,
            tool: $tool,
            domain: $domain,
            verb: $verb,
            summary: $summary,
            render: |$out| $render
        }
    };
    (
        crate_path: $cp:path,
        tool: $tool:path,
        domain: $domain:expr,
        verb: $verb:expr,
        summary: $summary:expr,
        render: |$out:ident| $render:block $(,)?
    ) => {
        const _: () = {
            use $cp as __cp;
            use __cp::dispatch::cli::{CliOp, CliBuildFn, CliRunFn};
            use __cp::contract::{OrcaTool, OrcaToolDef};

            fn build() -> __cp::clap::Command {
                let cmd = __cp::clap::Command::new($verb).about($summary);
                // Cross-cutting `--peer <PEER>` is registered as a global flag
                // on the root command (see `build_root`) and propagates to
                // every subcommand automatically. Don't redeclare it here —
                // clap rejects duplicate `long` names on globals.
                <<$tool as OrcaToolDef>::Args as __cp::clap::Args>::augment_args(cmd)
            }

            fn run(
                m: &__cp::clap::ArgMatches,
                ctx: ::std::sync::Arc<__cp::contract::ToolCtx>,
            ) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = __cp::anyhow::Result<()>> + Send>> {
                let m = m.clone();
                Box::pin(async move {
                    // `try_dispatch` already lifted the global `--peer` value
                    // onto the ctx clone before invoking us; read it there.
                    let peer = ctx.peer().map(|s| s.to_string());
                    let args = <<$tool as OrcaToolDef>::Args as __cp::clap::FromArgMatches>::from_arg_matches(&m)
                        .map_err(|e| __cp::anyhow::anyhow!("{e}"))?;
                    let $out: <$tool as OrcaToolDef>::Output = if let Some(peer) = peer {
                        __cp::dispatch::cli::exec_remote::<$tool>(&peer, args, &ctx).await?
                    } else if <$tool as OrcaToolDef>::LOCAL_ONLY
                        || !__cp::dispatch::cli::local_daemon_reachable()
                    {
                        // Pre-daemon ops (install, daemon start, --version) and
                        // every CLI invocation on a host that isn't running orca
                        // execute in-process. Once the daemon is up, every other
                        // tool round-trips through its HTTP surface so CLI = REST
                        // = MCP — see [[feedback-cli-api-mcp-one-path]].
                        <$tool as OrcaTool>::run(args, &ctx).await?
                    } else {
                        __cp::dispatch::cli::exec_local_daemon::<$tool>(args, &ctx).await?
                    };
                    { $render }
                    Ok(())
                })
            }

            __cp::inventory::submit! {
                CliOp {
                    domain: $domain,
                    verb: $verb,
                    summary: $summary,
                    build: build as CliBuildFn,
                    run: run as CliRunFn,
                }
            }
        };
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Command;

    fn flat_root() -> Command {
        Command::new("orca").subcommand(
            Command::new("engine")
                .subcommand_required(true)
                .subcommand(Command::new("list")),
        )
    }

    fn nested_root() -> Command {
        Command::new("orca").subcommand(
            Command::new("pod")
                .subcommand_required(true)
                .subcommand(
                    Command::new("peer")
                        .subcommand_required(true)
                        .subcommand(Command::new("list")),
                )
                .subcommand(Command::new("list")), // pod.list lives alongside pod.peer.*
        )
    }

    #[test]
    fn walk_to_verb_flat_domain() {
        let m = flat_root().get_matches_from(["orca", "engine", "list"]);
        let (domain, verb, _) = walk_to_verb(&m).unwrap();
        assert_eq!(domain, "engine");
        assert_eq!(verb, "list");
    }

    #[test]
    fn walk_to_verb_nested_domain() {
        let m = nested_root().get_matches_from(["orca", "pod", "peer", "list"]);
        let (domain, verb, _) = walk_to_verb(&m).unwrap();
        assert_eq!(domain, "pod.peer");
        assert_eq!(verb, "list");
    }

    #[test]
    fn walk_to_verb_mixed_tree_resolves_shallow_verb() {
        // `pod.list` must still resolve when `pod.peer.*` exists as a sibling
        // branch under the same `pod` segment.
        let m = nested_root().get_matches_from(["orca", "pod", "list"]);
        let (domain, verb, _) = walk_to_verb(&m).unwrap();
        assert_eq!(domain, "pod");
        assert_eq!(verb, "list");
    }

    #[test]
    fn walk_to_verb_no_subcommand_returns_none() {
        let m = Command::new("orca")
            .subcommand(Command::new("engine"))
            .get_matches_from(["orca"]);
        assert!(walk_to_verb(&m).is_none());
    }

    #[test]
    fn ops_iterator_returns_inventory_entries() {
        // Smoke: just ensure the iterator can be exhausted without panicking.
        let count = ops().count();
        // dispatch's own test binary has no registered ops, but inventory
        // may carry entries pulled in from upstream crates — either is fine.
        let _ = count;
    }

    #[test]
    fn build_root_adds_global_peer_flag_even_with_empty_inventory() {
        let cmd = build_root(Command::new("orca"));
        // The global --peer flag is always present.
        let m = cmd
            .clone()
            .try_get_matches_from(["orca", "--peer", "host-e"]);
        assert!(m.is_ok(), "global --peer must parse: {m:?}");
    }

    #[test]
    fn extract_peer_flag_reads_top_level_then_subcommand_then_skips_empty() {
        let root = || {
            Command::new("orca")
                .arg(clap::Arg::new(PEER_FLAG).long("peer").global(true))
                .subcommand(
                    Command::new("engine")
                        .subcommand_required(true)
                        .subcommand(Command::new("list")),
                )
        };

        // Top-level set.
        let m = root().get_matches_from(["orca", "--peer", "host-a", "engine", "list"]);
        assert_eq!(extract_peer_flag(&m).as_deref(), Some("host-a"));

        // Subcommand-level set (clap globals attach there too).
        let m = root().get_matches_from(["orca", "engine", "list", "--peer", "host-b"]);
        assert_eq!(extract_peer_flag(&m).as_deref(), Some("host-b"));

        // Whitespace-only value is treated as None.
        let m = root().get_matches_from(["orca", "--peer", "   ", "engine", "list"]);
        assert!(extract_peer_flag(&m).is_none());

        // Absent.
        let m = root().get_matches_from(["orca", "engine", "list"]);
        assert!(extract_peer_flag(&m).is_none());
    }

    #[tokio::test]
    async fn try_dispatch_returns_none_for_unregistered_domain() {
        // Construct a synthetic root containing a subcommand whose
        // (domain, verb) pair is guaranteed not to match any registered op.
        let cmd = Command::new("orca").subcommand(
            Command::new("__orca_unregistered_domain_xyz")
                .subcommand_required(true)
                .subcommand(Command::new("nope")),
        );
        let m = cmd.get_matches_from(["orca", "__orca_unregistered_domain_xyz", "nope"]);
        let cfg = std::sync::Arc::new(contract::config::Config::load().unwrap());
        let ctx = std::sync::Arc::new(contract::ToolCtx::new(cfg));
        assert!(try_dispatch(&m, ctx).await.is_none());
    }

    #[tokio::test]
    async fn try_dispatch_returns_none_when_no_subcommand_selected() {
        let m = Command::new("orca").get_matches_from(["orca"]);
        let cfg = std::sync::Arc::new(contract::config::Config::load().unwrap());
        let ctx = std::sync::Arc::new(contract::ToolCtx::new(cfg));
        assert!(try_dispatch(&m, ctx).await.is_none());
    }

    #[tokio::test]
    async fn exec_remote_propagates_through_registered_remote_exec_service() {
        use contract::{CallerIdentity, OrcaToolDef, RemoteExec};
        use schemars::JsonSchema;
        use serde::{Deserialize, Serialize};
        use std::sync::Arc;

        // Minimal tool definition.
        #[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
        struct Args {
            x: u32,
        }
        #[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
        struct Out {
            y: u32,
        }
        struct ExampleTool;
        impl OrcaToolDef for ExampleTool {
            type Args = Args;
            type Output = Out;
            const NAME: &'static str = "example.echo";
            const DESCRIPTION: &'static str = "echo";
        }

        struct DoubleExec;
        #[async_trait::async_trait]
        impl RemoteExec for DoubleExec {
            #[allow(clippy::disallowed_types)]
            async fn exec(
                &self,
                peer: &str,
                tool: &str,
                args: serde_json::Value,
                _caller: Option<CallerIdentity>,
                _correlation_id: Option<String>,
            ) -> anyhow::Result<serde_json::Value> {
                assert_eq!(peer, "host-x");
                assert_eq!(tool, "example.echo");
                let x = args["x"].as_u64().unwrap() as u32;
                Ok(serde_json::json!({ "y": x * 2 }))
            }
        }

        let cfg = Arc::new(contract::config::Config::load().unwrap());
        let mut ctx = contract::ToolCtx::new(cfg);
        let svc: Arc<dyn RemoteExec> = Arc::new(DoubleExec);
        ctx.register_service(svc);
        let out = exec_remote::<ExampleTool>("host-x", Args { x: 21 }, &ctx)
            .await
            .unwrap();
        assert_eq!(out, Out { y: 42 });
    }

    #[tokio::test]
    async fn exec_remote_errors_when_no_remote_exec_service_registered() {
        use contract::OrcaToolDef;
        use schemars::JsonSchema;
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, JsonSchema)]
        struct A;
        #[derive(Serialize, Deserialize, JsonSchema)]
        struct B;
        struct T2;
        impl OrcaToolDef for T2 {
            type Args = A;
            type Output = B;
            const NAME: &'static str = "ex.t2";
            const DESCRIPTION: &'static str = "t2";
        }

        let cfg = std::sync::Arc::new(contract::config::Config::load().unwrap());
        let ctx = contract::ToolCtx::new(cfg);
        assert!(exec_remote::<T2>("h", A, &ctx).await.is_err());
    }
}
