# Orca Plugin Authoring Guide

Orca supports two plugin mechanisms. Pick based on language and coupling:

| | **Native subprocess plugin** | **Manifest plugin (`orca-plugin.toml`)** |
|---|---|---|
| Language | Rust | any (MCP SDK) |
| Runs | own process, spoken to over a Unix socket | external process / HTTP endpoint |
| Tool model | `#[orca_tool]` + inventory, served via a `serve_*_plugin!` macro | MCP tools over stdio / HTTP-SSE |
| Author depends on | `plugin-toolkit` | the MCP SDK of your language |
| Compatibility | wire protocol-major negotiation (`plugin-proto`) | MCP protocol |
| When | first-party integrations, typed access to orca contracts | non-Rust, third-party, experimental |

Both are out-of-process — orca links **no plugin into its address space**. The
mechanism behind native plugins (wire protocol, capability delegation, the
loader supervisor) is described in [`dynamic-linking.md`](dynamic-linking.md);
this guide is how to *write* one.

> **There is no in-process linking.** A native plugin is a normal Rust `rlib`
> crate with a `[[bin]]` target that orca runs as a **persistent child
> process** and talks to over a Unix socket. There is no `abi_stable`, no
> `dlopen`/`libloading`, no `cdylib`, no `#[export_root_module]` / `PluginMod`.
> See [`OUT-OF-PROCESS-PLUGINS.md`](OUT-OF-PROCESS-PLUGINS.md) for why (crash
> isolation, size, ABI/libc coupling) and [`dynamic-linking.md`](dynamic-linking.md)
> for the mechanism.
>
> The legacy in-process cdylib form (`crate-type = ["cdylib"]`,
> `export_service_plugin!` / `export_tool_plugin!`, no `main.rs`) has been
> **fully retired**: every first-party plugin under `argyle-labs/*` is now a
> `[[bin]]` subprocess, and no repo builds, publishes, or `dlopen`s a cdylib.
> The rule that carries across the port:
> **plugin-toolkit only, no exceptions**. A crate is allowed as a direct plugin
> dependency only when the plugin is its **sole consumer** (see the
> sole-consumer rule below).

---

# Part 1 — Native subprocess plugins (Rust)

A native plugin is a normal Rust `rlib` crate with a `[[bin]]` target. orca
runs that binary as a **persistent child process**; on startup it connects back
to the orca daemon over a Unix-domain socket, declares its tool surface in a
`Hello` frame, and then serves tool invocations — delegating any HTTP / DB /
secret work back to the daemon as capabilities. There is no `cdylib`, no
`dlopen`, no `abi_stable`.

## Anatomy

```
my-plugin/
├── Cargo.toml          ← a normal rlib crate with a [[bin]] target
├── build.rs            ← (optional) codegen typed clients from OpenAPI/GraphQL specs
├── specs/              ← (optional) vendored OpenAPI/GraphQL spec files
└── src/
    ├── main.rs         ← connect + serve loop
    └── tools.rs        ← #[orca_tool] functions
```

### `Cargo.toml`

```toml
[dependencies]
# The single gateway to the whole orca surface. Re-exports the contract,
# dispatch, the #[orca_tool] macro, the wire protocol (plugin-proto), and the
# host capabilities (http/db/secret). A native plugin's ONLY orca dependency.
#
# A standalone plugin repo depends on the toolkit by GIT so it resolves without
# the orca tree checked out. For local development, override it to an in-tree
# checkout with a `[patch]` in `.cargo/config.toml`.
plugin-toolkit = { git = "https://github.com/argyle-labs/orca", branch = "main" }

[build-dependencies]
# Only if you codegen typed HTTP/GraphQL clients in build.rs.
plugin-toolkit-build = { git = "https://github.com/argyle-labs/orca", branch = "main" }
```

`plugin-toolkit` is the **only** orca dependency a plugin needs
(`feedback-plugin-toolkit-only-no-exceptions`). It re-exports the contract,
dispatch, the wire protocol, and the runtime deps a plugin uses (`serde`,
`schemars`, `clap`, `inventory`, `anyhow`) so plugins never pin those directly.

#### The sole-consumer dependency rule

A plugin depends on **its own domain client + `plugin-toolkit`, and nothing
else**. Async/time/HTTP plumbing rides **orca-owned seams** the toolkit exposes —
the HTTP client (`plugin_toolkit::client`), time (`plugin_toolkit::time`),
subprocess (`plugin_toolkit::process`), and the shared reactor
(`plugin_toolkit::reactor`) — so a plugin **never names `tokio`, `futures`,
`reqwest`, or `chrono` directly**.

**Re-export is not abstraction.** The seam is the boundary, not a renamed
passthrough of a third-party crate. The toolkit does **not** re-export `reqwest`
or `futures_util` into a plugin's namespace: a plugin builds an orca `Request`
and reads an orca `Response`/`Stream`, and never sees the crate underneath.
(`tokio` is deliberately not re-exported for the same reason.)

