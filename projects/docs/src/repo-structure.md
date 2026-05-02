# Repository Structure

Everything in this repo. Where it lives and why it's organized this way.

## Root

```
brain/
  Cargo.toml            Workspace manifest — lists all member crates
  Cargo.lock            Pinned dependency versions
  rust-toolchain.toml   Pins the exact Rust toolchain for reproducible builds
  .nvmrc                Pins the Node.js version for the frontend
  Makefile              All developer workflows (init, build, dev, test, lint, format, audit, clean)
  README.md             Quick-start, usage, config locations, make targets
  docs/
    brain-architecture.md   Deep-dive crate map, dependency flow, design decisions, DB schema
    operational_rules.md    Dev workflow, deployment, config locations, troubleshooting
  hooks/                Claude Code event hooks (safety guards, logging)
  tests/                Integration tests (tools, daemon state)
  config/               Reference docs for agents (TOOL_RULES, DELEGATION, SEVERITY_RUBRIC, etc.)
  projects/
    core/               brain-core — ModelBackend trait, LM Studio + Claude backends, tool types
    server/             brain — CLI binary, session, web server, MCP server
    agents/             brain-agents — embedded agent definitions
    utils/              brain-utils — config, types, logging, ledger, auth, db
    jobs/               brain-jobs — background job queue
    scanner/            brain-scanner — file scanning for OpenAPI generation
    commands/           brain-commands — CLI subcommand handlers
    docs/               brain-docs — WHY documentation, embedded in the binary
    frontend/           React frontend source
```

## Workspace crates

### `projects/core` — brain-core

The LLM backend abstraction layer.

- `src/backend/mod.rs` — `ModelBackend` trait. Both backends implement `chat()`, which streams tokens to an `OutputSink` and returns a `BackendResponse`.
- `src/backend/lmstudio.rs` — LM Studio backend. Speaks the OpenAI-compatible completions API over SSE. This is the default.
- `src/backend/claude.rs` — Anthropic Messages API backend. Used for escalation only.
- `src/backend/serialize.rs` — Message serialization helpers shared by both backends.
- `src/tools/mod.rs` — `ToolRegistry`: tool definitions + execution dispatch (read, write, edit, glob, grep, bash, confirm, delegate).

### `projects/utils` — brain-utils

Shared types and infrastructure used by every other crate. No dependencies on other brain crates.

- `src/config.rs` — `Config` struct. Loads `~/brain/config/brain.toml`. All paths go through `dirs::home_dir()`. Default model is LM Studio; Claude is opt-in.
- `src/types.rs` — `Message`, `ToolCall`, `ToolResult`, `BackendResponse`, `truncate_preview`.
- `src/log.rs` — JSONL session logging to `~/.brain/logs/sessions/`. `search_logs()` powers the MCP `brain_search_logs` tool.
- `src/ledger.rs` — Token accounting per session. Displayed by `/tokens` and `/context`.
- `src/auth.rs` — macOS Keychain integration. Stores/retrieves credentials. Never touches disk or env files.
- `src/db.rs` — **brain.db** (encrypted SQLite/SQLCipher). All registry CRUD: `list_mcp_servers`, `upsert_mcp_server`, `list_schema_databases`, `upsert_docker_runtime`, `list_openapi_specs`, `upsert_openapi_spec`, etc. Opens at `~/.brain/brain.db` via `open_default()`. Key at `~/.brain/.db_key`.

### `projects/agents` — brain-agents

Embedded agent definitions. At build time, all `.md` files from the agents directory are compiled in as a fallback — a fresh install works immediately even without the brain vault.

Agent frontmatter schema:
```yaml
---
name: agent-name
description: One-line description
tools: Read, Glob, Grep, Bash, Write, Edit, Agent, WebFetch, WebSearch
model: inherit  # or "claude" or "lmstudio"
color: orange   # shown in web UI
---
```

### `projects/jobs` — brain-jobs

Background job queue for async side-effects that shouldn't block the session loop.

