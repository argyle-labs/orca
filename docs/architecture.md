# Architecture

Orca is a single Rust binary that runs on every host in a pod and
exposes one tool surface (CLI / REST / MCP / WASM client) via the
`#[orca_tool]` macro. Every host runs the same daemon; lifecycle,
storage, services, and observability are all orca verbs.

For sequencing of what's shipped vs. next, see
[`ROADMAP.md`](ROADMAP.md).

## The four-surface model

A single `#[orca_tool]` declaration in a domain crate (e.g.
`projects/system`, `projects/plugins/proxmox`) emits to all four
surfaces automatically:

| Surface | Entry point |
|---|---|
| CLI | clap subcommand under `orca <noun> <verb>` |
| REST | `/api/v1/<tool>` on `:12000` (HTTP) and `:12443` (HTTPS) |
| MCP | JSON-RPC 2.0 over stdio for Claude Code / agentic clients |
| WASM | Browser SDK consumed by the web UI (the `peacock` plugin) |

No hand-written `#[utoipa::path]`; the macro is the sole emitter
(`feedback_all_endpoints_in_openapi.md`). No transport-specific
domain logic in `projects/server` — the server is thin
(`feedback_server_is_thin.md`).

## Workspace layout

```
projects/
  app-kit/         UniFFI embedding layer (iOS / Android / Linux bindings)
  auth/            credentials, sessions/tokens, PKI (CA + cert mint/rotate)
  contract/        stable contract types + metadata traits (cache-friendly leaf)
  conversation/    REPL/TUI session state + background agent jobs
  containers/      runtime-agnostic container model + adapter trait
  database/        external-database schema introspection
  db/              encrypted SQLite: config rows, migrations, registries, manifest parser
  derive/          #[orca_tool] proc-macro
  dev/             developer-only tooling (cargo-watch supervisor, dist helpers)
  dispatch/        runtime side of the derive/dispatch pair (routing + manifest emit)
  files/           generic fs primitives (list/read/search/tree/stat) + vault
  graphql/         generic stateless GraphQL client
  inventory-tests/ test-only crate linking every domain (inventory population)
  macro-runtime/   registration types/paths the derive macros expand into
  namespace/       resource grouping — shareable per-user workspaces
  notifications/   backend-agnostic event/notification dispatcher
  openapi/         OpenAPI parser + navigable view
  orca-inventory/  topology aggregator (pod members + system nodes)
  plugin-abi/      ABI-stable cdylib plugin contract (PluginMod / abi_stable)
  plugin-loader/   dynamic cdylib loader (per-LibHeader, version-gated)
  plugin-toolkit/  the single dependency a native plugin author needs
  plugin-toolkit-build/  build.rs codegen for typed OpenAPI/GraphQL clients
  pod/             mesh: mTLS, mDNS discovery, pairing, dispatch, cert rotation
  runtime/         plugin host (package name `plugins`): registry + KV + install
  server/          thin HTTP+MCP transport layer (binary `orca`)
  spec/            OpenAPI/GraphQL spec registry tools
  storage/         generic storage adapter trait + registry
  system/          install/update/scheduler/daemon/host/topology — lifecycle core
  utils/           shared helpers (config, hashing, path, http, pki, jsonrpc)
```

