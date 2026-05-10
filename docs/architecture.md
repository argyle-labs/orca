# Architecture

brain is a single Rust binary that serves four roles simultaneously:

1. **CLI** — interactive REPL for local AI sessions (`brain`, `brain <project>`)
2. **TUI** — split-pane terminal UI via crossterm (default mode; `--classic` for readline)
3. **Web server** — React site + REST API on `:12000` (`brain serve`)
4. **MCP server** — JSON-RPC 2.0 over stdio for Claude Code integration (`brain mcp-serve`)

## Why one binary

Deployment is `cp brain ~/.local/bin/brain`. No Docker, no node runtime at the install target, no separate web server process. The React site and all documentation are compiled into the binary at build time via `rust-embed`. See [single-binary.md](single-binary.md).

## Crate map

The repo is a Cargo workspace. Each crate has a single responsibility.

```
projects/
  core/          brain-core    — ModelBackend trait, LM Studio + Claude backends, tool types
  utils/         brain-utils   — Config, types, logging, ledger, auth, db (brain.db CRUD)
  agents/        brain-agents  — embedded agent definitions (build-time fallback)
  jobs/          brain-jobs    — background job queue
  scanner/       brain-scanner — source file scanning for OpenAPI generation
  commands/      brain-commands— CLI subcommand handlers (one module per command)
  docs/          brain-docs    — embedded WHY documentation (this file)
  server/        brain (binary)— CLI, session, web server, MCP server
  frontend/      React site (compiled into server binary via rust-embed)
```

### Key files in `projects/server/src/`

```
main.rs               CLI entry (clap), Command enum dispatch, project auto-detection
context.rs            Assembles system prompt from project memory + agent definition
tui.rs                crossterm split-pane TUI, keybindings

session/
  mod.rs              Session struct (config, backend, messages, tools, ledger, log)
  chat.rs             Chat loop — sends messages, handles tool calls, agentic rounds (max 30)
  commands.rs         Slash commands (/model, /flag, /search, /escalate, /context, /tokens)
  delegate.rs         delegate tool — sub-session for one-shot agent calls
  util.rs             resolve_model() (LM Studio → Claude fallback), history, git check

mcp/
  mod.rs              JSON-RPC 2.0 dispatcher (stdin → stdout); tool federation routing
  tools.rs            Tool definitions (JSON schemas for all brain-owned MCP tools)
  handlers.rs         Tool implementations (agents, services, registry CRUD)
  specs.rs            OpenAPI spec helpers — disk first, DB fallback
  docs.rs             Doc tree helpers (runs without axum)
  context7.rs         Context7 MCP proxy

serve/
  mod.rs              axum router; rust-embed serves site/dist in release
  openapi.rs          utoipa spec assembly; static SPEC OnceLock
  tree.rs             TreeNode type, vault tree builder (brain + rebuy + docs roots)
  mcp_client.rs       Spawns MCP server subprocesses, pools them, injects DOCKER_HOST
  api/                HTTP handlers — one file per domain, all utoipa-annotated
    specs.rs          External API spec registry (list/get/register/refresh/unregister)
    mcp.rs            MCP server proxy (tools list, tool run)
    schema.rs         MySQL schema visualizer
    schema_registry.rs  Schema DB CRUD (list/add/remove)
    docker_registry.rs  Docker runtime CRUD (list/add/remove)
    docker.rs         Docker Compose service management
    docs.rs           Vault doc tree + search
    logs.rs           Docker service log fetching
    health.rs         Rebuy local service health checks
    atlassian.rs      Jira + Confluence via Atlassian REST API
    bitbucket.rs      Bitbucket repo and PR listing
    system.rs         Brain install status + install/uninstall actions
    tests_handler.rs  Test suite runner
    ctx7.rs           Context7 documentation proxy
    learning.rs       Learning progress tracking
    download.rs       Spec download helpers
    pdf.rs            PDF rendering
```

## Request flow (web)

```
browser → axum router → handler → filesystem / MCP client / docker CLI / brain.db
                      ↓ (root="docs")
                      rust-embed (compiled-in docs/)
```

## Request flow (MCP — brain's own tools)

```
Claude Code → stdin → mcp/mod.rs dispatcher → tool implementation
                                             ↓ (brain_run)
                                             session.rs → lmstudio / claude backend
```

## Request flow (MCP — federated tools)

```
Claude Code → stdin → mcp/mod.rs dispatcher
                    ↓ (tool not in brain's own tools)
                    tool_registry: HashMap<tool_name, server_name>
                    ↓
                    mcp_client.rs: McpPool::get_or_connect(server_name)
                    ↓
                    registered MCP server subprocess (from brain.db)
```

## The three-surface pattern

Every registry (MCP servers, schema DBs, Docker runtimes, OpenAPI specs) has four surfaces:

| Surface | Location |
|---------|----------|
| DB CRUD | `brain-utils/src/db.rs` — `list_*`, `upsert_*`, `remove_*` functions |
| CLI | `brain-commands/src/*_cmd.rs` — clap subcommand enum + handler |
| REST API | `projects/server/src/serve/api/*_registry.rs` — GET/POST/DELETE handlers |
| MCP tools | `projects/server/src/mcp/tools.rs` + `handlers.rs` + `mod.rs` dispatch |

All four surfaces must be kept in sync when adding a new registry feature.

## Configuration

Runtime config splits across two locations:
- `~/brain/config/brain.toml` — static settings (LLM endpoints, API keys). Not in this repo.
- `~/.brain/brain.db` — dynamic registries (MCP servers, schema DBs, Docker runtimes, OpenAPI specs). Managed via CLI.

The binary reads `brain.toml` at startup. `brain.db` is opened on demand via `brain_utils::db::open_default()`.
