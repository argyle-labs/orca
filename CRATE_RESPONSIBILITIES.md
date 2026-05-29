# Crate Responsibilities

The single source of truth for what each crate owns. **Before adding code, find the crate that already owns the domain.** Before adding a new crate, justify here why none of the existing crates can absorb the responsibility.

## Principles

1. **Domain-driven, not too many crates.** Each crate is a coherent capability, not a per-feature dumping ground.
2. **One owner per responsibility.** If two crates implement the same primitive (hashing, http client, path resolution), one is wrong.
3. **Dependencies point down.** `surface → platform → core`. No upward edges, no sibling-to-sibling cycles inside a layer.
4. **Thin crates are fine when they're a clean public seam.** Never split for the sake of splitting.
5. **Two kinds of "namespace."** *Application* namespacing = the Rust module / crate hierarchy itself; that's free. *Resource* namespacing = the `namespace` crate, which groups user resources (containers, VMs, LXCs, fs favorites) into named buckets like `home` / `rebuy` / `homelab`. Don't conflate them.

---

## Layers

```
SURFACE        server (binary "orca"), app-kit, sdk
               ──────────────────────────────────────────
PLATFORM       dispatch · fs · system · pod · auth · llm · mcp ·
               agents · plugins · namespace · conversation · specs ·
               schema · scanner · replicate · integrations/*
               ──────────────────────────────────────────
CORE           utils · db · orca-contract · orca-macro
```

A crate **MAY** depend on anything strictly below it. It **MUST NOT** depend on anything at or above. Sibling-to-sibling deps inside a layer are allowed when acyclic; the dep direction inside PLATFORM is documented per crate below.

---

## Core layer

### `orca-utils` (`orca_utils`)j
Pure utilities with no business logic. Config struct, path helpers (`expand_tilde`), content hashing (`fs::hash::{sha256, sha256_bytes, blake3_file}`), file I/O helpers, time helpers, env loading. **No DB. No network. No tools.** Every other crate may import this.

### `orca-db` (`db`)
SQLite connection + schema bootstrap + typed CRUD over the canonical schema. Owns `apply_schema`, migrations, SQLCipher key handling. **Every persistent table's CRUD lives here** — platform crates use `db::<table>::*`, never inline SQL, never a second connection pool.

### `orca-contract`
Cold types only: `ToolCtx`, `OrcaToolDef`, `OrcaTool`, `RemoteExec`, `CallerIdentity`, `OrcaError`, `JsonAny`. No tokio, no axum, no inventory. Anchors the macro + dispatch protocol.

### `orca-macro`
The `#[orca_tool]` and `#[derive(Replicated)]` proc macros. Generates registration + erased wrappers only.

---

## Platform layer

PLATFORM crates form a DAG. Below, each entry's "→" lists the platform siblings it depends on; everything also depends freely on CORE.

### `dispatch` (formerly `orca-dispatch`)
The `#[orca_tool]` runtime: inventory walk, MCP / REST / CLI dispatchers, role table, remote-ok allowlist. Drives all three live tool surfaces. No platform sibling deps.

### `fs`
**Generic filesystem + storage.** `fs.{list,read,tree,search,stat}`, `fs.roots.{list,create,delete}`, `fs.ignore.{list,create,delete}`, plus the **storage manager** (mount/unmount/discover for NFS + SMB + later SSHFS/S3 — see `integrations/nfs`, `integrations/smb`, candidates to fold into `fs/storage/`). Embedded vault, tree compaction, markdown helpers. Consumes `db::docs::*` for root/ignore registry.
→ `integrations/{nfs,smb}` until those are absorbed.

### `system`
**Local host management.** Daemon lifecycle, install/update/package, host identity, host status, host addressing, system info, periodic jobs, scheduler, sysadmin (users/sudo), diagnostics. Includes `host`, `host_identity`, `host_status` (formerly in `fleet`). Large because the responsibility is large; if it grows further, split by boundary (e.g. `system-runtime` vs `system-install`), not by file count.

### `pod`
**Multi-host mesh.** Peer discovery (mDNS + manual), mTLS, pairing, pod/exec, roster sync, replication sync, cert rotation. Owns `system.peer.*`, `system.pod.*` tools.
→ `replicate`, `system` (for host identity/status).

