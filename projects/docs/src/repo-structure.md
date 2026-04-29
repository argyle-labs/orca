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
  README.md             Quick-start and links to these docs
  projects/
    core/               brain-core — ModelBackend trait, LM Studio + Claude backends, tool types
    server/             brain — CLI binary, session, web server, MCP server
    agents/             brain-agents — embedded agent definitions
    utils/              brain-utils — config, types, logging, ledger, auth, state
    jobs/               brain-jobs — background job queue
    scanner/            brain-scanner — file scanner utilities
    docs/               WHY documentation, embedded in the binary
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

Shared types and infrastructure used by every other crate.

- `src/config.rs` — `Config` struct. Loads `~/brain/config/brain.toml`. All paths go through `dirs::home_dir()`. Default model is LM Studio; Claude is opt-in.
- `src/types.rs` — `Message`, `ToolCall`, `ToolResult`, `BackendResponse`, `truncate_preview`.
- `src/log.rs` — JSONL session logging to `~/brain/ai/claude/logs/sessions/`. `search_logs()` powers the MCP `brain_search_logs` tool.
- `src/ledger.rs` — Token accounting per session. Displayed by `/tokens` and `/context`.
- `src/auth.rs` — macOS Keychain integration. Stores/retrieves the Anthropic API key. Never touches disk or env files.
- `src/state.rs` — Daemon + dev supersession state (PID file, session lock, atomic writes).

### `projects/agents` — brain-agents

Embedded agent definitions. At build time, all `.md` files from the agents directory are compiled in as a fallback — a fresh install works immediately even without the brain vault.

### `projects/jobs` — brain-jobs

Background job queue for async side-effects that shouldn't block the session loop.

### `projects/scanner` — brain-scanner

File scanner utilities used by tree-building and search tools.

### `projects/server` — brain (binary)

The main crate. Produces the `brain` binary.

**CLI and session:**
- `src/main.rs` — `Cli` struct (clap), all subcommands (`Command` enum), dispatches to modules. Handles project auto-detection from cwd.
- `src/session/mod.rs` — `Session` struct. Holds config, backend, messages, tools, ledger, log.
- `src/session/chat.rs` — Chat loop. Sends messages to backend, handles tool call dispatch, drives agentic rounds (max 30).
- `src/session/commands.rs` — Interactive slash commands (`/model`, `/flag`, `/search`, `/escalate`, `/context`, `/tokens`, `/narration`).
- `src/session/delegate.rs` — `delegate` tool: spawns a sub-session for one-shot agent delegation.
- `src/session/util.rs` — `resolve_model()` (LM Studio first, Claude fallback), history file, git change check.
- `src/context.rs` — Assembles the system prompt from project memory + agent definition.
- `src/tui.rs` — ratatui split-pane TUI (default interactive mode).

**Web server:**
- `src/serve/mod.rs` — axum router. In release, serves embedded `site/dist/` via rust-embed. In dev, API only.
- `src/serve/api/` — HTTP handlers (one file per domain). Every handler has `#[utoipa::path(...)]`.
- `src/serve/openapi.rs` — Assembles the OpenAPI spec from all handler annotations.
- `src/serve/tree.rs` — `TreeNode` type and vault tree builder for brain + rebuy roots.
- `src/serve/mcp_client.rs` — HTTP → MCP stdio proxy. Spawns MCP server processes on demand, pools them.

**MCP server:**
- `src/mcp/mod.rs` — Standalone JSON-RPC 2.0 server (`brain mcp-serve`). Reads from stdin, writes to stdout.
- `src/mcp/tools.rs` — Tool definitions (JSON schemas for all MCP tools).
- `src/mcp/handlers.rs` — Handler implementations for each MCP tool.
- `src/mcp/docs.rs` — Doc tree helpers (separate from `serve/tree.rs` — runs without axum).
- `src/mcp/specs.rs` — OpenAPI spec helpers for the MCP `get_rebuy_spec` tools.
- `src/mcp/context7.rs` — Proxy for context7 MCP tool calls.

**Tests:**
- `tests/daemon_test.rs` — State serialization, read/write round-trip, pid_alive.
- `tests/tools_test.rs` — Core tool operations in temp directories.

## `projects/frontend`

```
frontend/
  package.json          Dependencies and npm scripts
  vite.config.ts        Build config: manual chunks, dev proxy to :12000
  tsconfig.json         TypeScript config (noEmit: true — Vite compiles)
  scripts/
    gen.ts              Generates src/api/ from the OpenAPI spec at :12000
    mcp-server.ts       Standalone MCP server (docs + file tree, no LLM)
  src/
    main.tsx            React root: QueryClient, RouterProvider, ThemeProvider
    routeTree.ts        Route definitions (lazy-loaded except HomePage)
    routes/             One file per page
    components/         Shared UI components
    contexts/           ThemeContext
    api/                Auto-generated — do not edit (output of gen.ts)
    schema/             Schema visualizer (XyFlow canvas, generated types)
```

All `src/api/` files are generated by `brain gen` (which calls `scripts/gen.ts`). Never edit them manually — re-run `brain gen` after any backend API change.

## `projects/docs`

All `.md` files are compiled into the binary via rust-embed. Accessible as `root="docs"` via:
- `GET /api/doc?root=docs&path=<name>` — read a doc
- `GET /api/search?q=<query>&root=docs` — search
- MCP tools: `read_doc`, `search_docs`, `get_tree`
- Brain site sidebar

Do not put personal notes here. Personal notes belong in `~/brain/notes/`. These docs ship with the binary.
