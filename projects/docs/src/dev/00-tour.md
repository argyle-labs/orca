# Codebase Tour

Orca is a Rust binary that wears four hats simultaneously: CLI, TUI, HTTP web server, and MCP (Model Context Protocol) server. One binary, one `cargo build --release`, four modes of operation. This document orients you in the codebase before you write a single line.

---

## What Orca Does

Orca is an AI orchestrator. It:

- Runs **interactive chat sessions** with a local or cloud LLM (LM Studio locally, Claude via API as escalation)
- Serves a **web dashboard** for viewing docs, logs, health checks, and API specs
- Exposes **MCP tools** that Claude Code uses — things like `get_config`, `run_agent`, `list_mcp_servers`, `search_docs`
- Manages **agent definitions** — named Markdown files (e.g., `wolf.md`, `bear.md`) embedded in the binary that give each AI persona a distinct system prompt

When you type `orca` with no arguments, you get the TUI chat session. When you type `orca serve`, you get the web server. When Claude Code talks to the `orca-local` MCP server, it is talking to `orca mcp-serve` running as a subprocess.

---

## The Four-Role Architecture

### 1. CLI
Every command is a `clap` subcommand defined in `projects/server/src/main.rs`. The `Command` enum has one variant per subcommand:

```rust
// projects/server/src/main.rs:29
#[derive(Subcommand)]
enum Command {
    Login { service: LoginService },
    Serve { dev: bool, port: u16 },
    McpServe,
    Daemon { action: DaemonAction },
    // ... 20+ more
}
```

Most subcommand handlers are thin wrappers that call into `orca_commands` (`projects/commands/`). The few that need `Session` or `ProjectContext` live in `main.rs` directly because those require the server crate.

### 2. TUI (Split-Pane Chat)
When you run `orca` with no subcommand, `main.rs` builds a `Session` and calls either `session.run_tui()` (default) or `session.run()` (classic readline mode with `--classic`). The `Session` lives in `projects/server/src/session.rs` and manages conversation history, tool dispatch, and output routing.

### 3. Web Server (axum)
`orca serve` calls `serve::run()` in `projects/server/src/serve/mod.rs`. It builds an axum `Router`, binds a TCP listener, and serves:
- `/api/*` — JSON REST endpoints for all registered features
- `/scalar*` — API reference viewer
- `/*` — static frontend (SvelteKit build embedded in the binary via `rust-embed`)

### 4. MCP Server (JSON-RPC over stdio)
`orca mcp-serve` calls `mcp::serve()` in `projects/server/src/mcp/mod.rs`. It reads JSON-RPC lines from stdin, dispatches to handlers, and writes responses to stdout. Claude Code communicates over this pipe. The server also acts as a federation hub — it proxies tool calls to other registered MCP servers.

---

## Cargo Workspace

The workspace root is `/Users/scottkey/code/orca/Cargo.toml`. All member crates are under `projects/`:

```toml
# Cargo.toml:1
[workspace]
members = [
    "projects/agents",
    "projects/commands",
    "projects/core",
    "projects/docs",
    "projects/jobs",
    "projects/scanner",
    "projects/server",
    "projects/utils",
]
```

### What each crate does

| Crate | Path | Purpose |
|---|---|---|
| `orca` (binary) | `projects/server/` | The final binary: CLI entry point, HTTP server, MCP server, session logic |
| `orca_core` | `projects/core/` | Model backend abstraction — `ModelBackend` trait, `ClaudeBackend`, `LMStudioBackend` |
| `orca_agents` | `projects/agents/` | Agent prompt registry — embeds `.md` files at compile time via `build.rs` |
| `orca_commands` | `projects/commands/` | CLI subcommand handlers that don't need the full server context |
| `orca_docs` | `projects/docs/` | Embeds this doc tree into the binary via `rust-embed`; provides `list()`, `read()`, `search()` |
| `orca_jobs` | `projects/jobs/` | Background job infrastructure |
| `orca_scanner` | `projects/scanner/` | PII detection logic for the hook scanner |
| `orca_utils` | `projects/utils/` | Shared types, config, database access, auth helpers |

