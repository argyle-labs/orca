# Brain Codebase Architecture

Source: `~/code/brain` — Rust Cargo workspace. See also the embedded doc at [`projects/docs/src/architecture.md`](../projects/docs/src/architecture.md).

## Crate Map

```
brain (server binary)
├── brain-core        — model backends + tool execution
├── brain-utils       — shared types, config, auth, ledger, log, db
├── brain-jobs        — background agent tasks
├── brain-commands    — CLI subcommand handlers
├── brain-agents      — embedded agent prompt registry
├── brain-scanner     — file/code scanning for OpenAPI generation
└── brain-docs        — embedded WHY documentation (compiled into binary)
```

## Dependency Flow

```
brain (binary)
  → brain-commands, brain-core, brain-jobs, brain-utils, brain-agents, brain-scanner, brain-docs
brain-commands
  → brain-core, brain-utils, brain-agents, brain-scanner
brain-core
  → brain-utils
brain-jobs
  → brain-core, brain-utils
brain-agents
  → (standalone — embeds .md files at compile time)
brain-scanner
  → brain-utils
brain-docs
  → (standalone — embeds .md files at compile time)
brain-utils
  → (no internal deps — leaf crate)
```

No circular dependencies. Utils is the leaf; server is the root.

## Crate Purposes

### `brain` (projects/server)

The binary entry point. Four simultaneous roles: CLI, TUI, web server, MCP server.

- `main.rs` — CLI parsing (clap), `Command` enum dispatch
- `session/` — REPL + TUI loop, chat rounds, tool execution, job management, slash commands
- `mcp/` — MCP stdio server (JSON-RPC 2.0)
  - `mod.rs` — dispatcher (stdin → stdout), tool federation routing
  - `tools.rs` — JSON schemas for all brain-owned MCP tools
  - `handlers.rs` — implementations for agent/service/registry tools
  - `specs.rs` — OpenAPI spec helpers (disk + DB fallback)
  - `docs.rs` — doc tree helpers (axum-free)
  - `context7.rs` — Context7 MCP proxy
- `serve/` — Axum HTTP server
  - `openapi.rs` — utoipa spec assembly, static `SPEC` OnceLock
  - `tree.rs` — `TreeNode` struct + vault tree builder
  - `mcp_client.rs` — spawns MCP subprocess pool, injects `DOCKER_HOST`
  - `api/` — handlers, one file per domain (see [API docs](../projects/docs/src/api.md))
- `context.rs` — `ProjectContext`: resolves project memory, builds system prompt
- `tui.rs` — crossterm split-pane UI, keybindings

### `brain-core` (projects/core)

Model backend abstraction and tool dispatch.

- `backend/mod.rs` — `ModelBackend` trait, `build_backend` factory, `OutputSink` type
- `backend/claude.rs` — Anthropic API client with SSE streaming parser
- `backend/lmstudio.rs` — LM Studio (OpenAI-compatible) client, default backend
- `tools/mod.rs` — `ToolRegistry`: tool definitions + dispatch (read, write, edit, glob, grep, bash)
- `tools/bash.rs` — async bash execution, permission prompt, 30s timeout

### `brain-utils` (projects/utils)

Shared primitives. No dependencies on other brain crates. The leaf of the dependency tree.

- `config.rs` — `Config` (brain.toml), `Model` enum, all path resolution
- `types.rs` — `Message`, `ToolCall`, `ToolResult`, `ToolDef`, `truncate_preview`
- `auth.rs` — OS keychain read/write via `keyring` crate (never touches disk)
- `ledger.rs` — `TokenLedger` + `fmt_tokens` (session token accounting)
- `log.rs` — `SessionLog` JSONL writer; `search_logs()` powers MCP tool
- `db.rs` — **brain.db** (encrypted SQLite/SQLCipher): MCP server registry, schema DB registry, Docker runtime registry, OpenAPI spec registry. `open_default()` → `~/.brain/brain.db`. All CRUD lives here.
- `tools/fs.rs` — read_file, write_file, edit_file (exact string replace)
- `tools/search.rs` — glob_files (globwalk), grep_content (recursive ripgrep-style)

### `brain-jobs` (projects/jobs)

Background agent execution, decoupled from the foreground session.

- `JobManager` — spawns tokio tasks, buffers output, notifies on completion, supports cancel
- `run_background_chat` — full chat+tool loop in a background task

### `brain-commands` (projects/commands)

Thin CLI command handlers. Cannot import the server crate. One module per subcommand.

- `spec.rs` — `brain spec` (list/add/register/refresh/unregister/sync/dump)
- `mcp_cmd.rs` — `brain mcp` (list/add/remove/sync/map/mappings)
- `schema_cmd.rs` — `brain schema` (list/add/remove)
- `docker_cmd.rs` — `brain docker` (list/add/remove)
- `auth.rs` — `brain auth` / OAuth flows (GitHub, Atlassian)
- `agents.rs` — `brain agents` (list, doctor)
- `log_cmd.rs` — `brain log` (search, tail)
- `daemon.rs` — `brain daemon` (start/stop/status)
- `codegen.rs` — `brain gen` (TypeScript codegen from OpenAPI spec)
- `install.rs` — `brain install` / `brain uninstall`
- `doctor.rs` — validate agents, symlinks, tools, stale refs

