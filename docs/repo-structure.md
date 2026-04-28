# Repository Structure

Everything in this repo. Where it lives and why it's organized this way.

## Root

```
brain/
  Cargo.toml          Rust dependencies and build profiles
  Cargo.lock          Pinned dependency versions (gitignored — this is a binary, not a library)
  rust-toolchain.toml Pins the exact Rust toolchain version for reproducible builds
  .nvmrc              Pins the Node.js version for the frontend
  Makefile            All developer workflows (init, build, dev, test, lint, format, audit, clean)
  build.rs            Compile-time: embeds agent .md files as a fallback if ~/brain is missing
  README.md           Project overview and quick-start
  docs/               WHY documentation, embedded in the binary (accessible via API + MCP)
  site/               React frontend source
  src/                Rust source
  tests/              Integration tests
```

## src/

### Entry point

**`src/main.rs`** — CLI entry point. Defines the `Cli` struct (clap), all subcommands (`Command` enum), and dispatches to the appropriate module. Handles project auto-detection from the current working directory.

### Core session

**`src/session.rs`** — The chat loop. Sends messages to the active backend, receives streaming token responses, handles tool call dispatch, manages the message history buffer, and drives the REPL in `--classic` mode. This is the most central file in the codebase.

**`src/context.rs`** — Assembles the system prompt. Loads project memory from `~/brain/ai/claude/memory/<project>/` and prepends it to the active agent's system prompt. Called at session start and on project switch.

**`src/agents.rs`** — Agent registry. Reads `.md` files from `~/brain/ai/claude/agents/`. Falls back to build-time embedded agents (from `build.rs`) if the directory is missing. Exposes `load_agent(name)` to retrieve an agent's system prompt.

**`src/config.rs`** — Config loading. Reads `~/brain/config/brain.toml`. Resolves all path constants (vault root, agents dir, logs dir, memory root, specs dir). No hardcoded paths — everything goes through `dirs::home_dir()`.

### Backends

**`src/backend/mod.rs`** — `ModelBackend` trait with a single method: `stream_response`. Both backends implement this.

**`src/backend/lmstudio.rs`** — LM Studio backend. Speaks the OpenAI-compatible chat completions API. Handles streaming via Server-Sent Events. This is the default backend.

