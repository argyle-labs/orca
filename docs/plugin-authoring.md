# Orca Plugin Authoring Guide

Orca supports two plugin mechanisms. Pick based on language and coupling:

| | **Native cdylib plugin** | **Manifest plugin (`orca-plugin.toml`)** |
|---|---|---|
| Language | Rust | any (MCP SDK) |
| Runs | in-process (dlopen + `abi_stable`) | external process / HTTP endpoint |
| Tool model | `#[orca_tool]` + inventory | MCP tools over stdio / HTTP-SSE |
| Author depends on | `plugin-toolkit` | the MCP SDK of your language |
| Reference | [`argyle-labs/jellyfin`](https://github.com/argyle-labs/jellyfin) | any MCP server |
| When | hot path, typed access to orca internals, first-party | non-Rust, out-of-process, third-party, experimental |

The native cdylib model is the current path for first-party integrations. The
manifest model remains for non-Rust and out-of-process plugins.

---

# Part 1 — Native cdylib plugins (Rust)

A native plugin is a Rust crate compiled to a `cdylib`. Orca's `plugin-loader`
opens it at runtime with `abi_stable`, performs a layout + version
compatibility gate, and dispatches tool calls into it in-process. No IPC.

## Anatomy

```
my-plugin/
├── Cargo.toml          ← crate-type = ["cdylib", "rlib"]
├── build.rs            ← (optional) codegen typed clients from OpenAPI/GraphQL specs
├── specs/              ← (optional) vendored OpenAPI/GraphQL spec files
└── src/
    ├── lib.rs          ← plugin logic
    ├── abi_export.rs   ← exports the ABI root module
    └── tools.rs        ← #[orca_tool] functions
```

### `Cargo.toml`

```toml
[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
# The single gateway to the whole orca surface. Pull domains via features
# (tools, db, containers, notify, graphql, openapi, http). smb-style storage
# adapters can use `default-features = false` for a thin slice.
#
# A standalone plugin repo depends on the toolkit by GIT so it resolves
# without the orca tree checked out. For local development, override it to an
# in-tree checkout with a `[patch]` in `.cargo/config.toml` (see any
# first-party plugin repo, e.g. argyle-labs/proxmox).
plugin-toolkit = { git = "https://github.com/argyle-labs/orca", branch = "main" }
# Direct (non-rewritable) dep: #[export_root_module] expands to bare
# `::abi_stable` paths, so it must be a real dependency of the plugin. Pin to
# the orca workspace version (0.11) so the cdylib's layout hash matches what
# plugin-loader checks at load time.
abi_stable = "0.11"

[build-dependencies]
# Only if you codegen typed HTTP/GraphQL clients in build.rs.
plugin-toolkit-build = { git = "https://github.com/argyle-labs/orca", branch = "main" }
```

`plugin-toolkit` is the **only** orca dependency a plugin needs
(`feedback-plugin-toolkit-only-no-exceptions`). It re-exports the contract,
dispatch, domain crates (`storage`, `containers`, `notify`), and runtime deps
(`serde`, `schemars`, `clap`, `inventory`, `tokio`, `abi_stable`) so plugins
never pin those directly.

### The ABI root module (`abi_export.rs`)

The contract lives in `plugin-abi` as `PluginModRef` (an `abi_stable`
prefix-typed `RootModule`). A plugin exports it with `#[export_root_module]`:

```rust
use abi_stable::{export_root_module, prefix_type::PrefixTypeTrait};
use plugin_toolkit::abi::{PluginMod, PluginModRef};

#[export_root_module]
fn export() -> PluginModRef {
    PluginMod {
        plugin_semver,    // -> RString  (this plugin's version)
        target_software,  // -> RString  (e.g. "jellyfin")
        target_compat,    // -> RString  (target software version range, e.g. "10.8-10.10")
        orca_compat,      // -> RString  (orca semver range, e.g. ">=0.0.8, <0.1.0")
        manifest,         // -> RString  (JSON array of ToolDef)
        invoke,           // (name: RStr, args_json: RStr) -> RResult<RString, RString>
        backends,         // -> RString  (optional JSON array of BackendDef; "[]" if none)
    }
    .leak_into_prefix()
}
```

- `manifest()` delegates to `dispatch::tool_manifest_json()`, which walks the
  link-time `inventory` slice, filters to this plugin's namespace, and emits
  the tool schemas as JSON.
- `invoke()` parses the args, calls `dispatch(name, args, ctx)` to look up and
  run the tool. The plugin owns its own tokio runtime (a process-local
  `OnceLock`) to drive async tool bodies behind the synchronous ABI call.

### Registering tools (`tools.rs`)

Same `#[orca_tool]` macro the in-tree domain crates use:

```rust
use plugin_toolkit::prelude::*;

#[orca_tool(domain = "jellyfin", verb = "server_info")]
/// Return Jellyfin server identity + version.
pub async fn server_info(ctx: &ToolCtx) -> Result<ServerInfo, OrcaError> {
    // ...
}
```

The macro emits an `OrcaTool` impl and an `inventory::submit!` registration
named `jellyfin.server_info`. For standard CRUD surfaces, `endpoint_resource!`
generates the five `{list,detail,create,update,delete}` tools from one
declaration.

### Build-time client codegen (`build.rs`)

If the plugin wraps a documented HTTP or GraphQL API, generate a typed client
from the spec rather than hand-writing untyped JSON calls:

```rust
fn main() {
    plugin_toolkit_build::openapi::generate_all("specs", "jellyfin_client");
    // or: plugin_toolkit_build::graphql::generate("schema", "queries");
}
```

`plugin-toolkit-build` rewrites the generated code's crate paths to
`::plugin_toolkit::*`, so the plugin never depends on `progenitor` /
`graphql_client_codegen` directly.

## How the loader loads it

`plugin-loader` opens each cdylib through its **own** library header:

```rust
let header = lib_header_from_path(path)?;                  // opens this specific .so/.dylib
let module: PluginModRef = header.init_root_module::<PluginModRef>()?;
```

This is load-bearing (fixed in commit `6891499f`): `abi_stable`'s
`load_from_file` caches the resolved root module in a process-global cell keyed
by the root-module *type*. Every plugin shares the same `PluginModRef` type, so
the first load would win and every other plugin would alias the first. Loading
through each library's own `LibHeader` resolves the root module from *that*
cdylib's cell, letting N plugins coexist. The `abi_stable` layout + version
check plus the `orca_compat` semver range form the compatibility gate —
incompatible plugins are refused cleanly, never UB.

---

# Part 2 — Manifest plugins (`orca-plugin.toml`)

For non-Rust or out-of-process integrations. A manifest registers an external
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

# ── Vendored specs (for spec-first connectors) ──────────────────────────────
# [plugin.specs]
# dir = "specs/"

# ── Compose sub-plugins ─────────────────────────────────────────────────────
# [[uses]]
# path = "../shared-plugin/orca-plugin.toml"
# id   = "shared@my-workspace"

[plugin.agents]
manifest_dir = "agents/"
```

> Transport (`command`/`args`/`url`) lives in the manifest on disk, not in DB
> columns — the host re-reads it at dial time. The legacy `mode` and
> `mcp_transport` DB columns were dropped; do not reintroduce them.

### Registration

```bash
orca plugin add ~/code/my-plugin/orca-plugin.toml
orca plugin list
orca plugin remove my-plugin
orca plugin enable my-plugin
orca plugin disable my-plugin
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

const transport = new StdioServerTransport();
await server.connect(transport);
```

---

# Persistent data (`plugin_data`)

Both plugin types can store encrypted per-plugin key/value data. Values are
strings — use JSON for structure.

```bash
orca plugin data-set my-plugin my-key "hello world"
orca plugin data-get my-plugin my-key
orca plugin data-list my-plugin
orca plugin data-delete my-plugin my-key
```

REST surface:

```
GET    /api/plugins/{id}/data           → list all entries
GET    /api/plugins/{id}/data/{key}     → get one
PUT    /api/plugins/{id}/data/{key}     → set { "value": "..." }
DELETE /api/plugins/{id}/data/{key}     → delete
```

Native plugins read/write the same store through `plugin-toolkit`; manifest
plugins call the REST surface above.

---

# Agents

Plugins may ship agent definitions — markdown files with YAML frontmatter — in
the `agents/` directory declared by `manifest_dir`. Agents are compiled into
the binary, so rebuild after adding them:

```bash
cd ~/code/argyle-labs/orca && make install-dev
```

---

# Checklist

**Native cdylib plugin:**
- [ ] `crate-type = ["cdylib", "rlib"]`; single `plugin-toolkit` orca dep + direct `abi_stable`
- [ ] `#[export_root_module]` returning `PluginModRef` with all ABI fns wired
- [ ] `manifest()` → `dispatch::tool_manifest_json()`; `invoke()` → `dispatch(...)`
- [ ] tools declared with `#[orca_tool]` / `endpoint_resource!`
- [ ] `orca_compat` semver range set to the orca releases you target
- [ ] (spec-first) `build.rs` codegen via `plugin-toolkit-build`

**Manifest plugin:**
- [ ] `orca-plugin.toml` with `id`, `version`, `tier`, `[plugin.mcp]`
- [ ] MCP server with ≥1 tool; absolute paths in `args`
- [ ] `orca plugin add ~/path/to/orca-plugin.toml`; verify with `orca plugin list`
