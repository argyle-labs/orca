# Toolkit capabilities & exposed surface

A native plugin links **no** `reqwest`, `rustls`, `tokio`, `futures`, or
`chrono`. Everything heavy lives host-side and is reached over a **capability**;
the plugin only ever sees the orca-owned type at the seam. This page lists what
the toolkit exposes.

## Host capabilities

The daemon offers these to a plugin over the socket; the toolkit wraps each as a
plain Rust call. "Re-export is not abstraction" — the plugin builds an orca
`Request` / row / secret ref, never the crate underneath.

| Capability | Reached via | Purpose |
|---|---|---|
| `http.request` | `plugin_toolkit::client::Client` | buffered HTTP |
| `http.stream` | `Client::stream` / `Client::events` | byte / SSE streams |
| `db.op` | `plugin_toolkit::runtime` (via `endpoint_resource!`) and `core_tables` | namespaced + core tables |
| `secret.op` | resolved host-side; a plugin *provides* a backend (see [backends](backends.md)) | secret resolution |
| `agents.register` | `plugin_toolkit::agents::register` | contribute agents/hooks/skills |

## Exposed toolkit tools & macros

The gateway is one import: `use plugin_toolkit::prelude::*;`. What it gives you:

| Item | Kind | What it does |
|---|---|---|
| `#[orca_tool]` | attr macro | register an async tool ([registering tools](registering-tools.md)) |
| `#[endpoint_tool]` | attr macro | sugar: args-struct + `#[orca_tool]` wrapper |
| `#[endpoint_resource]` | attr macro | CRUD resource + table ([CRUD](endpoint-resource-crud.md)) |
| `#[orca_struct]` / `#[orca_struct(args)]` | attr macro | serde + schemars + `clap` on a type/arg-struct |
| `#[orca_error]` | attr macro | error enum with `#[orca(display=…, from)]` |
| `#[orca_async]` | attr macro | implement a core async trait with plain `async fn` |
| `ToolCtx`, `JsonAny` | types | the tool context + typed-JSON value |
| `Result`, `anyhow`, `bail`, `Context` | re-exports | the plugin error surface (`anyhow`) |
| `json!` | macro | ad-hoc JSON (test fixtures; real payloads are typed) |
| `Route`, `Routes`, `resolve_reachable` | types/fn | per-instance base-URL fallback |
| `lifecycle::{run, stdout_string, timestamp}` | fns | exec / backup-stamp boilerplate |
| `sha256_hex`, `tracing` | fns/macros | hashing, structured logging |
| `http`, `graphql`, `openapi`, `ApiClientBuilder` | modules | transport primitives |

Authority: [`../../projects/plugin-toolkit/src/prelude.rs`](../../projects/plugin-toolkit/src/prelude.rs)
(the only import a plugin source file writes — if something isn't in the prelude,
treat it as nonexistent from the plugin's perspective).

### The serve-loop macros (from the crate root, not the prelude)

`serve_tool_plugin!`, `serve_service_plugin!`, `serve_storage_plugin!`,
`serve_backup_kind_plugin!`, `serve_backup_target_plugin!` —
[`../../projects/plugin-toolkit/src/serve_macros.rs`](../../projects/plugin-toolkit/src/serve_macros.rs).
See [Native plugin](native-plugin.md).

## HTTP (`client`)

```rust
use plugin_toolkit::client::{Client, Request};

let http = Client::new();
let resp = http.get("https://host/System/Info")?;          // buffered GET
let resp = http.post_json("https://host/Items", &body)?;   // buffered POST
let resp = http.send(Request::new("PUT", url).json(&body)?)?;
if resp.is_success() { let v: MyType = resp.json()?; }

let mut bytes  = http.stream(Request::new("GET", url))?;   // ByteStream (no host buffer)
let mut events = http.events(Request::new("GET", sse_url))?; // EventStream (SSE)
```

`ByteStream` / `EventStream` own their own `next()` — the orca equivalent of
draining reqwest's `bytes_stream()`, so a plugin never names `futures`'s
`StreamExt`. Source:
[`../../projects/plugin-toolkit/src/client.rs`](../../projects/plugin-toolkit/src/client.rs).

## Long-lived child process (`process`)

Drive a line-oriented peer (e.g. a JSON-RPC subprocess) through
`plugin_toolkit::process::Command` — the plugin-facing surface is
`request` (correlated round-trip) / `notify` (fire-and-forget) / `kill` (also
killed on drop). Never name the runtime's process API.

## Core tables (`core_tables`)

A plugin's own data lives in **namespaced** tables. A fixed set of orca-owned
core tables (`mcp_servers`, `mcp_tool_mappings`, `openapi_specs`, `plugins`,
`plugin_credentials`) is reached through `plugin_toolkit::core_tables::*`:

```rust
use plugin_toolkit::core_tables::{mcp_servers, plugins};
let servers = mcp_servers::list()?;   // enabled, sorted by name
mcp_servers::upsert(&server)?;
```

Same `db.op` capability sink as namespaced data, using the empty-namespace
convention (`""` + a literal core table name). The `DbOp` surface carries only
`List`/`Get`/`Upsert`/`Delete`; accessors filter and sort in Rust.

## Build-time client codegen (`build.rs`)

If the plugin wraps a documented HTTP / GraphQL API, generate a typed client from
its spec instead of hand-writing untyped calls:

```rust
fn main() {
    plugin_toolkit_build::openapi::generate_all("specs", "jellyfin_client");
    // or: plugin_toolkit_build::graphql::generate("schema", "queries");
}
```

`plugin-toolkit-build` rewrites generated crate paths to `::plugin_toolkit::*`,
so the plugin never depends on `progenitor` / `graphql_client_codegen`, and the
generated client issues requests through the `http.request` capability.