### `replicate` (`orca-replicate`)
Generic mesh replication engine. `#[derive(Replicated)]` registers a table for LWW sync; the engine is transport-agnostic.

### `auth`
User/session/token primitives. `auth.token.{create,list,delete}`, loopback token, GitHub/Atlassian OAuth. Backed by `db::{users, sessions, api_tokens, oauth}`.

### `llm`
Provider abstraction (Anthropic / LMStudio / Ollama / generic `Provider` trait). Model catalog. No tool defs that aren't strictly provider-related.

### `mcp`
MCP client pool — federation to external MCP servers — and protocol helpers. **Not** the MCP-stdio server entry point (that's in `server`).

### `agents`
Agent definitions, embedded slash commands, embedded skills. Owns any tool that introspects bundled commands/skills (e.g. `agents.commands.list`).

### `plugins`
Plugin host: discovery, install, credential management, plugin↔host JSON-RPC. Backed by `db::{plugins, plugin_*}`.

### `namespace`
**Resource grouping.** Lets the user organize concrete resources — Docker containers, VMs, LXCs, fs favorites, etc. — into named buckets (e.g. `home`, `rebuy`, `homelab`). Owns the namespace registry, membership, and sharing primitives.
This is *not* the Rust-module sense of "namespace." Code organization is handled by the crate graph itself.

### `conversation`
Multi-turn chat state, message logs, session-event capture.
→ `agents`, `llm`, `mcp`.

### `specs`
OpenAPI spec management. CRUD over scanned specs at the namespace level.
→ `schema`, `scanner`, `namespace`.

### `schema`
Schema database loader for external sources (rebuy, etc.). Consumes scanned specs.

### `scanner`
OpenAPI scanning + spec generation. Walks routes, emits JSON specs to `~/.orca/specs/`.

### `integrations/*`
One crate per external system (`docker`, `dockge`, `homeassistant`, `nfs`, `ntfy`, `proxmox`, `smb`, `unraid`). Each owns its own HTTP/CLI client and tool defs. Leaves of the dep DAG. **Candidates to fold:** `nfs` + `smb` belong under `fs/storage/` per the storage-as-part-of-fs rule.

---

## Surface layer

### `server` (binary `orca`)
The user-facing binary. Owns:
- HTTPS + HTTP server (axum router, middleware, REST handlers).
- MCP-stdio server.
- CLI entry point.
- `build_tool_ctx` — wires every service into the shared `ToolCtx`.
- Daemon supervisor, dev-mode proxy, OpenAPI emission.

**No business logic.** All tools live in their platform crates; `server` only routes and wires.

### `app-kit` (`orca-app-kit`)
Embedded UI lifecycle + UniFFI bindings for mobile/desktop hosts. Wraps the same `ToolCtx` `server` uses, minus the network surface.

### `sdk`
Public SDK for plugin authors (Rust + bindings). Mirrors `orca-contract` types but for out-of-process consumers.

---

## Test plumbing

### `inventory-tests`
Sibling test crate that links every platform crate so the `#[orca_tool]` inventory slice is fully populated. Cross-crate integration tests only. **No production code.**

---

## Hard rules (enforced on review)

1. **No crate may reimplement sha256, hex encoding, or path expansion.** Use `orca_utils::fs::hash::{sha256, sha256_bytes, blake3_file}` and `orca_utils::path::expand_tilde`.
2. **Tools live with their owner.** A tool named `namespace.foo.bar` lives in the `namespace` crate; `fs.x` lives in `fs`; `system.peer.*` and `system.pod.*` live in `pod`.
3. **`db` owns every persistent table.** Platform crates use `db::<table>::*` helpers — never inline SQL, never a second connection pool.
4. **`server` never holds business logic.** A tool body doing real work inside `projects/server/` is misplaced.
5. **`orca-utils` may be imported by anything**, and must stay dependency-free itself (no tokio runtime, no axum, no DB).

---

## When to add a new crate

Only when **all three** are true:
- The responsibility is genuinely new (not splittable into an existing crate).
- It has at least two distinct consumers, OR a clear external publish target.
- Its public API can be described in one paragraph.

Otherwise add a `mod` inside the closest existing crate.

## When to merge crates

If a crate has <100 LOC of distinct logic AND only one consumer AND no plausible second consumer, merge it.

---

## Audit log

See `project_crate_audit_2026_05_29` in memory for the current punch list of violations and the order they're being fixed.