**`src/backend/claude.rs`** — Anthropic Messages API backend. Used for escalation only. Handles streaming via the Anthropic SSE format (different from OpenAI's).

### Tools

**`src/tools/mod.rs`** — Tool registry. Defines all tool schemas (the JSON that gets sent to the LLM) and routes `tool_call` events to the correct implementation.

**`src/tools/bash.rs`** — Shell execution. Runs commands via `Command::new("bash").arg("-c")`. In interactive mode, prompts the user before executing. Has a configurable timeout. Never uses shell interpolation on user-supplied strings.

**`src/tools/fs.rs`** — File tools: `read_file`, `write_file`, `edit_file` (exact string replacement). All paths are resolved through the config to prevent accidental writes outside expected directories.

**`src/tools/search.rs`** — `glob` (file pattern matching) and `grep` (full-text search with regex). Uses the `glob` and `regex` crates directly.

### Infrastructure

**`src/types.rs`** — Shared data types: `Message`, `ToolCall`, `ToolResult`, `BackendResponse`. Used across backends, session, and tools.

**`src/log.rs`** — Session logging. Writes JSONL to `~/brain/ai/claude/logs/sessions/YYYY-MM-DD_HHMMSS_<project>.jsonl`. Provides `search_logs(dir, query, limit)` for the MCP `brain_search_logs` tool and the `/search` interactive command.

**`src/ledger.rs`** — Token accounting. Tracks prompt/completion tokens per message, per session, and cumulative. Displayed via `/tokens` and `/context`.

**`src/auth.rs`** — macOS Keychain integration. Stores and retrieves the Anthropic API key via the `security` CLI. The key is never written to disk or environment files.

**`src/docs.rs`** — Embedded documentation. `rust-embed` compiles `docs/` into the binary. Exposes `read()`, `search()`, `tree()`, and `file_count()` — all serving from compiled-in bytes, no filesystem access.

**`src/jobs.rs`** — Background job queue. Async side-effects that shouldn't block the session loop.

**`src/tui.rs`** — Split-pane terminal UI (ratatui). The default interactive mode. Left pane: conversation. Right pane: context/tools. `--classic` flag bypasses this and uses rustyline.

**`src/scanner/`** — File scanner utilities used by tree-building and search tools.

### Web server

**`src/serve/mod.rs`** — axum router assembly. In release mode, serves static assets from the `Assets` rust-embed struct (compiled from `site/dist/`). In dev mode, serves API routes only (Vite handles the frontend). The `static_handler` fallback serves `index.html` for all unmatched paths (SPA routing).

**`src/serve/api.rs`** — All HTTP handler functions. Each handler is annotated with `#[utoipa::path(...)]` for OpenAPI spec generation. Handlers are grouped by tag: `docs`, `docker`, `mcp`, `schema`, `logs`.

**`src/serve/tree.rs`** — Filesystem tree builder for the `brain` and `rebuy` vault roots. `build_tree_raw` walks directories, `compact_tree` collapses single-file dirs and single-child dirs for a cleaner tree. `get_ignored` and `get_search_ignored` define what's excluded from the nav tree vs full-text search.

**`src/serve/mcp_client.rs`** — HTTP → MCP stdio proxy. Spawns MCP server processes on demand and keeps them alive in a pool. The `/api/mcp/tools` and `/api/mcp/run` endpoints use this to proxy calls to external MCP servers configured in `brain.toml`.

**`src/serve/openapi.rs`** — Assembles the utoipa OpenAPI spec from all handler annotations. Served at `/api/openapi.json` and `/api/openapi/public.json`.

### MCP server

**`src/mcp.rs`** — Standalone MCP stdio server (`brain mcp-serve`). Reads JSON-RPC 2.0 from stdin, dispatches to tool implementations, writes responses to stdout. Has its own doc tree helpers (separate from `serve/tree.rs`) because it runs as a subprocess without axum. The `root="docs"` case in all doc tools reads from `crate::docs` (embedded content).

## tests/

**`tests/tools_test.rs`** — Integration tests for the core tool operations (read, write, edit, glob, grep). Uses real filesystem operations in temp directories. Also tests model string parsing and frontmatter stripping.

## site/

```
site/
  package.json        Dependencies and npm scripts
  vite.config.ts      Build config: manual chunks, dev proxy
  tsconfig.json       TypeScript config (noEmit: true — Vite handles compilation)
  index.html          HTML entry point
  scripts/
    gen.ts            Generates src/api/ from the OpenAPI spec
  src/
    main.tsx          React root: QueryClient, RouterProvider, ThemeProvider
    routeTree.ts      Route definitions (lazy-loaded except HomePage)
    index.css         Global styles and CSS custom properties
    routes/           One file per page
    components/       Shared UI components
    contexts/         React context providers (ThemeContext)
    hooks/            Custom React hooks
    api/              Generated — do not edit (output of gen.ts)
    schema/           Generated — do not edit (output of gen.ts)
```

### Why `noEmit: true` in tsconfig

TypeScript is used for type checking only. Vite/Rolldown handles the actual compilation and bundling. Emitting `.js` files from `tsc` would be redundant and slower. The `tsc -b` in the build script runs type checking to catch errors before Vite bundles.

### Why manual chunks in vite.config.ts

Vendor libraries (React, Mantine, TanStack, XyFlow, Scalar) are split into stable named chunks. A change to app code doesn't bust the browser's cached copy of Mantine. Combined with lazy-loaded routes, the initial load is only `vendor-react` + `vendor-mantine` + the homepage code.

## docs/

All files in `docs/` are compiled into the binary via `src/docs.rs`. They're accessible as `root="docs"` in:
- `GET /api/tree` — navigation tree
- `GET /api/doc?root=docs&path=<name>` — read a doc
- `GET /api/search?q=<query>&root=docs` — full-text search
- MCP tools: `get_tree`, `read_doc`, `search_docs`
- The brain site sidebar

**Do not put personal notes here.** Personal notes belong in `~/brain/notes/`. These docs are for the project — they ship with the binary and are visible to all users.

## build.rs

Reads all `.md` files from the agents directory at compile time and generates a `AGENTS` constant. This is the fallback used by `src/agents.rs` when `~/brain/ai/claude/agents/` doesn't exist. It means a fresh `make install` on a new machine is immediately usable without setting up the vault.
