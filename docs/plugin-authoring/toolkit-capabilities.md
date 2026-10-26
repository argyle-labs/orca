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

### Namespace confinement

A plugin's capability calls are **confined to its own namespace**. The daemon
binds every session to the plugin's authoritative id (the install-dir filename,
validated against the handshake) and scopes `db.op`/`secret.op` to it:

- `db.op` may only touch the plugin's `plug__<id>__*` tables — the `namespace`
  field must equal the plugin's id. (Core tables use the empty-namespace
  convention below and are reached via `core_tables`.)
- `secret.op` may only touch secrets named `<id>` or `<id>.…` — build names with
  `plugin_toolkit::secrets::scoped_name` so `provider` == your plugin id.

A plugin therefore cannot read or write another plugin's tables or secrets. (This
is currently *warn-first*: a cross-namespace access is logged and allowed for one
release, then rejected — keep your `provider`/`namespace` equal to your plugin id
to stay compatible.)

> **Operator boundary.** The `plugin.data.*` MCP verbs are a separate,
> **operator/admin-scoped** surface: an operator manages *any* plugin's stored
> data by passing its `id`. That path is intentionally *not* confined to a
> calling domain — it is gated by operator auth, not by plugin identity. The
> confinement above applies only to the plugin-initiated capability channel.

### Calling core tools (`tool.call`)

A plugin calls another tool through `tool.call` **as the user who invoked it** —
never as itself. The target must be marked `tool_callable`, declared in the
plugin's `tool_calls`, and allowed for that user's role. A parent invocation that
was not execute-confirmed forces the child into `dryRun`, so a tool without
dry-run support refuses.

Every successful result is passed through `utils::scrub::redact_result` before it
reaches the plugin (errors through `scrub_line`). One vocabulary, `is_sensitive_key`,
decides which names are secret, and one marker, `***`, replaces what it hides:

- Key rule: a string value under a sensitive key (`token`, `secret`, `password`,
  `passwd`, `pwd`, `passphrase`, `apiKey`, `credential`, `privateKey`, `authorization`,
  `bearer`, or the whole key `key`, `value`, `auth`, `pin`, `otp`, `cookie`, `salt`,
  `seed`, or the word `pass`, `pw`, `cred`, `creds`, `jwt`, `hmac` between `_.-`
  separators) is replaced, including strings nested in objects and arrays under it.
  Numbers, booleans, null, and the shape of arrays and objects are kept; names such
  as `hasKey`, `claims`, `author`, and `bypass` are not secret.
- URI rule: a string value that is a URI carrying userinfo, a webhook/ntfy URL, a
  URL with a token-like path segment or a secret-named query parameter, or a path
  under `/run/secrets/` or `/var/run/secrets/` is replaced whole.
- Text rule: every other string goes through `redact_text`, which rewrites only the
  secret span of `"key": "value"` pairs, `NAME=value` (quoted values and spaces
  around `=` included), `--flag value`, `key: value`, `Authorization` / `X-Api-Key` /
  `PVEAPIToken` headers, URI userinfo (`scheme://user:pass@host` and bare
  `user:pass@host`), webhook URLs, and well-known token shapes (GitHub `ghp_` and
  `github_pat_`, `sk-`, AWS `AKIA`/`ASIA`, Slack `xox*`, JWT `eyJ...`). PEM private
  key blocks are replaced line by line across lines.

Plugins reuse the same rules through `plugin_toolkit::scrub` (`is_sensitive_key`,
`is_secret_value`, `is_secret`, `redact_text`, `scrub_line`).

A plugin that serves only domain backends (for example a security scanner) declares
its set with `Plugin::tool_calls(&["system.topology", ...])`; it applies to every
backend the plugin registers and is pinned at install like a tool's `tool_calls`.

The called tool sees the chain of plugins that led to it in `ctx.call_chain()`
(empty for a direct call), so it can return less to a plugin than to a direct
admin caller.

Tools a security scanner reaches this way:

| Tool | Role | Returns |
|---|---|---|
| `system.topology` | any | the fleet tree, including routes and host IPs; provider labels with secret names or values are redacted |
| `guest.read_packages` | admin | `/etc/os-release`, `/var/lib/dpkg/status`, `/lib/apk/db/installed` and `rpm -qa` (name and epoch:version-release) from one guest; a missing source is `null`, and `truncated` flags a source cut at the output cap |

`guest.exec` is not reachable through `tool.call`.

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

### The serve loop (the `Plugin` builder, from the crate root)

`fn main()` is a short chain on `plugin_toolkit::plugin::Plugin` —
`.tools(..)`, `.service(..)`, `.storage(..)`, `.replication(..)`, `.unit(..)`,
etc., terminated by `.serve()`:
[`../../projects/plugin-toolkit/src/plugin.rs`](../../projects/plugin-toolkit/src/plugin.rs).
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

## Host directory roots

`plugin_toolkit::host_paths` validates host directories a plugin manages (stack
roots, discovery roots, data roots) the same way in every plugin:

- `validate_root(kind, root, allowed)` accepts `root` only when it is absolute,
  has no `.`/`..`, is an existing directory whose canonical path is the path as
  given (no symlinked component), and sits strictly inside one of `allowed`.
  `allowed_parents(extra)` is `DEFAULT_ALLOWED_PARENTS` (`/opt`, `/srv`,
  `/mnt`, `/home`, `/data`, `/media`) plus the plugin's own parents.
- `valid_roots(kind, roots, allowed)` re-checks stored roots on read, returning
  the ones that pass and the errors of the ones dropped.
- `canonical_in_roots(dir, roots)` is the canonical `dir` when it is a
  directory strictly inside one of `roots`.
