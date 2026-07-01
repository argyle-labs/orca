# Crate Responsibilities

The single source of truth for what each crate owns. **Before adding code, find
the crate that already owns the domain.** Before adding a new crate, justify
here why none of the existing crates can absorb the responsibility.

The authoritative member list is `Cargo.toml [workspace.members]`. Every crate
lives under `projects/` with a flat package name (no `orca-` prefix).

## Principles

1. **Domain-driven, not too many crates.** Each crate is a coherent capability, not a per-feature dumping ground.
2. **One owner per responsibility.** If two crates implement the same primitive (hashing, http client, path resolution), one is wrong.
3. **Dependencies point down.** `surface → platform → core`. No upward edges, no sibling-to-sibling cycles inside a layer.
4. **Thin crates are fine when they're a clean public seam.** Never split for the sake of splitting.
5. **Two kinds of "namespace."** *Application* namespacing = the Rust module / crate hierarchy itself; that's free. *Resource* namespacing = the `namespace` crate, which groups user resources into named buckets (`home` / `work` / `personal`). Don't conflate them.

---

## Layers

```
SURFACE        server (binary "orca") · app-kit · dev
               ──────────────────────────────────────────
PLUGIN SDK     plugin-abi · plugin-loader · plugin-toolkit · plugin-toolkit-build
PLUGINS        plugins/{agents,docker,mcp,smb}  (in-tree, compiled in)
               ──────────────────────────────────────────
PLATFORM       dispatch · auth · files · system · pod · namespace ·
               conversation · notifications · storage · containers ·
               database · graphql · openapi · spec · runtime(pkg "plugins") ·
               orca-inventory
               ──────────────────────────────────────────
CORE           utils · db · contract · derive · macro-runtime
```

A crate **MAY** depend on anything strictly below it. It **MUST NOT** depend on
anything at or above. Sibling-to-sibling deps inside a layer are allowed when
acyclic.

---

## Core layer

### `utils`
Pure cross-cutting utilities with no business logic: framing, git, content hashing, http helpers, JSON-schema, jsonrpc, mesh status, path helpers (`expand_tilde`), PKI, search, shutdown, state, time. **No business tools.** Every other crate may import this.

### `db`
Encrypted SQLite (SQLCipher) layer: connection + schema bootstrap + migrations + typed CRUD over the canonical schema, plus dynamic config rows and the plugin registries. **Every persistent table's CRUD lives here** — platform crates use `db::<table>::*`, never inline SQL, never a second connection pool. Also owns `db::plugin_manifest` (the `orca-plugin.toml` parser, shared by registration and dial-time consumers).

### `contract`
Cold types + metadata traits only: `ToolCtx`, `OrcaToolDef`, `OrcaTool`, `CallerIdentity`, `OrcaError`, etc. No tokio, no axum, no inventory. The stable seam the macro + dispatch protocol anchor to; cache-friendly leaf crate.

### `derive`
The `#[orca_tool]` (and related) proc-macros. Emits `OrcaTool` impls + `inventory::submit!` registration entries + erased wrappers only.

### `macro-runtime`
Consolidates the registration types and emission target paths the `derive` macros expand into, so generated code references a stable runtime crate rather than re-deriving paths.

---

## Platform layer

### `dispatch`
The `#[orca_tool]` runtime: inventory walk, in-process routing, MCP / REST / CLI dispatchers, role table, manifest JSON emission (`tool_manifest_json`). Drives the live tool surfaces.

### `auth`
Authentication domain: credentials, sessions/tokens, PKI (CA + cert mint/rotate), secrets. Backed by `db::{users, sessions, api_tokens, oauth}`.

### `files`
Generic filesystem primitives + tools (`fs.{list,read,tree,search,stat}`, roots, ignores), embedded vault, markdown helpers.

### `system`
Local host management: install/update lifecycle, daemon, scheduler, host identity/status, runtime snapshots, profile management, diagnostics. Large because the responsibility is large; split by boundary if it grows, not by file count.

### `pod`
Multi-host mesh: peer discovery (mDNS + manual), mTLS, pairing, pod/exec, roster + host-status replication, cert rotation. Owns `system.peer.*` / `system.pod.*` tools.

### `namespace`
Resource grouping — per-user shareable workspaces that bucket concrete resources (containers, VMs, fs favorites) under names like `home` / `homelab`. Owns the namespace registry, membership, sharing.