- `JobManager` — spawns tokio tasks, buffers output, notifies on completion, supports cancel
- `run_background_chat` — full chat+tool loop in a background task

### `projects/scanner` — brain-scanner

File scanner utilities used by `brain spec sync` to generate OpenAPI specs from source code.

- `ci4_generator.rs` — scans CodeIgniter 4 PHP routes and JSON Schema files
- `ci2_generator.rs` — scans CI2 `api.php` dispatch chains (rebuyengine)
- `nextjs_generator.rs` — scans Next.js App Router route handlers
- `graphql_parser.rs` — parses GraphQL SDL into structured `GraphQlInfo`
- `openapi_dir()` — canonical output path: `~/brain/rebuy/openapi/specs/`
- `SpecRegistry` — reads/writes `registry.json` for disk-based spec metadata

### `projects/commands` — brain-commands

Thin CLI command handlers. Cannot import the server crate. One module per subcommand group.

- `spec.rs` — `brain spec` (list/add/register/refresh/unregister/sync/dump)
- `mcp_cmd.rs` — `brain mcp` (list/add/remove/sync/map/unmap/mappings)
- `schema_cmd.rs` — `brain schema` (list/add/remove)
- `docker_cmd.rs` — `brain docker` (list/add/remove)
- `auth.rs` — `brain auth`, OAuth flows for GitHub and Atlassian
- `agents.rs` — `brain agents` (list, doctor)
- `daemon.rs` — `brain daemon` (start/stop/status, cooperative port handoff)
- `log_cmd.rs` — `brain log` (search session logs, tail)
- `codegen.rs` — `brain gen` (TypeScript codegen from `/api/openapi.json`)
- `install.rs` — `brain install` / `brain uninstall` (symlinks, MCP registration)
- `doctor.rs` — validate agents, symlinks, vault structure, wolf.md routing refs

### `projects/server` — brain (binary)

The main crate. Produces the `brain` binary.

**CLI and session (`src/`):**
- `main.rs` — `Cli` struct (clap), all subcommands (`Command` enum), dispatches to modules
- `context.rs` — Assembles the system prompt from project memory + agent definition
- `tui.rs` — crossterm split-pane TUI (default interactive mode)
- `session/mod.rs` — `Session` struct: config, backend, messages, tools, ledger, log
- `session/chat.rs` — Chat loop: sends messages to backend, handles tool call dispatch, drives agentic rounds (max 30)
- `session/commands.rs` — Interactive slash commands (`/model`, `/flag`, `/search`, `/escalate`, `/context`, `/tokens`)
- `session/delegate.rs` — `delegate` tool: spawns a sub-session for one-shot agent delegation
- `session/util.rs` — `resolve_model()` (LM Studio first, Claude fallback), history file, git change check

**Web server (`src/serve/`):**
- `mod.rs` — axum router; in release serves embedded `site/dist/` via rust-embed; in dev, API only
- `openapi.rs` — Assembles the utoipa OpenAPI spec from all handler annotations; static `SPEC` OnceLock
- `tree.rs` — `TreeNode` type and vault tree builder for brain + rebuy + docs roots
- `mcp_client.rs` — HTTP → MCP stdio proxy; spawns and pools MCP server processes; injects `DOCKER_HOST`
- `api/specs.rs` — OpenAPI spec list/get (disk + DB), register/refresh/unregister endpoints
- `api/schema_registry.rs` — Schema database CRUD (GET/POST/DELETE `/api/schema/databases`)
- `api/docker_registry.rs` — Docker runtime CRUD (GET/POST/DELETE `/api/docker/runtimes`)
- `api/mcp.rs` — MCP proxy (GET `/api/mcp/tools`, POST `/api/mcp/run`)
- `api/schema.rs` — MySQL schema visualizer (reads from DB-registered databases)
- `api/docker.rs` — Docker Compose service management
- `api/docs.rs` — Vault doc tree and search
- `api/health.rs` — Rebuy local service health checks
- `api/atlassian.rs` — Jira + Confluence via Atlassian REST API
- `api/system.rs` — Brain install status + install/uninstall actions
- `api/mod.rs` — Shared types, `err()` helper, handler prelude