Two rules follow:

- **Sole-consumer justification.** A crate is admissible as a *direct* plugin
  dependency only when the plugin is that crate's **sole consumer** — its own
  generated/domain client. Anything a *second* plugin would also want belongs in
  core, reached over a seam. The same test applies to a seam itself: a new seam
  is justified when it has a sole consumer that core owns the other end of.
- **Thin plugin, maximal core.** Everything heavy lives in core and is reached
  over a capability or an orca-owned surface; the plugin carries only its own
  logic + generated types + serde.

### The serve loop (`main.rs`)

You do **not** hand-write the connect/handshake/dispatch loop. A plugin binary
boots its serve loop with one of the `serve_*_plugin!` macros
(`projects/plugin-toolkit/src/serve_macros.rs`), which each emit a whole
`fn main()` that connects the orca-provided socket (`$ORCA_PLUGIN_SOCKET`),
sends `Hello`, major-checks `Welcome`, and then serves `Invoke → dispatch →
Result` until orca sends `Shutdown`. Pick the macro that matches the plugin's
shape:

```rust
// 1. Pure tool-surface plugin — manifest is this plugin's own `"{name}."`
//    slice of the linked #[orca_tool] inventory.
plugin_toolkit::serve_tool_plugin! { name: "docker", target_compat: ">=20.10" }

// 2. Hybrid tool + registered domain backend — `backends` yields the backends
//    JSON; `backend_dispatch: fn(&str, &str) -> Option<Result<String, String>>`
//    handles the domain's `*.__backend.*` calls (return `None` to fall through
//    to #[orca_tool] dispatch).
plugin_toolkit::serve_tool_plugin! {
    name: "ntfy", target_compat: "",
    backends: ntfy_backends_json(),
    backend_dispatch: ntfy_backend_dispatch,
}

// 3. Service-backend plugin.
plugin_toolkit::serve_service_plugin! {
    name: "audiobookshelf",
    target_compat: "any",
    backend: AudiobookshelfBackend::new("audiobookshelf"),
}

// 4. Storage-backend plugin.
plugin_toolkit::serve_storage_plugin! {
    name: "smb",
    target_compat: "any",
    backend: SmbBackend::new("smb"),
}
```

Under the hood each macro calls `plugin_toolkit::serve::serve(PluginSpec { .. })`
with `version` derived from `CARGO_PKG_VERSION`. A plugin retires a legacy
cdylib export by swapping the macro name (`export_service_plugin!` →
`serve_service_plugin!`) and declaring a `[[bin]]` instead of a cdylib.

- The daemon reads `Hello`, checks the protocol **major**, and replies
  `Welcome` with the capabilities it offers. Mismatch ⇒ clean refusal.
- dispatch walks the link-time `inventory` slice, finds the named tool, and
  runs it on the shared reactor. Tool bodies are async; the toolkit drives them.

### Registering tools (`tools.rs`)

The same `#[orca_tool]` macro the in-tree domain crates use:

```rust
use plugin_toolkit::prelude::*;

#[orca_tool(domain = "jellyfin", verb = "server_info")]
/// Return Jellyfin server identity + version.
pub async fn server_info(_ctx: &ToolCtx) -> Result<ServerInfo, OrcaError> {
    // reach the network via the orca-owned HTTP client — never a linked reqwest
    let resp = plugin_toolkit::client::Client::new()
        .get("https://jellyfin.local/System/Info")?;
    let info: ServerInfo = resp.json()?;
    Ok(info)
}
```

The macro emits an `OrcaTool` impl and an `inventory::submit!` registration
named `jellyfin.server_info`. For standard CRUD surfaces, `endpoint_resource!`
generates the five `{list,detail,create,update,delete}` tools from one
declaration. For unit-shaped resources (containers, vms), register against the
Managed Unit surface so they appear as `orca <kind> <verb>`.

### Build-time client codegen (`build.rs`)

If the plugin wraps a documented HTTP or GraphQL API, generate a typed client
from the spec rather than hand-writing untyped calls:

```rust
fn main() {
    plugin_toolkit_build::openapi::generate_all("specs", "jellyfin_client");
    // or: plugin_toolkit_build::graphql::generate("schema", "queries");
}
```

`plugin-toolkit-build` rewrites the generated code's crate paths to
`::plugin_toolkit::*`, so the plugin never depends on `progenitor` /
`graphql_client_codegen` directly. The generated client issues requests through
the host `http.request` capability, not a bundled HTTP stack.

### Reading/writing orca core tables (`core_tables`)