### `conversation`
Interactive REPL/TUI session state, message logs, and background agent-job management.

### `notifications`
Backend-agnostic event dispatcher routing notifications to multiple backends.

### `storage`
Generic storage adapter trait + registry across backend types (NFS, SMB, …). The seam first-party storage plugins (e.g. `smb`) plug into via `plugin_toolkit::storage`.

### `containers`
Runtime-agnostic container model + adapter trait (Docker / LXC / Podman). The seam container plugins (e.g. `docker`) implement.

### `database`
Multi-database schema introspection + type definitions for external databases.

### `graphql`
Generic stateless GraphQL client, composing over the shared HTTP transport.

### `openapi`
OpenAPI spec parser + navigable view, feeding the registry and typed clients.

### `spec`
Tool surface for managing OpenAPI/GraphQL specs as first-class registry objects.

### `runtime` (package **`plugins`**)
Plugin host: plugin registry, install/remove from `orca-plugin.toml` manifests, and the plugin runtime key-value store. **Note the directory/package mismatch:** `projects/runtime/` builds the crate named `plugins`.

### `orca-inventory`
Server-side inventory aggregator combining pod members and system nodes into one topology view.

---

## Plugin SDK layer

### `plugin-abi`
The ABI-stable contract (`PluginMod` / `PluginModRef`, `abi_stable` root module) for externally-compiled cdylib plugins.

### `plugin-loader`
Dynamic loader for cdylib plugins: opens each library via its own `LibHeader`, runs the layout + version + `orca_compat` compatibility gate, and exposes the plugin's tools.

### `plugin-toolkit`
The single dependency a native plugin author needs. A facade re-exporting the contract, dispatch, domain crates (`storage`, `containers`, `notify`), the `#[orca_tool]` macro, and runtime deps — gated by features (`tools`, `db`, `containers`, `notify`, `graphql`, `openapi`, `http`).

### `plugin-toolkit-build`
Build-script helper: `openapi::generate_all` / `graphql::generate` codegen typed clients from vendored specs and rewrite crate paths to `::plugin_toolkit::*`.

---

## In-tree plugins (`projects/plugins/`)

Compiled into the binary as library crates and dispatched through `#[orca_tool]`.

| Crate | Owns |
|---|---|
| `agents` | Embedded agent prompts + resolution (`agent.list`, `agent.get`) |
| `docker` | Docker/compose integration (`docker.{list,detail,create,update,delete}`) |
| `mcp` | MCP server registry + federation passthrough (`mcp.*`, `McpPool`) |
| `smb` | SMB/CIFS storage adapter (via `plugin_toolkit::storage`; no `#[orca_tool]`) |

---

## Surface layer

### `server` (binary `orca`)
The user-facing binary: axum HTTP/HTTPS + MCP-stdio + CLI entry point, `build_tool_ctx` wiring, daemon supervisor, dev-mode proxy, OpenAPI emission. **No business logic** — all tools live in their platform crates.

### `app-kit`
UniFFI embedding layer exposing `OrcaTool`s as native bindings for iOS / Android / Linux. Wraps the same `ToolCtx`, minus the network surface.

### `dev`
Developer-only tooling: cargo-watch supervisor, binary distribution helpers. Not part of the shipped runtime surface.

---

## Test plumbing

### `inventory-tests`
Test-only crate that links every domain crate so the `#[orca_tool]` inventory slice is fully populated. Cross-crate integration tests only. **No production code.**

---

## Hard rules (enforced on review)

1. **No crate may reimplement sha256, hex encoding, or path expansion.** Use `utils`.
2. **Tools live with their owner.** `namespace.*` in `namespace`; `fs.*` in `files`; `system.peer.*`/`system.pod.*` in `pod`.
3. **`db` owns every persistent table.** Platform crates use `db::<table>::*` — never inline SQL, never a second connection pool.
4. **`server` never holds business logic.** A tool body doing real work inside `projects/server/` is misplaced.
5. **`utils` may be imported by anything**, and must stay dependency-free of tokio runtime / axum / DB itself.
6. **A native plugin's only orca dependency is `plugin-toolkit`** (`feedback-plugin-toolkit-only-no-exceptions`), plus a direct `abi_stable` for the export macro.

---

## When to add a new crate

Only when **all three** are true: the responsibility is genuinely new, it has
≥2 distinct consumers or a clear external publish target, and its public API
fits in one paragraph. Otherwise add a `mod` inside the closest existing crate.
