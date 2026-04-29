# Architecture

brain is a single Rust binary that serves four roles simultaneously:

1. **CLI** — interactive REPL for local AI sessions (`brain`, `brain <project>`)
2. **TUI** — split-pane terminal UI via ratatui (default mode; `--classic` for readline)
3. **Web server** — React site + REST API on `:12000` (`brain serve`)
4. **MCP server** — JSON-RPC 2.0 over stdio for Claude Code integration (`brain mcp-serve`)

## Why one binary

Deployment is `cp brain ~/.local/bin/brain`. No Docker, no node runtime at the install target, no separate web server process. The React site is compiled into the binary at build time via `rust-embed`. See [single-binary.md](single-binary.md).

## Crate map

The repo is a Cargo workspace. Each crate has a single responsibility.

```
projects/
  core/          brain-core — ModelBackend trait, LM Studio + Claude backends, tool types
  utils/         brain-utils — Config, types, logging, ledger, auth, state
  agents/        brain-agents — embedded agent definitions (build-time fallback)
  jobs/          brain-jobs — background job queue
  scanner/       brain-scanner — file scanner utilities
  server/        brain (binary) — CLI, session, web server, MCP server
  frontend/      React site (compiled into server binary via rust-embed)
  docs/          WHY documentation (compiled into server binary via rust-embed)
```

### Key files in `projects/server/src/`

```
main.rs               CLI entry (clap), subcommand dispatch, project auto-detection
context.rs            Assembles system prompt from project memory + agent definition
tui.rs                ratatui split-pane TUI
session/
  mod.rs              Session struct (config, backend, messages, tools, ledger, log)
  chat.rs             Chat loop — sends messages, handles tool calls, agentic rounds
  commands.rs         Slash commands (/model, /flag, /search, /escalate, /context…)
  delegate.rs         delegate tool — sub-session for one-shot agent calls
  util.rs             resolve_model() (LM Studio → Claude fallback), history, git check
mcp/
  mod.rs              JSON-RPC 2.0 dispatcher (stdin → stdout)
  tools.rs            Tool definitions (JSON schemas)
  handlers.rs         Tool implementations
  docs.rs             Doc tree helpers (runs without axum)
  specs.rs            OpenAPI spec helpers
  context7.rs         Context7 MCP proxy
serve/
  mod.rs              axum router; rust-embed serves site/dist in release
  api/                HTTP handlers (one file per domain, all utoipa-annotated)
  openapi.rs          utoipa spec assembly
  tree.rs             TreeNode type, vault tree builder
  mcp_client.rs       Spawns MCP server subprocesses, pools them
```

## Request flow (web)

```
browser → axum router → handler → filesystem / MCP client / docker CLI
                      ↓ (root="docs")
                      rust-embed (compiled-in docs/)
```

## Request flow (MCP)

```
Claude Code → stdin → mcp.rs dispatcher → tool implementation
                                         ↓ (brain_run)
                                         session.rs → lmstudio / claude backend
```

## Configuration

All runtime config lives in `~/brain/config/brain.toml` (the vault, not this repo). The binary reads it at startup. There is no config bundled into the binary — this keeps developer configuration out of the shipped artifact.