A plugin's own data lives in its **namespaced** tables. But a thin/subprocess
plugin sometimes needs a fixed set of **orca-owned core tables** — `mcp_servers`,
`mcp_tool_mappings`, `openapi_specs`, `plugins`, `plugin_credentials` (the MCP
client is the first caller). Reach them through
`plugin_toolkit::core_tables::*`:

```rust
use plugin_toolkit::core_tables::{mcp_servers, plugins};

let servers = mcp_servers::list()?;        // enabled servers, sorted by name
let all_plugins = plugins::list()?;        // registered plugins, sorted by id
mcp_servers::upsert(&server)?;
```

These accessors route over the **same** capability sink as every other DB
call — `runtime::db_op` — but with the **empty-namespace convention**: an empty
namespace string (`""`) plus a **literal core table name**. Core resolves an
empty namespace to the bare (un-prefixed) table, so a plugin reaches
`mcp_servers` through the identical FFI/cap path it already uses for its own
namespaced data. The module rides the light `db` feature — no `rusqlite`, no
`db` crate. The `DbOp` surface carries only `List`/`Get`/`Upsert`/`Delete` (no
`WHERE`/`ORDER BY`), so the accessors `List`/`Get`, decode `DbRow` → typed, and
then filter and sort **in Rust**.

### HTTP and streaming (`client`)

A plugin links **no reqwest, no rustls, no `futures_util`**. It makes HTTP
requests through the orca-owned client seam,
`plugin_toolkit::client::{Client, Request, Response}` — a plugin builds an orca
`Request` and reads an orca `Response`; the reqwest/TLS stack lives host-side and
is reached over the `http.request` / `http.stream` capabilities.

```rust
use plugin_toolkit::client::{Client, Request};

let http = Client::new();

// Buffered request/response.
let resp = http.get("https://host/System/Info")?;      // convenience GET
let resp = http.post_json("https://host/Items", &body)?; // convenience POST
let resp = http.send(Request::new("PUT", url).json(&body)?)?; // full builder
if resp.is_success() { let v: MyType = resp.json()?; }

// Streaming — the body is NOT buffered host-side.
let mut bytes = http.stream(Request::new("GET", url))?;   // ByteStream
while let Some(chunk) = bytes.next() { /* … */ }

let mut events = http.events(Request::new("GET", sse_url))?; // EventStream (SSE)
while let Some(ev) = events.next() { /* ev.event / ev.data */ }
```

`ByteStream` / `EventStream` own their own `next()` — the orca-owned equivalent
of draining reqwest's `bytes_stream()` — so a consumer **never names `futures`'s
`StreamExt` or reqwest's `bytes_stream`**. Streaming rides cap-frames
(`http.stream`) under the hood; the plugin only sees the orca surface. This is
the concrete case of *re-export is not abstraction*: the toolkit does not hand a
plugin reqwest or `futures_util` — the orca-owned `Request`/`Response`/`Stream`
types are the boundary.

### Talking to a long-lived child process (`process`)

A plugin that drives an external line-oriented peer (e.g. a JSON-RPC subprocess)
spawns it through the orca-owned `plugin_toolkit::process` surface, never naming
the runtime's process API:

```rust
use plugin_toolkit::process::Command;
use std::time::Duration;

let child = Command::new("some-jsonrpc-server").arg("--stdio").spawn()?;

// request  = correlated round-trip: mints an id, writes, returns the matching reply.
let reply = child.request(r#"{"jsonrpc":"2.0","method":"ping"}"#, Duration::from_secs(5)).await?;
// notify   = fire-and-forget: a message with no id and no awaited response.
child.notify(r#"{"jsonrpc":"2.0","method":"log"}"#).await?;
// kill     = explicit early stop (the child is also killed on drop).
```

The plugin-facing surface is **`request` / `notify` / `kill`**. The lower-level
`write_line` / `read_line` helpers are `cfg(test)`-gated internal helpers, not
part of the plugin surface — drive the peer through `request`/`notify` instead.

## How the daemon runs it

`plugin-loader`'s supervisor spawns the plugin process, hands it a socket,
performs the handshake, registers its `manifest`/`backends`/`schema`, and
routes matching tool calls as `Invoke` frames. A plugin crash takes down only
that plugin. See [`dynamic-linking.md`](dynamic-linking.md) for the full
lifecycle and the capability protocol.

---

# Part 2 — Manifest plugins (`orca-plugin.toml`)

For non-Rust or third-party integrations. A manifest registers an external
MCP server (stdio or HTTP/SSE) plus optional nav links, command aliases, vault
roots, and agents.

> **Future seam — `plugin_toolkit::manifest`.** Today, manifest `toml` +
> `parse_path` parsing is done **in core** (`db::plugin_manifest`) for
> registration, and any plugin that needs to read a manifest inlines its own
> `toml` parse. A future `plugin_toolkit::manifest` seam will absorb that
> in-plugin parsing so plugins reach a manifest through the toolkit instead of
> naming `toml` directly. It is **not built yet** — do not assume a
> `plugin_toolkit::manifest` module exists.