The key dependency direction is: `server` → `core`, `agents`, `commands`, `docs`, `jobs`, `scanner`, `utils`. The `server` crate is the top of the dependency tree and the only one with a `main.rs`.

---

## How to Run in Dev Mode

```bash
# From the workspace root
make dev
```

`make dev` runs `orca dev` which:
1. Parks any running daemon (sends `SIGUSR1` to release the port)
2. Spawns the Vite dev server (`npm run dev` in `projects/frontend/`) on port `12001`
3. Starts the Rust server in dev mode on port `12000`
4. Proxies non-API requests from `:12000` → `:12001` (Vite) for hot reload
5. On exit, reclaims the port back to the daemon

For the backend only (no frontend hot reload):
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
| Add a CLI subcommand | `projects/server/src/main.rs` (add variant to `Command` enum) + `projects/commands/src/` (handler) |
| Add an HTTP API endpoint | `projects/server/src/serve/api/` (handler file) + `projects/server/src/serve/api/mod.rs` (router wiring) |
| Add an MCP tool | `projects/server/src/mcp/mod.rs` (`dispatch` match arm) + `projects/server/src/mcp/handlers.rs` (logic) |
| Add a new agent | `projects/agents/src/agents/` (new `.md` file with YAML frontmatter) |
| Add a doc page | `projects/docs/src/` (this directory — any `.md` file is auto-embedded) |
| Change model backend logic | `projects/core/src/backend/` |
| Change shared types | `projects/utils/src/types.rs` |
| Change config fields | `projects/utils/src/config.rs` |
| Change DB schema | `projects/utils/src/db.rs` + add a migration |

---

## Key Files at a Glance

```
projects/server/src/
  main.rs               ← CLI entry, Command enum, #[tokio::main]
  context.rs            ← ProjectContext: memory loading, system prompt assembly
  session.rs            ← Session: conversation loop, tool dispatch
  mcp/
    mod.rs              ← MCP stdio server, JSON-RPC dispatch table
    handlers.rs         ← MCP tool implementations
    docs.rs             ← doc tree / search / read tools
    specs.rs            ← OpenAPI spec tools
  serve/
    mod.rs              ← axum router builder, run(), run_daemon()
    api/
      mod.rs            ← shared response helpers (err, db_json, db_ok)
      health.rs         ← GET /api/health
      mcp.rs            ← /api/mcp/* endpoints
      docs.rs           ← /api/docs/* endpoints
      ...

projects/core/src/
  backend/
    mod.rs              ← ModelBackend trait, OutputSink type, build_backend()
    claude.rs           ← ClaudeBackend (Anthropic API, streaming SSE)
    lmstudio.rs         ← LMStudioBackend (OpenAI-compat local server)

projects/agents/src/
  lib.rs                ← load_agent_prompt(), list_embedded_agents()
  build.rs              ← code-gen: bakes .md files into embedded_agents.rs
  agents/               ← wolf.md, bear.md, otter.md, ... (YAML frontmatter + prompt body)

projects/docs/src/
  lib.rs                ← list(), read(), search(), tree() over embedded docs
  dev/                  ← this directory (developer docs)
```

---

## The Binary is Self-Contained

Three separate things are compiled into the binary at build time:

1. **Agent prompts** (`orca_agents`) — `build.rs` reads every `.md` in `src/agents/`, generates a `match` arm per file using `include_str!`, writes it to `$OUT_DIR/embedded_agents.rs`.

2. **Documentation** (`orca_docs`) — `rust-embed` bakes every `.md` in `projects/docs/src/` into the binary as byte slices. That is how `orca mcp-serve` can serve `read_doc` without touching the filesystem.

3. **Frontend** (`orca` binary) — the SvelteKit build output (`projects/frontend/dist/`) is embedded with another `rust-embed` struct in `projects/server/src/serve/mod.rs`. The web server serves these static files without a separate CDN or file system path.

This design means a single `orca` binary installs everything: web UI, docs, agent prompts, and MCP server — no separate install steps for assets.
