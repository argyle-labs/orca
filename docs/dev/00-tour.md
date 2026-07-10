# Codebase Tour

Orca is a Rust binary that wears four hats simultaneously: CLI, TUI, HTTP web server, and MCP (Model Context Protocol) server. One binary, one `cargo build --release`, four modes of operation. This document orients you in the codebase before you write a single line.

---

## What Orca Does

Orca is an AI orchestrator. It:

- Runs **interactive agent sessions** (model backends are supplied by plugins, not core)
- Serves a **web dashboard** for viewing docs, logs, health checks, and API specs
- Exposes **MCP tools** that Claude Code uses — things like `get_config`, `run_agent`, `list_mcp_servers`, `search_docs`
- Manages **agent definitions** — named Markdown files (e.g., `wolf.md`, `bear.md`) embedded in the binary that give each AI persona a distinct system prompt

When you type `orca` with no arguments, you get the TUI chat session. When you type `orca serve`, you get the web server. When Claude Code talks to the `orca-local` MCP server, it is talking to `orca mcp-serve` running as a subprocess.

---

## The Four-Role Architecture

### 1. CLI
Every command is a `clap` subcommand defined in `projects/server/src/main.rs`. The `Command` enum has one variant per subcommand:

```rust
// projects/server/src/main.rs
#[derive(Subcommand)]
enum Command {
    Serve { dev: bool, port: u16 },
    McpServe,
    Daemon { port: Option<u16> },
    Dev { port: Option<u16> },
    Pod { action: PodAction },
    Run { agent: String, prompt: String },
    // ... plus Escalate, Audit, Log, Hook, Admin, Openapi
    #[command(external_subcommand)]
    Op(Vec<String>),   // dynamic `orca <noun> <verb>` → #[orca_tool] CLI
}
```

The hard-coded variants are the lifecycle/built-in commands. Everything else —
`orca docker list`, `orca model list`, `orca plugin add`, … — is routed through
the `Op` external subcommand to the macro-generated tool CLI, so there is no
per-command handler to hand-write.

### 2. TUI (Split-Pane Chat)
When you run `orca` with no subcommand, `main.rs` builds a `Session` and calls either `session.run_tui()` (default) or `session.run()` (classic readline mode with `--classic`). The `Session` lives in `projects/server/src/session.rs` and manages conversation history, tool dispatch, and output routing.