**MCP server (`src/mcp/`):**
- `mod.rs` — Standalone JSON-RPC 2.0 server (`brain mcp-serve`); tool federation routing
- `tools.rs` — Tool definitions (JSON schemas for all brain-owned MCP tools)
- `handlers.rs` — Handler implementations: agents, services, registry CRUD (MCP/schema/docker)
- `specs.rs` — OpenAPI spec helpers: disk first, DB fallback; async fetch for register/refresh
- `docs.rs` — Doc tree helpers (separate from `serve/tree.rs` — runs without axum)
- `context7.rs` — Proxy for context7 MCP tool calls

**Tests:**
- `tests/daemon_test.rs` — State serialization, read/write round-trip, pid_alive
- `tests/tools_test.rs` — Core tool operations in temp directories

### `projects/docs` — brain-docs

All `.md` files in `src/` are compiled into the binary via rust-embed. Accessible as `root="docs"` via:
- `GET /api/doc?root=docs&path=<name>` — read a doc
- `GET /api/search?q=<query>&root=docs` — search
- MCP tools: `read_doc`, `search_docs`, `get_tree`
- Brain site sidebar

Files: `architecture.md`, `repo-structure.md`, `mcp-server.md`, `api.md`, `frontend.md`, `agent-model.md`, `local-first.md`, `security.md`, `stack.md`, `testing.md`, `single-binary.md`, `vault-structure.md`, `learn/` (4 guides).

Do not put personal notes here. Personal notes belong in `~/brain/notes/`. These docs ship with the binary.

## `projects/frontend`

```
frontend/
  package.json          Dependencies and npm scripts
  vite.config.ts        Build config: manual chunks, dev proxy /api/ → :12000
  tsconfig.json         TypeScript config (noEmit: true — Vite compiles)
  scripts/
    gen.ts              Generates src/api/ from the OpenAPI spec at :12000/openapi.json
  src/
    main.tsx            React root: QueryClient, RouterProvider, ThemeProvider
    routeTree.ts        Route definitions (lazy-loaded except HomePage)
    routes/             One file per page
    components/         Shared UI components
    contexts/           ThemeContext (dark/light + palette)
    api/                Auto-generated — do not edit (output of gen.ts)
    schema/             Schema visualizer (XyFlow canvas, generated types)
```

All `src/api/` files are generated by `brain gen` (which calls `scripts/gen.ts`). Never edit them manually — re-run `brain gen` after any backend API change.

## `config/`

Reference documents for agents. Not code. Loaded by the `brain_get_config` MCP tool.

| File | Purpose |
|------|---------|
| `TOOL_RULES.md` | Guardrails for all agents: read/write/edit/bash disciplines, modification policy |
| `DELEGATION.md` | Task → agent routing table (40+ specialists) |
| `CANONICAL_SOURCES.md` | Where to find authoritative types/schemas per project |
| `CODING_RULES.md` | Pre-action and post-change rules for coding agents |
| `SEVERITY_RUBRIC.md` | Bug/review severity levels with examples and reviewer rules |

## `hooks/`

Claude Code event hooks. See [operational_rules.md](../../docs/operational_rules.md#hooks).

| File | Trigger | Purpose |
|------|---------|---------|
| `bash-destructive-guard.sh` | Pre-Bash | Blocks destructive shell commands |
| `opnsense-guard.sh` | Pre-Bash | Blocks network appliance commands |
| `pii-scanner.sh` | Pre-Write | Warns on PII in file writes |
| `pre-commit-secrets-scan.sh` | Pre-Bash | Scans staged files for secrets |
| `glob-cache-check.py` | Pre-Glob | Validates glob patterns |
| `glob-cache-write.py` | Post-Glob | Caches glob results |
| `session-start.sh` | Session start | Logs session start |
| `session-stop.sh` | Session stop | Logs session stop |
