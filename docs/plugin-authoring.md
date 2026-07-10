# Orca Plugin Authoring Guide

Orca supports two plugin mechanisms. Pick based on language and coupling:

| | **Native subprocess plugin** | **Manifest plugin (`orca-plugin.toml`)** |
|---|---|---|
| Language | Rust | any (MCP SDK) |
| Runs | own process, spoken to over a Unix socket | external process / HTTP endpoint |
| Tool model | `#[orca_tool]` + inventory, served via `plugin_toolkit::serve` | MCP tools over stdio / HTTP-SSE |
| Author depends on | `plugin-toolkit` | the MCP SDK of your language |
| Compatibility | wire protocol-major negotiation (`plugin-proto`) | MCP protocol |
| When | first-party integrations, typed access to orca contracts | non-Rust, third-party, experimental |

Both are out-of-process — orca links **no plugin into its address space**. The
mechanism behind native plugins (wire protocol, capability delegation, the
loader supervisor) is described in [`dynamic-linking.md`](dynamic-linking.md);
this guide is how to *write* one.

> The former in-process `cdylib` / `abi_stable` model has been removed. There
> is no `#[export_root_module]`, no `PluginMod`, no `dlopen`. Do not resurrect
> it.

---

# Part 1 — Native subprocess plugins (Rust)

A native plugin is a Rust crate that compiles to a **small binary**. On
startup it connects back to the orca daemon over a Unix-domain socket,
declares its tool surface in a `Hello` frame, and then serves tool invocations
— delegating any HTTP / DB / secret work back to the daemon as capabilities.

## Anatomy

```
my-plugin/
├── Cargo.toml          ← a normal [[bin]] crate
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
dispatch, the wire protocol, and runtime deps (`serde`, `schemars`, `clap`,
`inventory`, `tokio`) so plugins never pin those directly.

### The serve loop (`main.rs`)

The plugin connects to the socket orca hands it, sends `Hello` with its tool
manifest, and runs the session loop. `plugin_toolkit` re-exports the protocol:

```rust
use plugin_toolkit::proto::{serve, Frame, PROTOCOL_VERSION};

fn main() -> anyhow::Result<()> {
    let stream = plugin_toolkit::connect_back()?;   // UDS handed over by the daemon

    let hello = Frame::Hello {
        protocol: PROTOCOL_VERSION.into(),
        plugin:   "jellyfin".into(),                 // the catalog / install key
        version:  env!("CARGO_PKG_VERSION").into(),
        manifest: plugin_toolkit::tool_manifest(),   // Vec<ToolDef> from the inventory slice
        backends: plugin_toolkit::backends(),        // domain backends this plugin registers
        schema:   Default::default(),                // optional declared SQL schema (none)
    };

    // The handler runs each Invoke; `caps` lets a tool call host capabilities
    // (http.request / db.op / secret.op) back over the same socket.
    serve(stream, hello, |tool, args, caps| {
        plugin_toolkit::dispatch(tool, args, caps)
    })
}
```

- The daemon reads `Hello`, checks the protocol **major**, and replies
  `Welcome` with the capabilities it offers. Mismatch ⇒ clean refusal.
- `dispatch` walks the link-time `inventory` slice, finds the named tool, and
  runs it. Tool bodies are async; the toolkit drives them.

### Registering tools (`tools.rs`)

The same `#[orca_tool]` macro the in-tree domain crates use:

```rust
use plugin_toolkit::prelude::*;

#[orca_tool(domain = "jellyfin", verb = "server_info")]
/// Return Jellyfin server identity + version.
pub async fn server_info(ctx: &ToolCtx) -> Result<ServerInfo, OrcaError> {
    // reach the network via the host HTTP capability — never a linked reqwest
    let resp = ctx.http().get("/System/Info").await?;
    // ...
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
roots, and agents. The schema is parsed by `db::plugin_manifest`.

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

Plugins ship agent definitions — markdown files with YAML frontmatter — in the
`agents/` directory declared by `manifest_dir`. Agents surface through the
`agent.{list,get,run}` tools and the `orca agents` CLI (ROADMAP §1.9).

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
