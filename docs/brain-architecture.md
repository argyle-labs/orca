# Brain Codebase Architecture

Source: `~/code/brain` — Rust Cargo workspace.

## Crate Map

```
brain (server binary)
├── brain-core        — model backends + tool execution
├── brain-utils       — shared types, config, auth, ledger, log, fs tools
├── brain-jobs        — background agent tasks
├── brain-commands    — CLI subcommand handlers
└── brain-agents      — embedded agent prompt registry
```

## Dependency Flow

```
brain (binary)
  → brain-commands, brain-core, brain-jobs, brain-utils, brain-agents
brain-commands
  → brain-core, brain-utils, brain-agents, brain-scanner
brain-core
  → brain-utils
brain-jobs
  → brain-core, brain-utils
brain-agents
  → (standalone — embeds .md files at compile time)
brain-utils
  → (no internal deps — leaf crate)
```

No circular dependencies. Utils is the leaf; server is the root.

## Crate Purposes

### `brain` (projects/server)
The binary entry point and interactive session layer.

- `main.rs` — CLI parsing (clap), dispatch to commands or interactive session
- `session.rs` — REPL + TUI loop, chat rounds, tool execution, job management, command dispatch
- `mcp.rs` — MCP stdio server (JSON-RPC 2.0) exposing brain tools to Claude Code
- `serve/` — Axum HTTP server: REST API, OpenAPI spec, Vite dev proxy, static serving
  - `serve/api/` — route handlers: health, logs, docker, docs, specs, schema, context7
  - `serve/tree.rs` — `TreeNode` struct + `build_tree_raw` (canonical doc-tree builder)
  - `serve/middleware.rs` — request logging, correlation IDs, body capture at TRACE
- `context.rs` — `ProjectContext`: resolves project memory, builds system prompt
- `tui.rs` — crossterm split-pane UI (input pane + output pane), keybindings

### `brain-core` (projects/core)
Model backend abstraction and tool dispatch.

- `backend/mod.rs` — `ModelBackend` trait, `build_backend` factory, `OutputSink` type
- `backend/claude.rs` — Anthropic API client with SSE streaming parser
- `backend/lmstudio.rs` — LM Studio (OpenAI-compatible) client
- `tools/mod.rs` — `ToolRegistry`: tool definitions, dispatch, calls into brain-utils
- `tools/bash.rs` — bash execution with async spawn_blocking, permission prompt, timeout

### `brain-utils` (projects/utils)
Shared primitives. No dependencies on other brain crates.

- `config.rs` — `Config` (brain.toml), `Model` enum, backend/API key fields
- `types.rs` — `Message`, `ToolCall`, `ToolResult`, `ToolDef`, `truncate_preview`
- `auth.rs` — OS keychain read/write via `keyring` crate
- `ledger.rs` — `TokenLedger` + `fmt_tokens` (session token accounting)
- `log.rs` — `SessionLog` JSONL writer with tombstone-based flagging; search/recall helpers
- `tools/fs.rs` — read_file, write_file, edit_file (exact string replace)
- `tools/search.rs` — glob_files (globwalk), grep_content (recursive ripgrep-style)

### `brain-jobs` (projects/jobs)
Background agent execution, decoupled from the foreground session.

- `JobManager` — spawns tokio tasks, buffers output, notifies on completion, supports cancel
- `run_background_chat` — full chat+tool loop in a background task

### `brain-commands` (projects/commands)
Thin CLI command handlers. Cannot import the server crate.

- `auth.rs` — login/logout/auth-status
- `agents.rs` — list agents, install embedded agents
- `spec.rs` — OpenAPI spec registry (~/brain/openapi/)
- `log_cmd.rs` — session log subcommands
- `doctor.rs` — validate agents, symlinks, tools
- `mcp_cmd.rs` — MCP registry management
- `codegen.rs` — `brain gen` TypeScript codegen
- `projects.rs` — list memory projects

### `brain-agents` (projects/agents)
Embeds agent `.md` files at compile time via `build.rs`. Exposes `load_agent_prompt(name)`.

## Key Design Decisions

**No re-exports** — all imports are direct (e.g., `use brain_utils::types::Message`, not re-exported from brain-core). This keeps the dependency graph readable.

**Output sink** — `OutputSink = Arc<Mutex<dyn Write + Send>>`. All model output flows through this abstraction. In the CLI it writes to stdout; in background jobs it writes to a Vec buffer.

**Session owns everything** — Session holds the backend, tool registry, job manager, ledger, and log. It is not shared across threads; background jobs use their own independent backend + registry.

**MCP doc roots** — The MCP `get_tree`/`read_doc`/`search_docs` tools serve two roots: `brain` (this vault at ~/brain) and `rebuy` (~/code/rebuy). The `docs` root serves embedded binary docs from brain-docs.

**Cancellation** — All chat calls accept a `CancellationToken`. A ctrl-c handler task is spawned once per chat/escalate/delegate call and aborted after the call completes.

## Logging

Set `BRAIN_LOG` env var to control verbosity:
- Default: `warn,brain=info` — quiet external crates, brain info+
- Debug mode: `BRAIN_LOG=debug brain serve`
- Trace (request bodies): `BRAIN_LOG=trace brain serve`

Tool result display in the session suppresses file listing noise (glob/grep show counts, not paths). Errors always show content.