### 3. Web Server (axum)
`orca serve` calls `serve::run()` in `projects/server/src/serve/mod.rs`. It builds an axum `Router`, binds a TCP listener, and serves:
- `/api/*` — JSON REST endpoints for all registered features
- `/scalar*` — API reference viewer
- `/*` — proxied to the web UI, served by the out-of-process `peacock`
  plugin (repo [argyle-labs/peacock](https://github.com/argyle-labs/peacock)),
  which owns the root route `/`. If no web plugin is registered the build is
  simply headless.

### 4. MCP Server (JSON-RPC over stdio)
`orca mcp-serve` calls `mcp::serve()` in `projects/server/src/mcp/mod.rs`. It reads JSON-RPC lines from stdin, dispatches to handlers, and writes responses to stdout. Claude Code communicates over this pipe. The server also acts as a federation hub — it proxies tool calls to other registered MCP servers.

---

## Cargo Workspace

The workspace root is `Cargo.toml`; `[workspace.members]` is the authoritative
list. All member crates are flat-named (no `orca-` prefix) under `projects/`.
The full roster and per-crate responsibilities live in
[`CRATE_RESPONSIBILITIES.md`](../../CRATE_RESPONSIBILITIES.md). The crates you
touch most often:

| Crate | Path | Purpose |
|---|---|---|
| `server` (binary `orca`) | `projects/server/` | CLI entry, HTTP/HTTPS + MCP-stdio server, `ToolCtx` wiring, daemon supervisor |
| `derive` / `dispatch` | `projects/derive`, `projects/dispatch` | `#[orca_tool]` proc-macro + the runtime that routes tool calls to all surfaces |
| `contract` | `projects/contract/` | Stable tool/metadata types (`ToolCtx`, `OrcaTool`, `OrcaError`) |
| `db` | `projects/db/` | Encrypted SQLite: config rows, migrations, registries, `orca-plugin.toml` parser |
| `model` | `projects/model/` | Model registry + provider backends — Claude / Ollama / LM Studio (`model.*`) (core) |
| _(agents)_ | `~/.claude/agents/` | Agent prompts are not a workspace crate — `orca install` materializes every registered agent (from core and loaded plugins) into `~/.claude/agents/` at runtime as `.md` files (YAML frontmatter + prompt body) |
| `conversation` | `projects/conversation/` | REPL/TUI session state + background agent jobs |
| `utils` | `projects/utils/` | Shared helpers: config, hashing, path, http, pki, jsonrpc |

Tools live in their domain crate, not in `server`; the `server` crate is the
top of the dependency tree and the only one with a `main.rs`. A single
`#[orca_tool]` declaration is emitted to CLI, REST, and MCP
automatically — there is no hand-written dispatch match arm to maintain.

---

## How to Run in Dev Mode

```bash
# From the workspace root
make dev
```

`make dev` runs `orca dev` which:
1. Parks any running daemon (sends `SIGUSR1` to release the port)
2. Starts the Rust server in dev mode on port `12000`
3. peacock runs its own Vite dev server on port `12001` and declares it to orca
   as the web provider's `dev_upstream`
4. orca proxies non-API (`/`) requests from `:12000` → the peacock Vite upstream
   for hot reload
5. On exit, reclaims the port back to the daemon

For the backend only (no peacock web UI / hot reload):
```bash
cargo run -- serve --dev
```

For the MCP server (simulating Claude Code connecting):
```bash
cargo run -- mcp-serve
```

---

## Where to Look for What

| What you want to change | Where to look |
|---|---|
| Add a tool (CLI + REST + MCP at once) | Add an `#[orca_tool]` fn in the owning domain crate (see `CRATE_RESPONSIBILITIES.md`). No per-surface wiring. |
| Add a built-in CLI subcommand (non-tool) | `projects/server/src/main.rs` (add a variant to the `Command` enum) |
| Wire a service into the shared context | `build_tool_ctx` in `projects/server/` |
| Add a new agent | Register it in its owning crate or plugin's agent registry, then re-run `orca install` (re-materializes `~/.claude/agents/` from all registered agents) |
| Add a doc page | `docs/` (any `.md` file is auto-embedded) |
| Change model backend logic | `projects/model/` |
| Change config fields | `projects/utils/src/config.rs` |
| Change DB schema | add a migration under `projects/db/migrations/` (`make migration <slug>`) |

---

## Key Files at a Glance

```
projects/server/src/
  main.rs               ← CLI entry, Command enum, #[tokio::main]
  mcp/
    mod.rs              ← MCP stdio server, JSON-RPC handling
    tools.rs            ← tool-surface plumbing
  serve/
    mod.rs              ← axum router builder, run(), run_daemon(), proxy of / to peacock
    openapi.rs          ← OpenAPI emission
    auth_routes.rs      ← auth endpoints
    middleware.rs       ← request middleware

projects/model/src/      ← model registry + backends (Claude / Ollama / LM Studio)
~/.claude/agents/         ← wolf.md, bear.md, otter.md, ... materialized by `orca install`
                            from every registered agent (core + loaded plugins)
projects/files/src/
  embedded.rs           ← OrcaDocs: list()/read()/tree()/search() over embedded docs

docs/                   ← this tree (developer + reference docs, embedded at build)
```

---

## The Binary is Self-Contained

Documentation is compiled into the binary at build time; agent prompts are
materialized to the filesystem at runtime. The web UI is served separately by
the peacock plugin (see below):

1. **Agent prompts** — agents are registered in code (core and loaded
   plugins). `orca install` materializes every registered agent into
   `~/.claude/agents/` at runtime as file-based `.md` agents; they are not
   baked into the binary as a crate.

2. **Documentation** (`projects/files`, `struct OrcaDocs` in
   `src/embedded.rs`) — `rust-embed` bakes every `.md` under `docs/` into the
   binary as byte slices, so `orca mcp-serve` serves docs without touching the
   filesystem.

3. **Web UI** — served by the out-of-process `peacock` plugin (SvelteKit
   project at `peacock/ui/`), which owns the root route `/`. orca core proxies
   unmatched `/` requests to peacock's `peacock.render` tool in prod, or to
   peacock's Vite dev server in dev. It is **not** embedded in the orca binary.

This design means the `orca` binary itself carries docs, agent prompts, and the
MCP server; the web UI ships as the peacock plugin.