### `brain-agents` (projects/agents)

Embeds agent `.md` files at compile time via `build.rs`. Exposes `load_agent_prompt(name)`.

- Agent files: `projects/agents/src/agents/` (39+ agents)
- Frontmatter: `name`, `description`, `tools`, `model`, `color`
- Loading: filesystem first (hot-reload in dev), embedded fallback

### `brain-scanner` (projects/scanner)

File scanning utilities for OpenAPI spec generation from source code.

- `ci4_generator.rs` — scans CodeIgniter 4 PHP routes + schemas
- `ci2_generator.rs` — scans CI2 `api.php` dispatch chains
- `nextjs_generator.rs` — scans Next.js App Router route handlers
- `graphql_parser.rs` — parses GraphQL SDL into structured `GraphQlInfo`
- `openapi_dir()` — canonical path: `~/brain/rebuy/openapi/specs/`
- `SpecRegistry` — reads/writes `registry.json` for disk-based spec metadata

### `brain-docs` (projects/docs)

Embeds all `.md` files from `src/` at compile time. Always accessible as `root="docs"` via REST API and MCP tools, regardless of filesystem.

---

## The Three-Surface Pattern

Every registry feature (MCP servers, schema DBs, Docker runtimes, OpenAPI specs) follows the same pattern:

```
1. DB CRUD (brain-utils/src/db.rs)
   └── Table schema + Row struct
   └── list_*, upsert_*, remove_* functions using open_default()

2. CLI (brain-commands/src/*_cmd.rs)
   └── Clap subcommand enum (List/Add/Remove/...)
   └── cmd_*() function calling db::open_default()

3. REST API (projects/server/src/serve/api/*_registry.rs)
   └── GET /api/<resource>         → list handler
   └── POST /api/<resource>        → add handler (JSON body)
   └── DELETE /api/<resource>/{name} → remove handler

4. MCP tools (projects/server/src/mcp/)
   └── tools.rs     — JSON schema for brain_*_list/add/remove
   └── handlers.rs  — implementation functions
   └── mod.rs       — dispatch match arms

5. Route registration (projects/server/src/serve/openapi.rs)
   └── .routes(routes!(api::handler_fn))
   └── schema registration in #[openapi(components(schemas(...)))]
```

**Adding a new registry:** implement all five surfaces in order. A registry that exists in the DB but has no CLI, REST API, or MCP tool surface is incomplete.

---

## Key Design Decisions

**No re-exports** — all imports are direct (e.g., `use brain_utils::types::Message`). Keeps the dependency graph readable.

**Output sink** — `OutputSink = Arc<Mutex<dyn Write + Send>>`. All model output flows through this abstraction. In CLI mode it writes to stdout; in background jobs it writes to a Vec buffer.

**Session owns everything** — Session holds the backend, tool registry, job manager, ledger, and log. Not shared across threads; background jobs use their own independent backend + registry.

**brain.db over brain.toml for runtime config** — Static config (LLM endpoints, API keys) stays in `brain.toml`. Dynamic registries (MCP servers, schema DBs, Docker runtimes, OpenAPI specs) live in `brain.db` (encrypted SQLite). This makes CRUD safe and atomic. See [`docs/project_config_db_migration.md`](../projects/agents/) for what has been migrated.

**MCP doc roots** — The MCP `get_tree`/`read_doc`/`search_docs` tools serve three roots: `brain` (vault), `rebuy` (`~/code/rebuy/`), and `docs` (embedded binary docs).

**MCP federation** — `brain mcp-serve` exposes brain's own tools AND forwards unknown tool calls to registered MCP servers (from brain.db). The tool registry merges brain tools with federated tools on `tools/list`. `FEDERATION_SKIP` prevents proxying `brain-local` and `context7` back.

**Cancellation** — All chat calls accept a `CancellationToken`. A ctrl-c handler task is spawned once per call and aborted after completion.

**Frontend is a thin client** — All business logic lives server-side. The frontend uses only generated API hooks (`src/api/hooks.ts`). No raw `fetch()`, no local parsing. Run `brain gen` after any API change.

---

## Logging

Set `BRAIN_LOG` env var to control verbosity:
- Default: `warn,brain=info` — quiet external crates, brain info+
- Debug mode: `BRAIN_LOG=debug brain serve`
- Trace (request bodies): `BRAIN_LOG=trace brain serve`

Tool result display in the session suppresses file listing noise (glob/grep show counts, not paths). Errors always show content.

---

## Database Schema (brain.db)

All tables are created idempotently on startup via `Config::load()` → `db::open_default()`.

| Table | Purpose | Key columns |
|-------|---------|-------------|
| `mcp_servers` | Registered MCP server definitions | name PK, command, args (JSON), env (JSON) |
| `mcp_tool_mappings` | brain-tool → mcp-tool routing | brain_tool PK, mcp_name, external_tool |
| `schema_databases` | MySQL/MariaDB connections | name PK, host, port, user, password, container |
| `docker_runtimes` | Docker socket/host/URL configs | name PK, socket_path, host, url, enabled |
| `openapi_specs` | URL-fetched OpenAPI JSON | name PK, url, spec_json, cached_at |
| `sessions` | Session chat history | id, project, messages (JSON) |
| `learning_progress` | Learning curriculum state | repo, step, completed_at |