## The manifest

```toml
[plugin]
id                = "my-plugin"           # unique, lowercase, hyphenated
version           = "0.1.0"
tier              = "personal"            # personal | external | homelab
description       = "What this plugin does"
context_injection = "minimal"             # minimal | full

# ── MCP server (stdio) ──────────────────────────────────────────────────────
[plugin.mcp]
command = "node"
args    = ["/abs/path/to/dist/index.js"]

[plugin.mcp.env]
MY_API_KEY = ""   # projected from the secret backend at dial time

# ── MCP server (HTTP/SSE) — alternative to stdio ────────────────────────────
# [plugin.mcp]
# url       = "http://10.0.0.5:12050"   # or `urls = [...]` for LAN/TS fallbacks
# token_env = "MY_PLUGIN_TOKEN"           # env var holding the bearer token

# ── Command aliases: short alias → MCP tool name ────────────────────────────
[plugin.commands]
run    = "my_plugin_run"
status = "my_plugin_status"

# ── Sidebar navigation ──────────────────────────────────────────────────────
[[plugin.nav_links]]
href  = "/my-page"
label = "My Page"

[plugin.agents]
manifest_dir = "agents/"
```

> Transport (`command`/`args`/`url`) lives in the manifest on disk, not in DB
> columns — the host re-reads it at dial time.

### Registration

```bash
orca plugin install ~/code/my-plugin/orca-plugin.toml
orca plugin list
orca plugin uninstall my-plugin
```

### Writing the MCP server (TypeScript example)

```typescript
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";

const server = new McpServer({ name: "my-plugin", version: "0.1.0" });

server.tool(
  "my_plugin_run",
  "Short description of what this tool does.",
  { input: z.string().describe("The input value") },
  async ({ input }) => ({ content: [{ type: "text", text: doWork(input) }] }),
);

await server.connect(new StdioServerTransport());
```

---

# Persistent data (`plugin_data`)

Both plugin types can store encrypted per-plugin key/value data via the host
`db.op` capability (native) or the REST surface (manifest):

```
GET    /api/plugins/{id}/data           → list all entries
GET    /api/plugins/{id}/data/{key}     → get one
PUT    /api/plugins/{id}/data/{key}     → set { "value": "..." }
DELETE /api/plugins/{id}/data/{key}     → delete
```

---

# Agents

Agents are a **core domain**, not a plugin. The domain lives in core at
`projects/agents`; the embedded base roster loads in-core via
`agents::embedded::register_base_roster()`. Its registration machinery is exposed
through `plugin_toolkit` exactly like `db` / `secret` / `storage`, so any plugin
can contribute agents, hooks, skills, slash commands, and prompt fragments into
the core domain. Agents surface through the `agent.{list,get,run}` tools and the
`orca agents` CLI (ROADMAP §1.9).

## Registering agents from a plugin

A native plugin registers by calling `plugin_toolkit::agents::register`, passing
an `AgentRegistration` (from `plugin_toolkit::abi`) carrying a `name` plus five
JSON-array-string fields:

```rust
use plugin_toolkit::agents::register;
use plugin_toolkit::abi::AgentRegistration;

register(AgentRegistration {
    name: "my-plugin".into(),
    agents_json,           // JSON array of agent definitions
    hooks_json,            // JSON array of hooks
    skills_json,           // JSON array of skills
    commands_json,         // JSON array of slash commands
    prompt_fragments_json, // JSON array of CLAUDE.md fragments
})?;
```

This sends the `agents.register` capability over the capability channel; the host
routes it into the core agents domain (`agents::registry::register_from_json` →
`agents::register_provider`) — the same seam pattern as the `db.op` / `secret.op`
capabilities. Nothing lives in `projects/plugins`.

A **manifest plugin** contributes agent definitions declaratively via the
`[plugin.agents]` `manifest_dir` shown above.

---

# Checklist

**Native subprocess plugin:**
- [ ] `[[bin]]` crate; single `plugin-toolkit` orca dependency
- [ ] `main()` connects back, sends `Hello` (protocol, name, version, manifest), runs `serve`
- [ ] tools declared with `#[orca_tool]` / `endpoint_resource!`
- [ ] all HTTP / DB / secret access goes through host capabilities, never linked deps
- [ ] (spec-first) `build.rs` codegen via `plugin-toolkit-build`

**Manifest plugin:**
- [ ] `orca-plugin.toml` with `id`, `version`, `tier`, `[plugin.mcp]`
- [ ] MCP server with ≥1 tool; absolute paths in `args`
- [ ] `orca plugin install ~/path/to/orca-plugin.toml`; verify with `orca plugin list`