The SvelteKit web UI is **not** a workspace crate and no longer lives in this
repo. It is an out-of-process plugin, **peacock** (repo
[argyle-labs/peacock](https://github.com/argyle-labs/peacock)), whose SvelteKit
project lives at `peacock/ui/`. peacock registers `contract::web` and owns the
root route `/`; orca core serves the UI by proxying unmatched `/` requests to
peacock's `peacock.render` tool in prod, or to peacock's Vite dev server (the
web provider's declared `dev_upstream`) in dev. The `ui.enabled` DB setting
gates the `/` owner at runtime. See [`OUT-OF-PROCESS-PLUGINS.md`](OUT-OF-PROCESS-PLUGINS.md).

System lifecycle lives in `projects/system/`. The major modules
(`install.rs`, `update.rs`, `scheduler.rs`, `daemon.rs`, `host.rs`,
`host_status.rs`, `system_info*`, `topology/`) are the surface the
ROADMAP Phase 1 work extends.

Plugins come in two forms. **Native cdylib plugins** (e.g. the
first-party `jellyfin` / `plex` repos) are built separately as `cdylib`s and
loaded in-process at runtime by `plugin-loader` via `abi_stable`, depending
only on `plugin-toolkit`. A second path — `orca-plugin.toml` manifest plugins —
registers external MCP servers. See
[`plugin-authoring.md`](plugin-authoring.md).

## Ports

| Port | Bind | Purpose |
|---|---|---|
| 12000 | HTTP | REST + MCP-over-HTTP, browser UI |
| 12443 | HTTPS | Same as 12000 with TLS |
| 12002 | mTLS | Pod mesh (peer-to-peer dispatch + replication) |

All three are per-host configurable via `~/.orca/orca.toml [ports]`
or env (`ORCA_HTTP_PORT` / `ORCA_HTTPS_PORT` / `ORCA_MESH_PORT`).
Always read via `http_port()` / `https_port()` / `mesh_port()`
helpers — never the consts at runtime
(`project_serve_scheme_http_https.md`).

## Identity, trust, and pairing

Each host has a stable `peer_id` anchored to `/etc/machine-id` (or
a fixed path on systems where `$HOME` churns —
`project_peer_identity_churn.md`). Pairing = mutual mTLS trust;
no asserted-role fallbacks. Self-secure = Tier-2 cred sync opt-in.

Cross-host dispatch is opt-out via `local_only` flag
(`project_universal_peer_dispatch.md`). `--peer <name>` on CLI and
`X-Orca-Peer` header on REST route the call through pod mesh.

Secrets never cross to non-secure hosts; sensitive operations
delegate back to a holder via callback
(`project_secret_delegation_not_distribution.md`).

## Config + state storage

Two tiers (`project_storage_tiers.md`):

- `~/.orca/` — files (mesh-replicated where opt-in)
- `~/.orca/orca.db` — encrypted SQLite, key at `~/.orca/.db_key`

`orca.toml` is build/runtime app config; `orca.db` is dynamic
state (config rows, secrets, install state, scheduler runs).
Database stays small — logs/metrics/history land on disk with
retention, not as rows (`project_db_size_and_retention.md`).

## Where state lives

Canonical map. State of a given kind has exactly one owner.

| State | Lives in | Owner |
|---|---|---|
| Desired state | config repo `main` branch | operator |
| Realized state | config repo `state/` branch | orca daemon |
| Runtime state (peers, tasks, scheduler runs, lifecycle events) | SQLite DB on each peer (`~/.orca/orca.db`) | orca daemon |
| Non-secret envs | DB (config store) | orca daemon |
| Secrets | secret backend (orca-native / 1Password / bw / vaultwarden) | backend |
| Mesh-replicated state (peer roster, trust info) | mesh CRDT | orca daemon (replicated) |
| Escrow state (DR keys, founding-peer CA) | secret backend, k-of-n distributed | gated subset of peers |

Boundaries:

- DB syncs to the config-repo `state/` branch on commit; reconciler
  only **reads** from the `main` branch, **writes** only to the DB
  and target hosts.
- Secrets never sit in the config repo — only handles do. Resolved
  values live in memory on a holder node at projection time and are
  zeroed after the write.
- The mesh CRDT is the only state replicated by gossip; everything
  else is per-peer-local or backend-owned.
- Escrow is the **only** state where a key is intentionally split
  across peers without a single owner — k-of-n unlock by design.

## See also

- [`single-binary.md`](single-binary.md) — why one binary, build sequence
- [`repo-structure.md`](repo-structure.md) — directory-level map
- [`ROADMAP.md`](ROADMAP.md) "Cross-cutting standing rules" — dev + deploy + hard rules
- [`install-runbook.md`](install-runbook.md) — fresh-host bootstrap
- [`plugin-authoring.md`](plugin-authoring.md) — plugin contract
- [`ROADMAP.md`](ROADMAP.md) — phasing
